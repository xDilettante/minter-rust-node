//! Tendermint 0.34 transaction gossip and the peer height used by the Go reactor.

use crate::{
    Session,
    consensus::{self, Reactor},
    wire::{self, Channel, Incoming, MAX_MEMPOOL_MESSAGE},
};
use anyhow::{Context, Result, ensure};
use prost::Message;
use std::{
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tendermint::node;
use tendermint_p2p::secret_connection::Sender;
use tendermint_proto::v0_34::{
    consensus as cs, mempool as mp,
    p2p::{Packet, PacketPing, packet},
    types,
};

pub(crate) const CHANNELS: &[Channel] = &[Channel::RoundState, Channel::Mempool];
pub(crate) const BLOCK_AND_GOSSIP: &[Channel] =
    &[Channel::RoundState, Channel::Mempool, Channel::Blocks];
const POLL: Duration = Duration::from_millis(100);
const KEEPALIVE: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_TXS_PER_MESSAGE: usize = 64;

#[derive(Clone, Copy)]
pub(crate) struct ConsensusRelay<'a> {
    pub reactor: &'a dyn Reactor,
    pub evidence: bool,
}

struct ConnectedPeer<'a> {
    reactor: &'a dyn Reactor,
    peer: node::Id,
}

impl Drop for ConnectedPeer<'_> {
    fn drop(&mut self) {
        self.reactor.peer_disconnected(self.peer);
    }
}

/// Application-owned pool. Only validated transactions can be returned for relay.
pub trait Mempool: Sync {
    fn round_step(&self) -> Result<cs::NewRoundStep>;
    fn receive(&self, source: node::Id, tx: Vec<u8>) -> Result<()>;
    fn pending(
        &self,
        peer: node::Id,
        after: u64,
        peer_height: u64,
    ) -> Result<(u64, Vec<Arc<[u8]>>)>;
}

pub(crate) struct GossipCursor {
    remote: node::Id,
    peer_height: AtomicU64,
    sequence: u64,
    last_height: Option<i64>,
}

impl GossipCursor {
    pub(crate) fn new(remote: node::Id) -> Self {
        Self {
            remote,
            peer_height: AtomicU64::new(0),
            sequence: 0,
            last_height: None,
        }
    }

    pub(crate) fn send_pending(
        &mut self,
        pool: &dyn Mempool,
        writer: &mut impl std::io::Write,
    ) -> Result<()> {
        let step = pool.round_step()?;
        if Some(step.height) != self.last_height {
            wire::send(
                writer,
                Channel::RoundState,
                &cs::Message {
                    sum: Some(cs::message::Sum::NewRoundStep(step)),
                },
            )?;
            self.last_height = Some(step.height);
        }
        let (sequence, entries) = pool.pending(
            self.remote,
            self.sequence,
            self.peer_height.load(Ordering::Relaxed),
        )?;
        for tx in entries {
            wire::send(writer, Channel::Mempool, &encode_tx(&tx))?;
        }
        self.sequence = sequence;
        Ok(())
    }

    pub(crate) fn receive(&self, pool: &dyn Mempool, channel: Channel, bytes: &[u8]) -> Result<()> {
        match channel {
            Channel::RoundState => receive_height(bytes, &self.peer_height),
            Channel::Mempool => {
                for tx in decode_txs(bytes)? {
                    pool.receive(self.remote, tx)?;
                }
                Ok(())
            }
            _ => anyhow::bail!("unexpected gossip channel"),
        }
    }
}

/// Standalone Go-compatible gossip session with a pinned remote identity.
pub(crate) fn serve(
    session: Session,
    remote: node::Id,
    socket: TcpStream,
    mempool: Option<&dyn Mempool>,
    consensus: Option<ConsensusRelay<'_>>,
    stopping: &AtomicBool,
    mut other: impl FnMut(&mut Sender<wire::DeadlineStream>, Channel, &[u8]) -> Result<()>,
) -> Result<()> {
    let local_port = socket.local_addr()?.port();
    let remote_port = socket.peer_addr()?.port();
    let _peer = consensus
        .map(|relay| {
            relay.reactor.peer_connected(remote)?;
            Ok::<_, anyhow::Error>(ConnectedPeer {
                reactor: relay.reactor,
                peer: remote,
            })
        })
        .transpose()?;
    let Session {
        connection,
        mut receiver,
        timeout,
        read_deadline,
        write_deadline,
    } = session;
    let (send, mut read) = connection.split()?;
    let writer = Writer {
        send,
        deadline: write_deadline,
        timeout,
    };
    let writer = Mutex::new(writer);
    let peer_height = AtomicU64::new(0);
    let peer_step = Mutex::new(None);
    let ended = AtomicBool::new(false);
    thread::scope(|scope| {
        let push = scope.spawn(|| {
            let result = push_messages(
                mempool,
                consensus,
                remote,
                &writer,
                &peer_height,
                &peer_step,
                || stopping.load(Ordering::Relaxed) || ended.load(Ordering::Relaxed),
            );
            let _ = socket.shutdown(Shutdown::Both);
            result
        });
        let result = (|| {
            let mut peer_ready = false;
            while !stopping.load(Ordering::Relaxed) {
                wire::set_deadline(&read_deadline, READ_TIMEOUT)?;
                match receiver.next(&mut read, |channel| match channel {
                    Channel::Mempool => MAX_MEMPOOL_MESSAGE,
                    Channel::Evidence => crate::evidence::MAX_MESSAGE,
                    channel if channel.is_consensus() => consensus::MAX_MESSAGE,
                    _ => crate::server::MAX_REQUEST_BYTES,
                })? {
                    Incoming::Ping => locked(&writer)?.pong()?,
                    Incoming::Pending => {}
                    Incoming::Message(Channel::Evidence, bytes) => {
                        let relay = consensus
                            .filter(|relay| relay.evidence)
                            .context("evidence relay is disabled")?;
                        relay
                            .reactor
                            .receive_evidence(types::EvidenceList::decode(bytes.as_slice())?)?;
                    }
                    Incoming::Message(Channel::Mempool, bytes) => {
                        let mempool = mempool.context("mempool relay is disabled")?;
                        for tx in decode_txs(&bytes)? {
                            mempool.receive(remote, tx)?;
                        }
                    }
                    Incoming::Message(channel, bytes) if channel.is_consensus() => {
                        let message = consensus::decode(channel, &bytes)?;
                        let round_step = matches!(&message, cs::message::Sum::NewRoundStep(_));
                        if let cs::message::Sum::NewRoundStep(step) = &message {
                            observe_height(step, &peer_height)?;
                            let mut last = peer_step
                                .lock()
                                .map_err(|_| anyhow::anyhow!("peer round step lock"))?;
                            if last
                                .as_ref()
                                .is_none_or(|old| position(old) < position(step))
                            {
                                *last = Some(*step);
                            }
                        }
                        if let Some(relay) = consensus {
                            let reply = relay.reactor.receive(remote, message)?;
                            if round_step && !peer_ready {
                                println!(
                                    "{}",
                                    serde_json::json!({
                                        "event": "consensus_peer_ready", "peerId": remote.to_string(),
                                        "localPort": local_port, "remotePort": remote_port,
                                    })
                                );
                                peer_ready = true;
                            }
                            if let Some(reply) = reply {
                                locked(&writer)?.send(
                                    consensus::channel(&reply),
                                    &cs::Message { sum: Some(reply) },
                                )?;
                            }
                        }
                    }
                    Incoming::Message(channel, bytes) => {
                        let mut writer = locked(&writer)?;
                        wire::set_deadline(&writer.deadline, timeout)?;
                        other(&mut writer.send, channel, &bytes)?;
                    }
                }
            }
            Ok(())
        })();
        ended.store(true, Ordering::Relaxed);
        let _ = socket.shutdown(Shutdown::Both);
        push.join()
            .map_err(|_| anyhow::anyhow!("Tendermint gossip writer panicked"))??;
        if stopping.load(Ordering::Relaxed)
            || result.as_ref().is_err_and(crate::server::peer_closed)
        {
            Ok(())
        } else {
            result
        }
    })
}

struct Writer {
    send: Sender<wire::DeadlineStream>,
    deadline: Arc<Mutex<Instant>>,
    timeout: Duration,
}

impl Writer {
    fn send(&mut self, channel: Channel, message: &impl Message) -> Result<()> {
        wire::set_deadline(&self.deadline, self.timeout)?;
        wire::send(&mut self.send, channel, message)
    }

    fn ping(&mut self) -> Result<()> {
        wire::set_deadline(&self.deadline, self.timeout)?;
        wire::write_frame(
            &mut self.send,
            &Packet {
                sum: Some(packet::Sum::PacketPing(PacketPing {})),
            },
        )
    }

    fn pong(&mut self) -> Result<()> {
        wire::set_deadline(&self.deadline, self.timeout)?;
        wire::pong(&mut self.send)
    }
}

fn locked(writer: &Mutex<Writer>) -> Result<std::sync::MutexGuard<'_, Writer>> {
    writer
        .lock()
        .map_err(|_| anyhow::anyhow!("gossip writer lock"))
}

fn push_messages(
    pool: Option<&dyn Mempool>,
    consensus: Option<ConsensusRelay<'_>>,
    peer: node::Id,
    writer: &Mutex<Writer>,
    height: &AtomicU64,
    peer_step: &Mutex<Option<cs::NewRoundStep>>,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    let mut last_step = None;
    let mut last_ping = Instant::now();
    let mut cursor = 0;
    let mut consensus_gossip = consensus::Gossip::default();
    let mut evidence_cursor = None;
    let mut evidence_retry = Instant::now();
    while !stopping() {
        let step = match (consensus, pool) {
            (Some(relay), _) => relay.reactor.round_step()?,
            (None, Some(pool)) => pool.round_step()?,
            (None, None) => anyhow::bail!("peer session has no gossip reactor"),
        };
        if Some(position(&step)) != last_step {
            locked(writer)?.send(
                Channel::RoundState,
                &cs::Message {
                    sum: Some(cs::message::Sum::NewRoundStep(step)),
                },
            )?;
            last_step = Some(position(&step));
        }
        if let Some(relay) = consensus {
            let reactor = relay.reactor;
            let peer_round = *peer_step
                .lock()
                .map_err(|_| anyhow::anyhow!("peer round step lock"))?;
            if let Some(peer_round) = peer_round {
                for message in reactor.pending(&peer_round, &mut consensus_gossip)? {
                    locked(writer)?.send(
                        consensus::channel(&message),
                        &cs::Message { sum: Some(message) },
                    )?;
                }
            }
            let peer_height = height.load(Ordering::Relaxed);
            if relay.evidence && peer_height != 0 && Instant::now() >= evidence_retry {
                if let Some((key, item)) = reactor.pending_evidence(peer_height, evidence_cursor)? {
                    locked(writer)?.send(
                        Channel::Evidence,
                        &types::EvidenceList {
                            evidence: vec![item],
                        },
                    )?;
                    evidence_cursor = Some(key);
                } else {
                    evidence_cursor = None;
                    evidence_retry = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(pool) = pool {
            let (next, entries) = pool.pending(peer, cursor, height.load(Ordering::Relaxed))?;
            for tx in entries {
                locked(writer)?.send(Channel::Mempool, &encode_tx(&tx))?;
            }
            cursor = next;
        }
        if last_ping.elapsed() >= KEEPALIVE {
            locked(writer)?.ping()?;
            last_ping = Instant::now();
        }
        thread::sleep(POLL);
    }
    Ok(())
}

fn position(step: &cs::NewRoundStep) -> (i64, i32, u32) {
    (step.height, step.round, step.step)
}

fn receive_height(bytes: &[u8], height: &AtomicU64) -> Result<()> {
    let message = cs::Message::decode(bytes)?
        .sum
        .context("empty round state message")?;
    if let cs::message::Sum::NewRoundStep(step) = message {
        observe_height(&step, height)?;
    }
    Ok(())
}

fn observe_height(step: &cs::NewRoundStep, height: &AtomicU64) -> Result<()> {
    ensure!(
        step.height > 0
            && step.round >= 0
            && (1..=8).contains(&step.step)
            && step.last_commit_round >= -1,
        "invalid Tendermint round step"
    );
    height.fetch_max(step.height as u64, Ordering::Relaxed);
    Ok(())
}
fn encode_tx(raw: &[u8]) -> mp::Message {
    mp::Message {
        sum: Some(mp::message::Sum::Txs(mp::Txs {
            txs: vec![raw.to_vec()],
        })),
    }
}

fn decode_txs(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let Some(mp::message::Sum::Txs(txs)) = mp::Message::decode(bytes)?.sum else {
        anyhow::bail!("empty Tendermint mempool message");
    };
    ensure!(
        (1..=MAX_TXS_PER_MESSAGE).contains(&txs.txs.len()),
        "invalid Tendermint transaction batch size"
    );
    Ok(txs.txs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_tendermint_transaction_envelope() {
        let wire = [10, 4, 10, 2, 1, 2];
        assert_eq!(encode_tx(&[1, 2]).encode_to_vec(), wire);
        assert_eq!(decode_txs(&wire).unwrap(), [vec![1, 2]]);
        assert!(decode_txs(&[10, 0]).is_err());
    }

    #[test]
    fn tracks_peer_height_monotonically() {
        let height = AtomicU64::new(0);
        for h in [10, 9] {
            let msg = cs::Message {
                sum: Some(cs::message::Sum::NewRoundStep(cs::NewRoundStep {
                    height: h,
                    step: 1,
                    last_commit_round: 0,
                    ..Default::default()
                })),
            };
            receive_height(&msg.encode_to_vec(), &height).unwrap();
        }
        assert_eq!(height.load(Ordering::Relaxed), 10);
    }
}
