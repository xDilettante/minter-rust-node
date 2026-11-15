use super::{Options, advance, open, shutdown};
use anyhow::{Context, Result, ensure};
use minter_app::{
    api::Api,
    native_sync::{MAX_CHECKPOINT_MANIFEST_BYTES, NativeConsensus, NativeNode},
    validator_signer::ValidatorSigner,
};
use minter_p2p::{
    server::{BlockServer, BlockSource},
    snapshot::SnapshotFile,
};
use minter_state::{Checkpoint, OperationControl};
use std::{
    collections::BTreeSet,
    fs::File,
    io::BufReader,
    net::{Shutdown, SocketAddr},
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

const MAX_CONNECTIONS: usize = 16;
const MAX_GOSSIP_PEERS: usize = 16;

fn read_checkpoint(path: &Path) -> Result<Checkpoint> {
    let manifest = File::open(path.join("manifest.json"))?;
    ensure!(
        manifest.metadata()?.len() <= MAX_CHECKPOINT_MANIFEST_BYTES,
        "oversized snapshot manifest"
    );
    Ok(serde_json::from_reader(BufReader::new(manifest))?)
}

pub(super) async fn run(
    options: Options,
    address: SocketAddr,
    control: &OperationControl,
    signals: &mut JoinSet<std::io::Result<()>>,
) -> Result<()> {
    control.check()?;
    ensure!(
        options.mempool_peer.len() <= MAX_GOSSIP_PEERS,
        "too many configured mempool peers"
    );
    ensure!(
        options.consensus_peer.len() <= MAX_GOSSIP_PEERS,
        "too many configured consensus peers"
    );
    let listener = TcpListener::bind(address)
        .await
        .context("bind native P2P")?;
    let address = listener.local_addr()?;
    ensure!(
        !address.ip().is_unspecified() || options.p2p_advertise.is_some(),
        "provide --p2p-advertise with a wildcard P2P listener"
    );
    let preparing_control = control.clone();
    let (options, mut sync, snapshot, history_context, served_checkpoint) =
        tokio::task::spawn_blocking(move || {
            let sync = open(&options, &preparing_control)?;
            preparing_control.check()?;
            let (snapshot, served_checkpoint) = options
                .serve_snapshot
                .as_deref()
                .map(|path| {
                    let checkpoint = read_checkpoint(path)?;
                    sync.verify_snapshot_anchor(&checkpoint)?;
                    let snapshot = SnapshotFile::new(
                        File::open(path.join("snapshot.zlib"))?,
                        checkpoint.height,
                    )?;
                    ensure!(
                        snapshot.advertisement().hash == checkpoint.snapshot_sha256
                            && snapshot.bytes() == checkpoint.bytes,
                        "served snapshot differs from its manifest"
                    );
                    Ok::<_, anyhow::Error>((snapshot, checkpoint))
                })
                .transpose()?
                .unzip();
            let history_context = options
                .serve_snapshot
                .as_deref()
                .map(|path| sync.served_history_context(path))
                .transpose()?
                .flatten();
            preparing_control.check()?;
            Ok::<_, anyhow::Error>((options, sync, snapshot, history_context, served_checkpoint))
        })
        .await
        .context("open native sync worker")??;
    control.check()?;
    if options.mempool {
        sync.enable_mempool()?;
    }
    let blocks = sync.blocks()?;
    let signer = options
        .validator_key
        .as_ref()
        .zip(options.validator_journal.as_ref())
        .map(|(key, journal)| ValidatorSigner::open(journal, key, sync.chain_id()))
        .transpose()?;
    let default_key = options.db.join("node_key.json");
    let advertised = options.p2p_advertise.unwrap_or(address);
    let mut server = BlockServer::new(
        options.node_key.as_deref().unwrap_or(&default_key),
        sync.chain_id(),
        advertised,
        Duration::from_secs(options.timeout_secs),
        snapshot,
        options.mempool,
        options.consensus,
    )?;
    if let Some(context) = history_context {
        server = server.with_history_context(context)?;
    }
    let server = Arc::new(server);
    let mut gossip_ids = BTreeSet::new();
    for peer in &options.consensus_peer {
        ensure!(peer.id != server.id(), "consensus peer is this node");
        ensure!(
            gossip_ids.insert(peer.id),
            "duplicate consensus peer ID {}",
            peer.id
        );
    }
    for peer in &options.mempool_peer {
        ensure!(peer.id != server.id(), "mempool peer is this node");
        ensure!(
            gossip_ids.insert(peer.id),
            "duplicate mempool peer ID {}",
            peer.id
        );
        ensure!(
            options
                .peer
                .iter()
                .all(|block_peer| block_peer.id != peer.id || block_peer.address == peer.address),
            "configure the same address for the shared block and mempool peer {}",
            peer.id
        );
    }
    let node = NativeNode::new(sync);
    let consensus = options
        .consensus
        .then(|| NativeConsensus::new(node.clone(), signer))
        .transpose()?;
    let mempool = options.mempool.then(|| node.clone());
    let http = crate::bind(options.api_listen)
        .await
        .context("bind native Minter v2 HTTP")?;
    let grpc = crate::bind(options.grpc_listen)
        .await
        .context("bind native Minter v2 gRPC")?;
    let api = (http.is_some() || grpc.is_some())
        .then(|| Api::native(node.clone(), server.id(), server.public_key()))
        .transpose()?;
    let preparing = node.clone();
    let preparing_control = control.clone();
    tokio::task::spawn_blocking(move || {
        preparing_control.check()?;
        let mut sync = preparing.lock()?;
        // Pin only after snapshot/history, keys, consensus and bindings are ready,
        // and before the first cleanup can retire an old snapshot's headers.
        if let Some(checkpoint) = &served_checkpoint {
            sync.remember_served_snapshot(checkpoint)?;
        }
        sync.maintain_native_history()?;
        if let Some(checkpoint) = &served_checkpoint {
            let base = sync.blocks()?.range()?.base;
            if base > checkpoint.height.saturating_add(1) {
                eprintln!("Snapshot {} is not advertised: retained full blocks start at {base}; export a newer snapshot", checkpoint.height);
            }
        }
        preparing_control.check()?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("prepare native history retention")??;
    control.check()?;
    let (api_stop, api_stopped) = watch::channel(false);
    let mut services = JoinSet::new();
    if let Some(api) = api {
        if let Some(http) = http {
            println!("Minter v2 HTTP listening on {}", http.local_addr()?);
            let api = api.clone();
            let stop = api_stopped.clone();
            services.spawn(async move {
                minter_api::serve_http(http, api, crate::shutdown_signal(stop))
                    .await
                    .context("serve native Minter v2 HTTP")
            });
        }
        if let Some(grpc) = grpc {
            println!("Minter v2 gRPC listening on {}", grpc.local_addr()?);
            services.spawn(async move {
                minter_api::serve_grpc(grpc, api, crate::shutdown_signal(api_stopped))
                    .await
                    .context("serve native Minter v2 gRPC")
            });
        }
    }
    println!("Tendermint 0.34 P2P listening on {}@{address}", server.id());
    if advertised != address {
        println!(
            "Tendermint 0.34 P2P advertised as {}@{advertised}",
            server.id()
        );
    }
    let stopping = control.cancellation_flag();
    if let Some(consensus) = consensus.clone() {
        let stopping = stopping.clone();
        services.spawn(async move {
            tokio::task::spawn_blocking(move || {
                while !stopping.load(Ordering::Relaxed) {
                    consensus.tick()?;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(())
            })
            .await
            .context("consensus timer worker")?
        });
    }
    let mut gossip = JoinSet::new();
    for endpoint in options.consensus_peer.iter().copied() {
        let server = server.clone();
        let blocks = blocks.clone();
        let mempool = mempool.clone();
        let consensus = consensus
            .clone()
            .context("consensus peer needs a reactor")?;
        let stopping = stopping.clone();
        let mut stopped = api_stop.subscribe();
        let timeout = Duration::from_secs(options.timeout_secs);
        gossip.spawn(async move {
            while !*stopped.borrow() {
                let connected = tokio::select! {
                    result = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(endpoint.address)) => result,
                    _ = stopped.changed() => break,
                };
                let result = match connected {
                    Ok(Ok(stream)) => {
                        let stream = stream.into_std()?;
                        stream.set_nonblocking(false)?;
                        let socket = stream.try_clone()?;
                        let (server, blocks, mempool, consensus, stopping) = (
                            server.clone(), blocks.clone(), mempool.clone(), consensus.clone(), stopping.clone(),
                        );
                        let mut worker = tokio::task::spawn_blocking(move || {
                            server.serve_consensus_peer(stream, endpoint.id, &blocks, mempool.as_ref(), &consensus, &stopping)
                        });
                        tokio::select! {
                            result = &mut worker => result.context("consensus peer session task")?,
                            _ = stopped.changed() => {
                                let _ = socket.shutdown(Shutdown::Both);
                                worker.await.context("stop consensus peer session")??;
                                break;
                            },
                        }
                    }
                    Ok(Err(error)) => Err(error).context("connect consensus peer"),
                    Err(error) => Err(error).context("connect consensus peer timeout"),
                };
                if let Err(error) = result {
                    eprintln!("Tendermint consensus peer {}@{}: {error:#}", endpoint.id, endpoint.address);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                    _ = stopped.changed() => break,
                }
            }
            Ok::<_, anyhow::Error>(())
        });
    }
    for endpoint in options
        .mempool_peer
        .iter()
        .copied()
        .filter(|endpoint| options.peer.iter().all(|peer| peer.id != endpoint.id))
    {
        let node = node.clone();
        let server = server.clone();
        let stopping = stopping.clone();
        gossip.spawn(async move {
            while !stopping.load(Ordering::Relaxed) {
                let (server, node, stopping) = (server.clone(), node.clone(), stopping.clone());
                let result = tokio::task::spawn_blocking(move || {
                    server.follow_mempool(endpoint, &node, &stopping)
                })
                .await
                .context("mempool session task")?;
                if let Err(error) = result {
                    eprintln!("Tendermint mempool peer {}: {error:#}", endpoint.address);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok::<_, anyhow::Error>(())
        });
    }
    let mut replay = if options.peer.is_empty() && options.blocks.is_empty() {
        None
    } else {
        let cancelled = stopping.clone();
        let outbound = server.clone();
        Some(tokio::task::spawn_blocking(move || {
            advance(options, &node, Some(&outbound), || {
                cancelled.load(Ordering::Relaxed)
            })
        }))
    };
    let mut connections = JoinSet::new();
    let mut sockets = Vec::new();
    let mut result = loop {
        tokio::select! {
            signal = shutdown::wait(signals) => break signal,
            accepted = listener.accept() => {
                let (stream, remote) = match accepted.context("accept native P2P connection") {
                    Ok(result) => result,
                    Err(error) => break Err(error),
                };
                if connections.len() >= MAX_CONNECTIONS {
                    continue;
                }
                let stream = match stream.into_std().and_then(|stream| {
                    stream.set_nonblocking(false)?;
                    Ok(stream)
                }) {
                    Ok(stream) => stream,
                    Err(error) => break Err(error).context("prepare native P2P connection"),
                };
                let socket = match stream.try_clone() {
                    Ok(socket) => socket,
                    Err(error) => break Err(error).context("clone native P2P socket for shutdown"),
                };
                let server = server.clone();
                let blocks = blocks.clone();
                let mempool = mempool.clone();
                let consensus = consensus.clone();
                let stopping = stopping.clone();
                let task = connections.spawn_blocking(move || {
                    if let Err(error) = server.serve(stream, &blocks, mempool.as_ref(), consensus.as_ref(), &stopping) {
                        eprintln!("Tendermint peer {remote}: {error:#}");
                    }
                });
                sockets.push((task.id(), socket));
            }
            done = connections.join_next_with_id(), if !connections.is_empty() => {
                if let Some(done) = done {
                    let id = done.map_or_else(|error| error.id(), |(id, ())| id);
                    sockets.retain(|(task, _)| *task != id);
                }
            }
            service = services.join_next(), if !services.is_empty() => {
                break shutdown::service_exit(service, control, signals).await;
            }
            done = gossip.join_next(), if !gossip.is_empty() => {
                if let Some(done) = done {
                    match done {
                        Ok(Err(error)) => break Err(error),
                        Err(error) => break Err(error).context("native mempool worker"),
                        Ok(Ok(())) => {}
                    }
                }
            }
            done = async { replay.as_mut().expect("active replay").await }, if replay.is_some() => {
                replay = None;
                match done {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => break Err(error),
                    Err(error) => break Err(error).context("native sync worker"),
                }
            }
        }
    };
    stopping.store(true, Ordering::Relaxed);
    api_stop.send_replace(true);
    for (_, socket) in sockets {
        let _ = socket.shutdown(Shutdown::Both);
    }
    while connections.join_next().await.is_some() {}
    result = result.and(
        shutdown::drain(&mut gossip)
            .await
            .context("stop native gossip workers"),
    );
    match tokio::time::timeout(Duration::from_secs(6), shutdown::drain(&mut services)).await {
        Ok(drained) => result = result.and(drained.context("stop native node services")),
        Err(_) => {
            services.shutdown().await;
            result = result.and(Err(anyhow::anyhow!(
                "native node services did not stop in six seconds"
            )));
        }
    }
    if let Some(replay) = replay {
        replay.await.context("native sync worker")??;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_extended_manifest_and_rejects_oversized_input() {
        let directory = tempfile::tempdir().unwrap();
        let expected = Checkpoint {
            format: 1,
            network: "minter-mainnet-5".into(),
            height: 40_700_741,
            block_hash: [1; 32],
            app_hash: [2; 32],
            snapshot_sha256: [3; 32],
            bytes: 1,
            nodes: 1,
        };
        let mut value = serde_json::to_value(&expected).unwrap();
        value["comment"] = serde_json::Value::String("x".repeat(20 * 1024));
        let path = directory.path().join("manifest.json");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let actual = read_checkpoint(directory.path()).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        value["comment"] =
            serde_json::Value::String("x".repeat(MAX_CHECKPOINT_MANIFEST_BYTES as usize + 1));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_checkpoint(directory.path()).is_err());
    }
}
