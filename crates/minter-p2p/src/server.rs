//! Serve the committed block archive to Tendermint 0.34 peers.

use crate::{
    BlockRange, Endpoint, Peer, Session, block_range, consensus, local_info, mempool,
    snapshot::SnapshotFile, wire,
};
use anyhow::{Context, Result, bail, ensure};
use ed25519_consensus::SigningKey;
use prost::Message;
use rand_core::OsRng;
use std::{
    fs::File,
    io::{self, BufReader, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use tendermint::{chain, node, private_key::PrivateKey};
use tendermint_config::NodeKey;
use tendermint_p2p::secret_connection::PublicKey;
use tendermint_proto::v0_34::{
    blockchain::{self as bc, message::Sum},
    p2p::{DefaultNodeInfo, Packet, PacketPing, packet},
    statesync::{self as ss, message::Sum as SnapshotMessage},
};
use wire::Channel;

const BLOCK_CHANNELS: &[Channel] = &[Channel::Blocks];
const SNAPSHOT_CHANNELS: &[Channel] = &[Channel::Blocks, Channel::Snapshots, Channel::Chunks];
pub(crate) const MAX_REQUEST_BYTES: usize = 1024;
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// The transport reads only blocks that the application has already committed.
pub trait BlockSource {
    fn range(&self) -> Result<BlockRange>;
    fn block(&self, height: u64) -> Result<Option<Vec<u8>>>;
}

/// A stable P2P identity shared by inbound and outbound connections.
pub struct BlockServer {
    key: SigningKey,
    info: DefaultNodeInfo,
    snapshot: Option<SnapshotFile>,
    history_context: Option<SnapshotFile>,
    channels: Vec<Channel>,
    timeout: Duration,
}

impl BlockServer {
    pub fn new(
        node_key: &Path,
        chain_id: &chain::Id,
        advertised_addr: SocketAddr,
        timeout: Duration,
        snapshot: Option<SnapshotFile>,
        enable_mempool: bool,
        enable_consensus: bool,
    ) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
            "invalid P2P timeout"
        );
        let key = node_key_at(node_key)?;
        let mut channels = if snapshot.is_some() {
            SNAPSHOT_CHANNELS
        } else {
            BLOCK_CHANNELS
        }
        .to_vec();
        if enable_consensus {
            channels.extend_from_slice(consensus::CHANNELS);
            channels.push(Channel::Evidence);
        } else if enable_mempool {
            channels.push(Channel::RoundState);
        }
        if enable_mempool {
            channels.push(Channel::Mempool);
        }
        let info = local_info(&key, chain_id, advertised_addr.to_string(), &channels);
        Ok(Self {
            key,
            info,
            timeout,
            snapshot,
            history_context: None,
            channels,
        })
    }

    pub fn id(&self) -> node::Id {
        PublicKey::from(&self.key).peer_id()
    }

    pub fn with_history_context(mut self, context: SnapshotFile) -> Result<Self> {
        let snapshot = self
            .snapshot
            .as_ref()
            .context("history requires a state snapshot")?;
        ensure!(
            context.advertisement().format == crate::snapshot::CONTEXT_FORMAT
                && context.advertisement().height == snapshot.advertisement().height,
            "history context differs from its snapshot height or format"
        );
        self.history_context = Some(context);
        Ok(self)
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verification_key().to_bytes()
    }

    pub fn follow_mempool(
        &self,
        endpoint: Endpoint,
        mempool: &impl mempool::Mempool,
        stopping: &AtomicBool,
    ) -> Result<()> {
        let (stream, deadline) = self.dial(endpoint)?;
        let socket = stream.try_clone()?;
        let (_, session) = self.authenticate(stream, deadline, endpoint.id, mempool::CHANNELS)?;
        mempool::serve(
            session,
            endpoint.id,
            socket,
            Some(mempool),
            None,
            stopping,
            |_, _, _| bail!("unexpected gossip channel"),
        )
    }

    pub fn connect_block_and_mempool(&self, endpoint: Endpoint) -> Result<Peer> {
        let (stream, deadline) = self.dial(endpoint)?;
        let (remote, session) =
            self.authenticate(stream, deadline, endpoint.id, mempool::BLOCK_AND_GOSSIP)?;
        Peer::with_gossip(remote, session)
    }

    fn dial(&self, endpoint: Endpoint) -> Result<(TcpStream, Instant)> {
        ensure!(endpoint.id != self.id(), "self connection");
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .context("peer timeout is too large")?;
        let stream = TcpStream::connect_timeout(&endpoint.address, self.timeout)
            .with_context(|| format!("connect to {}", endpoint.address))?;
        Ok((stream, deadline))
    }

    fn authenticate(
        &self,
        stream: TcpStream,
        deadline: Instant,
        peer: node::Id,
        channels: &[Channel],
    ) -> Result<(DefaultNodeInfo, Session)> {
        let mut info = self.info.clone();
        info.channels = channels.iter().map(|channel| *channel as u8).collect();
        Session::handshake(
            stream,
            deadline,
            self.timeout,
            self.key.clone(),
            &info,
            channels,
            Some(peer),
        )
    }

    pub fn serve(
        &self,
        stream: TcpStream,
        blocks: &impl BlockSource,
        mempool: Option<&impl mempool::Mempool>,
        reactor: Option<&impl consensus::Reactor>,
        stopping: &AtomicBool,
    ) -> Result<()> {
        self.serve_connection(stream, None, blocks, mempool, reactor, stopping)
    }

    /// Authenticate an already connected consensus peer and run both directions of gossip.
    /// The caller owns dialing and can interrupt the handshake by shutting down a cloned socket.
    /// Consensus channels are required; other locally enabled channels are negotiated.
    pub fn serve_consensus_peer(
        &self,
        stream: TcpStream,
        peer: node::Id,
        blocks: &impl BlockSource,
        mempool: Option<&impl mempool::Mempool>,
        reactor: &impl consensus::Reactor,
        stopping: &AtomicBool,
    ) -> Result<()> {
        ensure!(peer != self.id(), "self connection");
        ensure!(
            consensus::CHANNELS
                .iter()
                .all(|c| self.channels.contains(c)),
            "local consensus channels are disabled"
        );
        self.serve_connection(stream, Some(peer), blocks, mempool, Some(reactor), stopping)
    }

    fn serve_connection(
        &self,
        stream: TcpStream,
        expected: Option<node::Id>,
        blocks: &impl BlockSource,
        mempool: Option<&impl mempool::Mempool>,
        reactor: Option<&impl consensus::Reactor>,
        stopping: &AtomicBool,
    ) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .context("P2P timeout is too large")?;
        let socket = stream.try_clone()?;
        let (remote, mut session) = Session::handshake(
            stream,
            deadline,
            self.timeout,
            self.key.clone(),
            &self.info,
            &self.channels,
            None,
        )?;
        let peer_id = remote.default_node_id.parse::<node::Id>()?;
        ensure!(peer_id != self.id(), "self connection");
        // The handshake already ties NodeInfo ID to the authenticated public key.
        // Check the configured ID before delivering anything to an application reactor.
        if let Some(expected) = expected {
            ensure!(
                peer_id == expected,
                "peer public key does not match configured ID {expected}"
            );
            ensure!(
                consensus::CHANNELS
                    .iter()
                    .all(|c| remote.channels.contains(&(*c as u8))),
                "peer did not advertise the required consensus channels"
            );
        }
        session.reset_deadline()?;
        if remote.channels.contains(&(Channel::Blocks as u8)) {
            send_status(&mut session.connection, blocks)?;
        }
        let supports_gossip = mempool::CHANNELS
            .iter()
            .all(|channel| remote.channels.contains(&(*channel as u8)));
        let pool = mempool.filter(|_| supports_gossip);
        let reactor = reactor.filter(|_| {
            consensus::CHANNELS
                .iter()
                .all(|channel| remote.channels.contains(&(*channel as u8)))
        });
        if pool.is_some() || reactor.is_some() {
            return mempool::serve(
                session,
                peer_id,
                socket,
                pool.map(|pool| pool as &dyn mempool::Mempool),
                reactor.map(|reactor| mempool::ConsensusRelay {
                    reactor,
                    evidence: remote.channels.contains(&(Channel::Evidence as u8)),
                }),
                stopping,
                |writer, channel, bytes| self.serve_request(writer, channel, bytes, blocks),
            );
        }
        let mut last_ping = Instant::now();
        loop {
            session.set_deadline(self.timeout.max(Duration::from_secs(90)))?;
            if last_ping.elapsed() >= PING_INTERVAL {
                wire::write_frame(
                    &mut session.connection,
                    &Packet {
                        sum: Some(packet::Sum::PacketPing(PacketPing {})),
                    },
                )?;
                last_ping = Instant::now();
            }
            let received = session
                .receiver
                .next(&mut session.connection, |_| MAX_REQUEST_BYTES);
            let (channel, message) = match received {
                Ok(wire::Incoming::Message(channel, message)) => (channel, message),
                Ok(wire::Incoming::Ping) => {
                    wire::pong(&mut session.connection)?;
                    continue;
                }
                Ok(wire::Incoming::Pending) => continue,
                Err(error) if peer_closed(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            session.reset_deadline()?;
            self.serve_request(&mut session.connection, channel, &message, blocks)?;
        }
    }

    fn serve_request(
        &self,
        writer: &mut impl Write,
        channel: Channel,
        message: &[u8],
        blocks: &impl BlockSource,
    ) -> Result<()> {
        match channel {
            Channel::Blocks => serve_block(writer, message, blocks),
            Channel::Snapshots | Channel::Chunks => {
                self.serve_snapshot(writer, channel, message, blocks.range()?.base)
            }
            _ => bail!("mempool relay is disabled"),
        }
    }

    fn serve_snapshot(
        &self,
        writer: &mut impl Write,
        channel: Channel,
        message: &[u8],
        base: u64,
    ) -> Result<()> {
        let snapshot = self.snapshot.as_ref().context("State Sync is disabled")?;
        let available = base <= snapshot.advertisement().height.saturating_add(1);
        let request = ss::Message::decode(message)?
            .sum
            .context("empty State Sync message")?;
        let reply = match (channel, request) {
            (Channel::Snapshots, SnapshotMessage::SnapshotsRequest(_)) => {
                // A state without its following bodies would strand a restoring peer.
                if !available {
                    return Ok(());
                }
                for file in [Some(snapshot), self.history_context.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    wire::send(
                        writer,
                        channel,
                        &ss::Message {
                            sum: Some(SnapshotMessage::SnapshotsResponse(
                                file.advertisement().clone(),
                            )),
                        },
                    )?;
                }
                return Ok(());
            }
            (Channel::Chunks, SnapshotMessage::ChunkRequest(request)) => {
                ensure!(request.height > 0, "snapshot height must be positive");
                let file = [Some(snapshot), self.history_context.as_ref()]
                    .into_iter()
                    .flatten()
                    .find(|file| available && file.advertisement().format == request.format);
                let chunk = file.map(|file| file.chunk(&request)).transpose()?.flatten();
                SnapshotMessage::ChunkResponse(ss::ChunkResponse {
                    height: request.height,
                    format: request.format,
                    index: request.index,
                    missing: chunk.is_none(),
                    chunk: chunk.unwrap_or_default(),
                })
            }
            _ => bail!("unexpected State Sync message on {channel:?}"),
        };
        wire::send(writer, channel, &ss::Message { sum: Some(reply) })
    }
}

pub(crate) fn peer_closed(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
        )
    })
}

fn serve_block(writer: &mut impl Write, message: &[u8], blocks: &impl BlockSource) -> Result<()> {
    let request = bc::Message::decode(message)?
        .sum
        .context("empty blockchain message")?;
    match request {
        Sum::StatusRequest(_) => send_status(writer, blocks)?,
        Sum::StatusResponse(status) => {
            block_range(status)?;
        }
        Sum::BlockRequest(request) => {
            let height = u64::try_from(request.height).context("negative block request")?;
            ensure!(height > 0, "requested block height must be positive");
            if let Some(block) = blocks.block(height)? {
                wire::send_encoded(writer, Channel::Blocks, &block)?;
            } else {
                send_block(
                    writer,
                    Sum::NoBlockResponse(bc::NoBlockResponse {
                        height: request.height,
                    }),
                )?;
            }
        }
        _ => bail!("unexpected blockchain response from inbound peer"),
    }
    Ok(())
}

fn send_block(writer: &mut impl Write, reply: Sum) -> Result<()> {
    wire::send(writer, Channel::Blocks, &bc::Message { sum: Some(reply) })
}

fn send_status(writer: &mut impl Write, blocks: &impl BlockSource) -> Result<()> {
    let range = blocks.range()?;
    ensure!(range.base <= range.height, "invalid local block range");
    send_block(
        writer,
        Sum::StatusResponse(bc::StatusResponse {
            base: range.base.try_into()?,
            height: range.height.try_into()?,
        }),
    )
}

pub(crate) fn node_key_at(path: &Path) -> Result<SigningKey> {
    match File::open(path) {
        Ok(file) => read_node_key(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let key = SigningKey::new(OsRng);
            let directory = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            serde_json::to_writer(
                &mut file,
                &NodeKey {
                    priv_key: PrivateKey::from(key.clone()),
                },
            )?;
            file.write_all(b"\n")?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(path) {
                Ok(_) => Ok(key),
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    read_node_key(File::open(path)?)
                }
                Err(error) => Err(error).context("publish Tendermint node key"),
            }
        }
        Err(error) => Err(error).context("open Tendermint node key"),
    }
}

fn read_node_key(file: File) -> Result<SigningKey> {
    ensure!(
        file.metadata()?.len() <= 1024,
        "oversized Tendermint node key"
    );
    let key: NodeKey =
        serde_json::from_reader(BufReader::new(file)).context("decode Tendermint node key")?;
    key.priv_key
        .ed25519_signing_key()
        .context("Tendermint node key must be Ed25519")?
        .clone()
        .try_into()
        .context("invalid Tendermint node key")
}

#[cfg(test)]
mod snapshot_retention_tests {
    use super::*;
    use std::io::Write;

    struct Source(u64);
    impl BlockSource for Source {
        fn range(&self) -> Result<BlockRange> {
            Ok(BlockRange {
                base: self.0,
                height: 110,
            })
        }
        fn block(&self, _: u64) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    #[test]
    fn stops_advertising_when_post_snapshot_blocks_have_been_retired() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut file = tempfile::NamedTempFile::new_in(dir.path())?;
        file.write_all(b"snapshot")?;
        let snapshot = SnapshotFile::new(file.reopen()?, 100)?;
        let server = BlockServer::new(
            &dir.path().join("node_key.json"),
            &"retention-test".parse()?,
            "127.0.0.1:12345".parse()?,
            Duration::from_secs(1),
            Some(snapshot),
            false,
            false,
        )?;
        let request = ss::Message {
            sum: Some(SnapshotMessage::SnapshotsRequest(ss::SnapshotsRequest {})),
        }
        .encode_to_vec();
        let mut output = Vec::new();
        server.serve_request(&mut output, Channel::Snapshots, &request, &Source(102))?;
        assert!(
            output.is_empty(),
            "advertised a snapshot whose following bodies are gone"
        );
        server.serve_request(&mut output, Channel::Snapshots, &request, &Source(101))?;
        assert!(
            !output.is_empty(),
            "withheld a snapshot with a complete retained handoff"
        );
        Ok(())
    }
}
