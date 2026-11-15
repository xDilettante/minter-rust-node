use super::{error_body, error_response, http_status, parameters, query_field};
use crate::{
    json,
    pb::{SubscribeRequest, SubscribeResponse, api_service_server::ApiService},
};
use axum::{
    body::Body,
    extract::{
        Request as HttpRequest, State, WebSocketUpgrade,
        ws::{Message, WebSocket, rejection::WebSocketUpgradeRejection},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, Stream, StreamExt, stream};
use serde_json::Value;
use std::{convert::Infallible, future::ready, time::Duration};
use tonic::{Request, Status, codegen::BoxStream};

const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn subscribe<S: ApiService>(
    State(service): State<S>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: HttpRequest,
) -> Response {
    let wants_websocket = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let query = parameters(request).await.and_then(|query| {
        if upgrade.is_ok()
            && let Some((_, method)) =
                form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "method")
            && !method.is_empty()
            && method != "GET"
        {
            return Err(Status::unimplemented("Method Not Allowed"));
        }
        query_field(&query, "query", |value| Ok(value.to_owned()))
    });
    match upgrade {
        Ok(upgrade) => upgrade
            .protocols(["Bearer"])
            .max_message_size(64 * 1024)
            .max_frame_size(64 * 1024)
            .on_upgrade(move |socket| websocket(socket, service, query)),
        Err(error) if wants_websocket => error.into_response(),
        Err(_) => match query {
            Ok(query) => http(service, query).await,
            Err(error) => error_response(error),
        },
    }
}

async fn http<S: ApiService>(service: S, query: String) -> Response {
    let mut frames = Box::pin(open(&service, query).await);
    // The Go gateway sends the first status after the first message or error.
    let Some(first) = frames.next().await else {
        return StatusCode::OK.into_response();
    };
    let status = first.error.unwrap_or(StatusCode::OK);
    let lines = stream::once(ready(first)).chain(frames).map(|frame| {
        let mut line = frame.body.to_string();
        line.push('\n');
        Ok::<_, Infallible>(line)
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from_stream(lines),
    )
        .into_response()
}

async fn websocket<S: ApiService>(socket: WebSocket, service: S, query: Result<String, Status>) {
    let (mut write, mut read) = socket.split();
    tokio::select! {
        _ = async {
            while let Some(Ok(_)) = read.next().await {}
        } => {},
        _ = async {
            let mut frames = match query {
                Ok(query) => Box::pin(open(&service, query).await),
                Err(error) => {
                    let (_, body) = error_body(&error);
                    let _ = tokio::time::timeout(
                        WS_WRITE_TIMEOUT, write.send(Message::Text(body.to_string().into()))
                    ).await;
                    return;
                }
            };
            while let Some(frame) = frames.next().await {
                if !matches!(
                    tokio::time::timeout(
                        WS_WRITE_TIMEOUT, write.send(Message::Text(frame.body.to_string().into()))
                    ).await,
                    Ok(Ok(()))
                ) {
                    return;
                }
            }
            let _ = tokio::time::timeout(WS_WRITE_TIMEOUT, write.send(Message::Close(None))).await;
        } => {},
    }
}

struct Frame {
    body: Value,
    error: Option<StatusCode>,
}

async fn open<S: ApiService>(service: &S, query: String) -> impl Stream<Item = Frame> + use<S> {
    let responses: BoxStream<SubscribeResponse> = match service
        .subscribe(Request::new(SubscribeRequest { query }))
        .await
    {
        Ok(response) => response.into_inner(),
        Err(error) => Box::pin(stream::once(ready(Err(error)))),
    };
    stream::unfold(Some(responses), |input| async {
        let mut responses = input?;
        let frame = match responses.next().await?.and_then(|response| json(&response)) {
            Ok(body) => Frame {
                body: serde_json::json!({"result": body}),
                error: None,
            },
            Err(error) => Frame {
                body: serde_json::json!({
                    "error": {"code": error.code() as i32, "message": error.message(), "details": []}
                }),
                error: Some(http_status(error.code())),
            },
        };
        let next = frame.error.is_none().then_some(responses);
        Some((frame, next))
    })
}
