use anyhow::Result;
use minter_p2p::{
    BlockRange,
    consensus::{EvidenceKey, Gossip, PendingEvidence, Reactor},
    mempool::Mempool,
    server::{BlockServer, BlockSource},
    snapshot::{CONTEXT_FORMAT, SnapshotFile, SnapshotPeer},
};
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tendermint::{chain, node};
use tendermint_proto::v0_34::{consensus as cs, types};

#[derive(Default)]
struct Probe {
    peers: Mutex<Vec<node::Id>>,
}

impl BlockSource for Probe {
    fn range(&self) -> Result<BlockRange> {
        Ok(BlockRange { base: 0, height: 0 })
    }
    fn block(&self, _: u64) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

impl Reactor for Probe {
    fn round_step(&self) -> Result<cs::NewRoundStep> {
        Ok(cs::NewRoundStep {
            height: 101,
            round: 0,
            step: 1,
            seconds_since_start_time: 0,
            last_commit_round: -1,
        })
    }
    fn receive(&self, peer: node::Id, _: cs::message::Sum) -> Result<Option<cs::message::Sum>> {
        self.peers.lock().unwrap().push(peer);
        Ok(None)
    }
    fn pending(&self, _: &cs::NewRoundStep, _: &mut Gossip) -> Result<Vec<cs::message::Sum>> {
        Ok(Vec::new())
    }
    fn receive_evidence(&self, _: types::EvidenceList) -> Result<()> {
        Ok(())
    }
    fn pending_evidence(&self, _: u64, _: Option<EvidenceKey>) -> Result<Option<PendingEvidence>> {
        Ok(None)
    }
}

fn server(directory: &std::path::Path, name: &str, consensus: bool, mempool: bool) -> BlockServer {
    BlockServer::new(
        &directory.join(name),
        &chain::Id::try_from("native-transport-test").unwrap(),
        "127.0.0.1:1".parse().unwrap(),
        Duration::from_secs(2),
        None,
        mempool,
        consensus,
    )
    .unwrap()
}

#[test]
fn one_connection_reads_state_and_history_snapshot_formats() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let state_path = directory.path().join("state.bin");
    let history_path = directory.path().join("history.bin");
    std::fs::write(&state_path, b"go-state")?;
    std::fs::write(&history_path, b"signed-history")?;
    let state = SnapshotFile::new(std::fs::File::open(&state_path)?, 42)?;
    let history = SnapshotFile::new_context(std::fs::File::open(&history_path)?, 42)?;
    let state_hash: [u8; 32] = state.advertisement().hash.as_slice().try_into()?;
    let history_hash: [u8; 32] = history.advertisement().hash.as_slice().try_into()?;
    let chain: chain::Id = "native-transport-test".parse()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let remote = BlockServer::new(
        &directory.path().join("server_key.json"),
        &chain,
        address,
        Duration::from_secs(2),
        Some(state),
        false,
        false,
    )?
    .with_history_context(history)?;
    let endpoint = format!("{}@{address}", remote.id()).parse()?;
    let stopping = AtomicBool::new(false);
    thread::scope(|scope| -> Result<()> {
        let inbound = scope.spawn(|| -> Result<()> {
            let (stream, _) = listener.accept()?;
            remote.serve(
                stream,
                &Probe::default(),
                None::<&NoPool>,
                None::<&Probe>,
                &stopping,
            )
        });
        let result = (|| -> Result<()> {
            let mut peer = SnapshotPeer::connect(endpoint, &chain, Duration::from_secs(2))?;
            assert_eq!(peer.find(42, 1, &state_hash)?.chunks, 1);
            assert_eq!(peer.chunk(42, 1, 0)?, b"go-state");
            assert_eq!(peer.find(42, CONTEXT_FORMAT, &history_hash)?.chunks, 1);
            assert_eq!(peer.chunk(42, CONTEXT_FORMAT, 0)?, b"signed-history");
            Ok(())
        })();
        let served = inbound.join().unwrap();
        result?;
        served
    })
}

#[test]
fn outbound_consensus_negotiates_optional_channels_and_stops() {
    let directory = tempfile::tempdir().unwrap();
    let local = server(directory.path(), "local.json", true, true);
    let remote = server(directory.path(), "remote.json", true, false);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    let a = Probe::default();
    let b = Probe::default();
    thread::scope(|scope| {
        let inbound =
            scope.spawn(|| remote.serve(accepted, &b, None::<&NoPool>, Some(&b), &stopping));
        let outbound = scope.spawn(|| {
            local.serve_consensus_peer(stream, remote.id(), &a, Some(&NoPool), &a, &stopping)
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let exchanged = loop {
            if a.peers.lock().unwrap().contains(&remote.id())
                && b.peers.lock().unwrap().contains(&local.id())
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let began_stopping = Instant::now();
        stopping.store(true, Ordering::Relaxed);
        assert!(outbound.join().unwrap().is_ok());
        assert!(inbound.join().unwrap().is_ok());
        assert!(began_stopping.elapsed() < Duration::from_secs(1));
        assert!(
            exchanged,
            "both reactors must receive authenticated peer messages"
        );
    });
}

#[test]
fn outbound_consensus_refuses_wrong_identity_and_missing_consensus() {
    for wrong_id in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let local = server(directory.path(), "local.json", true, false);
        let remote = server(directory.path(), "remote.json", wrong_id, false);
        let expected = if wrong_id {
            node::Id::new([9; 20])
        } else {
            remote.id()
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let stopping = AtomicBool::new(false);
        let a = Probe::default();
        let b = Probe::default();
        thread::scope(|scope| {
            let inbound =
                scope.spawn(|| remote.serve(accepted, &b, None::<&NoPool>, Some(&b), &stopping));
            let error = local
                .serve_consensus_peer(stream, expected, &a, None::<&NoPool>, &a, &stopping)
                .unwrap_err();
            let expected_error = if wrong_id {
                "configured ID"
            } else {
                "consensus channels"
            };
            assert!(format!("{error:#}").contains(expected_error), "{error:#}");
            stopping.store(true, Ordering::Relaxed);
            let _ = inbound.join().unwrap();
            assert!(a.peers.lock().unwrap().is_empty());
        });
    }
}

#[test]
fn caller_can_interrupt_an_outbound_handshake() {
    let directory = tempfile::tempdir().unwrap();
    let local = server(directory.path(), "local.json", true, false);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_silent_peer, _) = listener.accept().unwrap();
    let cancel = stream.try_clone().unwrap();
    let stopping = AtomicBool::new(false);
    let probe = Probe::default();
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            local.serve_consensus_peer(
                stream,
                node::Id::new([9; 20]),
                &probe,
                None::<&NoPool>,
                &probe,
                &stopping,
            )
        });
        thread::sleep(Duration::from_millis(50));
        let began_stopping = Instant::now();
        cancel.shutdown(std::net::Shutdown::Both).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(began_stopping.elapsed() < Duration::from_secs(1));
        assert!(probe.peers.lock().unwrap().is_empty());
    });
}

struct NoPool;
impl Mempool for NoPool {
    fn round_step(&self) -> Result<cs::NewRoundStep> {
        unreachable!()
    }
    fn receive(&self, _: node::Id, _: Vec<u8>) -> Result<()> {
        unreachable!()
    }
    fn pending(&self, _: node::Id, _: u64, _: u64) -> Result<(u64, Vec<Arc<[u8]>>)> {
        unreachable!()
    }
}
