mod event;

use super::RpcReply;
use event::WireResult;
use futures_util::{SinkExt, StreamExt, stream};
use minter_api::pb;
use serde_json::json;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{Semaphore, mpsc, watch},
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tonic::{Status, codegen::BoxStream};

const MAX_SUBSCRIPTIONS: usize = 100;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SUBSCRIPTION_LIFETIME: Duration = Duration::from_secs(60);
// Matches the RPC budget for a block with ABCI results and base64 overhead.
const MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) struct Subscriptions {
    endpoint: String,
    capacity: Arc<Semaphore>,
    stopped: watch::Sender<bool>,
}

impl Subscriptions {
    pub(super) fn new(address: SocketAddr) -> Self {
        Self {
            endpoint: format!("ws://{address}/websocket"),
            capacity: Arc::new(Semaphore::new(MAX_SUBSCRIPTIONS)),
            stopped: watch::channel(false).0,
        }
    }

    pub(super) fn shutdown(&self) {
        self.stopped.send_replace(true);
    }

    pub(super) async fn open(
        &self,
        query: String,
    ) -> Result<BoxStream<pb::SubscribeResponse>, Status> {
        let permit = self.capacity.clone().try_acquire_owned().map_err(|_| {
            Status::resource_exhausted(format!(
                "max_subscription_clients {MAX_SUBSCRIPTIONS} reached"
            ))
        })?;
        let mut stopped = self.stopped.subscribe();
        let (mut socket, mut first) = tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => return Err(shutting_down()),
            connection = timeout(CONNECT_TIMEOUT, connect(&self.endpoint, &query)) => {
                connection.map_err(|_| Status::deadline_exceeded("Tendermint subscription setup timed out"))??
            }
        };
        let (send, receive) = mpsc::channel(1);
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = stopped.wait_for(|stopped| *stopped) => Err(shutting_down()),
                _ = sleep(SUBSCRIPTION_LIFETIME) => Err(Status::deadline_exceeded("context deadline exceeded")),
                _ = send.closed() => Ok(()),
                result = async {
                    while let Ok(slot) = send.reserve().await {
                        slot.send(Ok(next_event(&mut socket, &query, &mut first).await?));
                    }
                    Ok(())
                } => result,
            };
            // Tendermint's OnDisconnect releases the subscription even if
            // the downstream transport stopped reading.
            drop((socket, permit));
            if let Err(error) = result {
                let _ = timeout(CONNECT_TIMEOUT, send.send(Err(error))).await;
            }
        });
        Ok(Box::pin(stream::unfold(receive, |mut receive| async {
            let item = receive.recv().await?;
            Some((item, receive))
        })))
    }
}

async fn connect(
    endpoint: &str,
    query: &str,
) -> Result<(Socket, Option<pb::SubscribeResponse>), Status> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_EVENT_BYTES))
        .max_frame_size(Some(MAX_EVENT_BYTES));
    let (mut socket, _) = connect_async_with_config(endpoint, Some(config), true)
        .await
        .map_err(transport_error)?;
    socket
        .send(Message::Text(
            json!({"jsonrpc":"2.0", "id":1, "method":"subscribe", "params":{"query":query}})
                .to_string()
                .into(),
        ))
        .await
        .map_err(transport_error)?;
    // A published event can reach the connection ahead of the acknowledgement.
    let first = receive(&mut socket, query, true).await?;
    Ok((socket, first))
}

async fn next_event(
    socket: &mut Socket,
    query: &str,
    first: &mut Option<pb::SubscribeResponse>,
) -> Result<pb::SubscribeResponse, Status> {
    if let Some(event) = first.take() {
        return Ok(event);
    }
    loop {
        if let Some(event) = receive(socket, query, false).await? {
            return Ok(event);
        }
    }
}

async fn receive(
    socket: &mut Socket,
    query: &str,
    starting: bool,
) -> Result<Option<pb::SubscribeResponse>, Status> {
    while let Some(message) = socket.next().await {
        let text = match message.map_err(transport_error)? {
            Message::Text(text) => text,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
            _ => return Err(invalid_reply()),
        };
        let reply: RpcReply<WireResult> =
            serde_json::from_str(&text).map_err(|_| invalid_reply())?;
        if reply.jsonrpc != "2.0" || reply.id != 1 {
            return Err(invalid_reply());
        }
        if let Some(error) = reply.error {
            let message = error.data().unwrap_or_else(|| error.message());
            return Err(
                if message.starts_with("max_subscription_clients ")
                    || message.starts_with("max_subscriptions_per_client ")
                {
                    Status::resource_exhausted(message)
                } else if starting {
                    Status::invalid_argument(message)
                } else {
                    Status::unavailable(message)
                },
            );
        }
        return reply.result.ok_or_else(invalid_reply)?.response(query);
    }
    Err(Status::unavailable("Tendermint subscription disconnected"))
}

fn transport_error(error: tokio_tungstenite::tungstenite::Error) -> Status {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Capacity(_) => {
            Status::resource_exhausted("Tendermint subscription event is too large")
        }
        _ => {
            eprintln!("Minter v2 subscription failed: {error}");
            Status::unavailable("Tendermint subscription disconnected")
        }
    }
}

fn invalid_reply() -> Status {
    Status::internal("invalid Tendermint subscription response")
}

fn shutting_down() -> Status {
    Status::unavailable("Minter v2 is shutting down")
}
