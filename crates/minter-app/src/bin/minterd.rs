use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use minter_app::{Application, api::Api};
use prost::{Message, bytes::Bytes};
use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tendermint_proto::v0_34::abci::{Request, Response, ResponseException, request, response};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};

#[path = "minterd/native_history.rs"]
mod native_history;
#[path = "minterd/native_sync.rs"]
mod native_sync;
#[path = "minterd/shutdown.rs"]
mod shutdown;
#[path = "minterd/validator_signer.rs"]
mod validator_signer;

// Tendermint 0.34's ABCI socket uses Go's signed varint for the frame size.
const MAX_FRAME: usize = 100 * 1024 * 1024;
// The official minter-mainnet-5 InitChain carries about 122 MiB of app state.
const MAX_INIT_CHAIN_FRAME: usize = 128 * 1024 * 1024;
const INIT_CHAIN_TAG: u8 = (5 << 3) | 2;

#[derive(Parser)]
#[command(
    about = "Minter node and Tendermint 0.34 ABCI application",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Options {
    #[command(subcommand)]
    action: Option<Action>,
    #[arg(long, required = true)]
    db: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:26658")]
    abci_listen: SocketAddr,
    #[arg(long, default_value_t = 1)]
    transaction_chain_id: u8,
    /// Trusted Go snapshot manifest to use for Tendermint State Sync.
    #[arg(long)]
    state_sync_checkpoint: Option<PathBuf>,
    /// Keep this many committed IAVL versions, including the head. Zero keeps all future versions.
    #[arg(long)]
    keep_iavl_versions: Option<u64>,
    /// Keep this many blocks of Minter events. Zero keeps all future events.
    #[arg(long)]
    keep_event_blocks: Option<u64>,
    /// Enable the Minter v2 HTTP API on this address.
    #[arg(long)]
    api_listen: Option<SocketAddr>,
    /// Enable the Minter v2 gRPC API on this address.
    #[arg(long)]
    grpc_listen: Option<SocketAddr>,
    /// Tendermint 0.34 RPC address for this node.
    #[arg(long, default_value = "127.0.0.1:26657")]
    tendermint_rpc: SocketAddr,
}

#[derive(Subcommand)]
enum Action {
    /// Download and execute sequential Tendermint 0.34 blocks.
    NativeSync(Box<native_sync::Options>),
    /// Export a Go-compatible snapshot from a stopped native node.
    NativeSnapshot(native_sync::ExportOptions),
    /// Manage compact authenticated history preceding a native snapshot.
    NativeHistory {
        #[command(subcommand)]
        command: native_history::Command,
    },
    /// Import and use a Tendermint 0.34 private validator signing journal.
    ValidatorSigner {
        #[command(subcommand)]
        command: validator_signer::Command,
    },
}

type Call = (Request, oneshot::Sender<Result<Response>>);

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    match options.action {
        Some(Action::NativeSync(options)) => return native_sync::run(*options).await,
        Some(Action::NativeSnapshot(options)) => return native_sync::export(options).await,
        Some(Action::NativeHistory { command }) => return native_history::run(command).await,
        Some(Action::ValidatorSigner { command }) => return validator_signer::run(command),
        None => {}
    }
    let db = options.db.as_deref().context("missing ABCI database")?;
    ensure!(
        options.abci_listen.ip().is_loopback(),
        "ABCI must bind to a loopback address"
    );
    let mut app = if let Some(path) = options.state_sync_checkpoint {
        let file = fs::File::open(&path)
            .with_context(|| format!("open State Sync checkpoint {}", path.display()))?;
        let checkpoint = serde_json::from_reader(file).context("decode State Sync checkpoint")?;
        Application::open_with_state_sync(db, options.transaction_chain_id, checkpoint)?
    } else {
        Application::open(db, options.transaction_chain_id)?
    };
    if let Some(keep) = options.keep_iavl_versions {
        app.set_keep_iavl_versions(keep)?;
    }
    if let Some(keep) = options.keep_event_blocks {
        app.set_keep_event_blocks(keep)?;
    }
    let listener = TcpListener::bind(options.abci_listen)
        .await
        .context("bind ABCI socket")?;
    let http = bind(options.api_listen)
        .await
        .context("bind Minter v2 HTTP")?;
    let grpc = bind(options.grpc_listen)
        .await
        .context("bind Minter v2 gRPC")?;
    let (stop_api, api_stopped) = watch::channel(false);
    let mut services = JoinSet::<Result<()>>::new();
    let api = if http.is_some() || grpc.is_some() {
        let api = Api::new(options.tendermint_rpc)?;
        api.attach(&app);
        if let Some(http) = http {
            println!("Minter v2 HTTP listening on {}", http.local_addr()?);
            let service = api.clone();
            let shutdown = shutdown_signal(api_stopped.clone());
            services.spawn(async move {
                minter_api::serve_http(http, service, shutdown)
                    .await
                    .context("serve Minter v2 HTTP")
            });
        }
        if let Some(grpc) = grpc {
            println!("Minter v2 gRPC listening on {}", grpc.local_addr()?);
            let service = api.clone();
            services.spawn(async move {
                minter_api::serve_grpc(grpc, service, shutdown_signal(api_stopped))
                    .await
                    .context("serve Minter v2 gRPC")
            });
        }
        Some(api)
    } else {
        None
    };
    println!("ABCI 0.34 listening on {}", listener.local_addr()?);
    let (send, mut receive) = mpsc::channel::<Call>(32);
    let worker_api = api.clone();
    let worker = std::thread::spawn(move || {
        while let Some((request, reply)) = receive.blocking_recv() {
            let response = match &worker_api {
                Some(api) => api.handle_abci(&mut app, request),
                None => app.handle(request),
            };
            let _ = reply.send(response);
        }
    });
    let capacity = Arc::new(Semaphore::new(64));
    let mut connections = JoinSet::new();
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    let result = loop {
        tokio::select! {
            stop_result = &mut stop => break stop_result.context("wait for shutdown"),
            accepted = listener.accept() => {
                let (socket, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error).context("accept ABCI connection"),
                };
                let Ok(permit) = capacity.clone().try_acquire_owned() else { continue; };
                let send = send.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve(socket, send).await {
                        eprintln!("ABCI connection ended with error: {error:#}");
                    }
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => (),
            service = services.join_next(), if !services.is_empty() => {
                break match service {
                    Some(Ok(Err(error))) => Err(error),
                    Some(Err(error)) => Err(error).context("Minter v2 API task"),
                    _ => Err(anyhow::anyhow!("Minter v2 API stopped unexpectedly")),
                };
            }
        }
    };
    stop_api.send_replace(true);
    if let Some(api) = api {
        api.shutdown_subscriptions();
    }
    connections.shutdown().await;
    if tokio::time::timeout(Duration::from_secs(6), async {
        while services.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        services.shutdown().await;
    }
    drop(send);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("ABCI application thread failed"))?;
    result
}

async fn bind(address: Option<SocketAddr>) -> std::io::Result<Option<TcpListener>> {
    match address {
        Some(address) => Ok(Some(TcpListener::bind(address).await?)),
        None => Ok(None),
    }
}

async fn shutdown_signal(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

async fn serve(socket: TcpStream, worker: mpsc::Sender<Call>) -> Result<()> {
    socket.set_nodelay(true)?;
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    while let Some(length) = read_length(&mut reader).await? {
        if length > MAX_FRAME {
            ensure!(
                reader.fill_buf().await?.first() == Some(&INIT_CHAIN_TAG),
                "only InitChain can exceed the ABCI frame limit"
            );
        }
        let mut bytes = Vec::with_capacity(length.min(8192));
        let received = (&mut reader)
            .take(length as u64)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(received == length, "truncated ABCI frame");
        let request = Request::decode(Bytes::from(bytes))?;
        ensure!(
            length <= MAX_FRAME || matches!(request.value, Some(request::Value::InitChain(_))),
            "only InitChain can exceed the ABCI frame limit"
        );
        let (send, receive) = oneshot::channel();
        worker
            .send((request, send))
            .await
            .map_err(|_| anyhow::anyhow!("ABCI application stopped"))?;
        match receive.await? {
            Ok(response) => write_frame(&mut writer, &response).await?,
            Err(error) => {
                let response = Response {
                    value: Some(response::Value::Exception(ResponseException {
                        error: error.to_string(),
                    })),
                };
                write_frame(&mut writer, &response).await?;
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn read_length(reader: &mut (impl AsyncRead + Unpin)) -> Result<Option<usize>> {
    let mut size = 0u64;
    let mut next = [0u8; 1];
    for index in 0..10 {
        let count = reader.read(&mut next).await?;
        if count == 0 && index == 0 {
            return Ok(None);
        }
        ensure!(count == 1, "truncated ABCI length");
        let byte = next[0];
        ensure!(index != 9 || byte <= 1, "ABCI length overflow");
        size |= u64::from(byte & 0x7f) << (index * 7);
        if byte < 0x80 {
            ensure!(
                size & 1 == 0 && size >> 1 <= MAX_INIT_CHAIN_FRAME as u64,
                "invalid ABCI frame length"
            );
            return Ok(Some((size >> 1) as usize));
        }
    }
    anyhow::bail!("ABCI length overflow")
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), response: &Response) -> Result<()> {
    let bytes = response.encode_to_vec();
    ensure!(
        bytes.len() <= MAX_FRAME,
        "ABCI response exceeds the frame limit"
    );
    let mut size = (bytes.len() as u64) << 1;
    let mut prefix = Vec::with_capacity(10);
    while size >= 0x80 {
        prefix.push((size as u8 & 0x7f) | 0x80);
        size >>= 7;
    }
    prefix.push(size as u8);
    writer.write_all(&prefix).await?;
    writer.write_all(&bytes).await?;
    Ok(())
}
