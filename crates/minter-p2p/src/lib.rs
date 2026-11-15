//! Tendermint 0.34 connections for block and State Sync reactors.

pub mod consensus;
pub mod evidence;
pub mod mempool;
pub mod server;
pub mod signer;
pub mod snapshot;
pub mod verify;
mod wire;

use anyhow::{Context, Result, bail, ensure};
use ed25519_consensus::SigningKey;
use prost::Message;
use rand_core::OsRng;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tendermint::{
    Hash,
    block::{self, Height},
    chain,
    crypto::{Sha256, default::Sha256 as Hasher},
    merkle, node,
};
use tendermint_p2p::secret_connection::{PublicKey, SecretConnection, Version};
use tendermint_proto::v0_34::{
    blockchain::{self as bc, message::Sum},
    p2p::{DefaultNodeInfo, DefaultNodeInfoOther, ProtocolVersion},
    types::{self, Header},
};
use wire::{
    Channel, DeadlineStream, MAX_BLOCK_MESSAGE, Receiver, read_delimited, read_frame, send,
    write_frame,
};

const BLOCK_PROTOCOL: u64 = 11;
const MAX_NODE_INFO_SIZE: usize = 10 * 1024;

/// A TCP endpoint pinned to the expected Tendermint node ID.
#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    pub id: node::Id,
    pub address: SocketAddr,
}

impl FromStr for Endpoint {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self> {
        let (id, address) = input.split_once('@').context("expected id@IP:PORT")?;
        let id = id.parse().context("invalid Tendermint peer ID")?;
        let address: SocketAddr = address.parse().context("invalid peer socket address")?;
        ensure!(address.port() != 0, "peer port must be nonzero");
        Ok(Self { id, address })
    }
}

/// A status advertised by the peer for its local block store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRange {
    pub base: u64,
    pub height: u64,
}

/// A block with a matching transaction root and the exact bytes received from the peer.
///
/// Header and commit authentication against a trusted validator set belongs to block sync.
pub struct ReceivedBlock {
    block: NativeBlock,
    proto_header: Header,
    id: block::Id,
    encoded: Vec<u8>,
    evidence_size: usize,
    wire: Vec<u8>,
}

/// Validated block fields; evidence retains Go 0.34 wire semantics.
pub struct NativeBlock {
    pub header: block::Header,
    pub data: Vec<Vec<u8>>,
    pub last_commit: Option<block::Commit>,
    pub evidence: types::EvidenceList,
}

impl ReceivedBlock {
    pub fn from_proto(block: types::Block) -> Result<Self> {
        let response = bc::BlockResponse { block: Some(block) };
        Self::decode(
            bc::Message {
                sum: Some(Sum::BlockResponse(response)),
            }
            .encode_to_vec(),
        )
    }

    /// Read the next length-delimited block from a capture or archive shard.
    pub fn read(reader: &mut impl Read) -> Result<Option<Self>> {
        read_delimited(reader, MAX_BLOCK_MESSAGE)?
            .map(Self::decode)
            .transpose()
    }

    /// Write the exact received block in the length-delimited archive format.
    pub fn write(&self, writer: &mut impl Write) -> Result<()> {
        let mut prefix = Vec::new();
        prost::encode_length_delimiter(self.wire.len(), &mut prefix)?;
        writer.write_all(&prefix)?;
        writer.write_all(&self.wire)?;
        Ok(())
    }

    /// Decode a captured blockchain message with the same checks as a live response.
    pub fn decode(wire: Vec<u8>) -> Result<Self> {
        ensure!(
            wire.len() <= MAX_BLOCK_MESSAGE,
            "blockchain message too large"
        );
        let message = bc::Message::decode(wire.as_slice())?;
        let Some(Sum::BlockResponse(response)) = message.sum else {
            bail!("expected a BlockResponse");
        };
        Self::from_response(response, wire)
    }

    fn from_response(response: bc::BlockResponse, wire: Vec<u8>) -> Result<Self> {
        let mut proto = response.block.context("empty BlockResponse")?;
        evidence::canonical_header(proto.header.as_mut().context("missing block header")?);
        let proto_header = proto.header.clone().context("missing block header")?;
        let mut evidence = proto.evidence.take().unwrap_or_default();
        let evidence_size = evidence::input_size(&evidence);
        for item in &mut evidence.evidence {
            *item = evidence::canonicalize(item)?;
        }
        let evidence_payloads = evidence
            .evidence
            .iter()
            .map(evidence::payload)
            .collect::<Result<Vec<_>>>()?;
        proto.evidence = Some(evidence.clone());
        proto.data.get_or_insert_with(Default::default);
        evidence::canonical_commit(proto.last_commit.as_mut().context("missing LastCommit")?)?;
        let encoded = proto.encode_to_vec();
        let block = NativeBlock {
            header: proto.header.context("missing block header")?.try_into()?,
            data: proto
                .data
                .unwrap_or_default()
                .txs
                .into_iter()
                .map(|tx| tx.to_vec())
                .collect(),
            last_commit: proto
                .last_commit
                .map(TryInto::try_into)
                .transpose()?
                .filter(|commit| commit != &block::Commit::default()),
            evidence,
        };
        ensure!(
            block.header.version.block == BLOCK_PROTOCOL,
            "unexpected block protocol"
        );
        ensure!(
            block.header.last_commit_hash == Some(verify::commit_hash(block.last_commit.as_ref())),
            "block LastCommitHash differs from signatures"
        );
        let tx_ids: Vec<[u8; 32]> = block.data.iter().map(Hasher::digest).collect();
        let root = merkle::simple_hash_from_byte_vectors::<Hasher>(&tx_ids);
        ensure!(
            block.header.data_hash == Some(Hash::Sha256(root)),
            "block DataHash differs from transactions"
        );
        let evidence_hash = merkle::simple_hash_from_byte_vectors::<Hasher>(&evidence_payloads);
        ensure!(
            block.header.evidence_hash == Some(Hash::Sha256(evidence_hash)),
            "block EvidenceHash differs from evidence"
        );
        let id = verify::block_id(&block.header, &encoded)?;
        Ok(Self {
            block,
            proto_header,
            id,
            encoded,
            evidence_size,
            wire,
        })
    }

    pub fn block(&self) -> &NativeBlock {
        &self.block
    }

    pub fn proto_header(&self) -> &Header {
        &self.proto_header
    }

    pub fn id(&self) -> block::Id {
        self.id
    }

    pub fn encoded_len(&self) -> usize {
        self.encoded.len()
    }

    /// Canonical Go block bytes used to derive this block's PartSet.
    pub(crate) fn encoded_block(&self) -> &[u8] {
        &self.encoded
    }

    pub fn evidence(&self) -> &types::EvidenceList {
        &self.block.evidence
    }

    /// Size used by Go Evidence.MaxBytes, before value canonicalization.
    pub fn evidence_size(&self) -> usize {
        self.evidence_size
    }

    pub fn wire(&self) -> &[u8] {
        &self.wire
    }
}

type Connection = SecretConnection<DeadlineStream>;

struct Session {
    read_deadline: Arc<Mutex<Instant>>,
    write_deadline: Arc<Mutex<Instant>>,
    timeout: Duration,
    connection: Connection,
    receiver: Receiver,
}

/// An authenticated connection with a single outstanding block-reactor request.
///
/// A protocol or I/O failure closes the connection. A new request then needs a new peer.
pub struct Peer {
    remote: DefaultNodeInfo,
    session: Option<Session>,
    gossip: Option<mempool::GossipCursor>,
}

impl Peer {
    pub fn connect(endpoint: Endpoint, chain_id: &chain::Id, timeout: Duration) -> Result<Self> {
        let (remote, session) = Session::connect(endpoint, chain_id, timeout, &[Channel::Blocks])?;
        Ok(Self {
            remote,
            session: Some(session),
            gossip: None,
        })
    }

    pub(crate) fn with_gossip(remote: DefaultNodeInfo, session: Session) -> Result<Self> {
        let cursor = mempool::GossipCursor::new(remote.default_node_id.parse()?);
        Ok(Self {
            remote,
            session: Some(session),
            gossip: Some(cursor),
        })
    }

    pub fn node_info(&self) -> &DefaultNodeInfo {
        &self.remote
    }

    pub fn status(&mut self) -> Result<BlockRange> {
        self.status_with(None)
    }

    pub fn status_with(&mut self, pool: Option<&dyn mempool::Mempool>) -> Result<BlockRange> {
        self.exchange(
            Sum::StatusRequest(bc::StatusRequest {}),
            pool,
            |reply, _| {
                let Sum::StatusResponse(status) = reply else {
                    bail!("unexpected reply to status request")
                };
                block_range(status)
            },
        )
    }

    pub fn block(&mut self, height: Height) -> Result<Option<ReceivedBlock>> {
        self.block_with(height, None)
    }

    pub fn block_with(
        &mut self,
        height: Height,
        pool: Option<&dyn mempool::Mempool>,
    ) -> Result<Option<ReceivedBlock>> {
        let requested = i64::try_from(height.value()).context("height does not fit Tendermint")?;
        ensure!(requested > 0, "block height must be positive");
        let chain_id = self.remote.network.clone();
        self.exchange(
            Sum::BlockRequest(bc::BlockRequest { height: requested }),
            pool,
            |reply, wire| match reply {
                Sum::NoBlockResponse(missing) => {
                    ensure!(
                        missing.height == requested,
                        "unexpected NoBlockResponse height"
                    );
                    Ok(None)
                }
                Sum::BlockResponse(response) => {
                    let block = ReceivedBlock::from_response(response, wire)?;
                    ensure!(
                        block.block.header.height == height
                            && block.block.header.chain_id.as_str() == chain_id,
                        "unexpected block height or chain"
                    );
                    Ok(Some(block))
                }
                _ => bail!("unexpected reply to block request"),
            },
        )
    }

    fn exchange<T>(
        &mut self,
        request: Sum,
        pool: Option<&dyn mempool::Mempool>,
        response: impl FnOnce(Sum, Vec<u8>) -> Result<T>,
    ) -> Result<T> {
        let mut session = self.session.take().context("peer connection is closed")?;
        session.reset_deadline()?;
        let expects_status = matches!(request, Sum::StatusRequest(_));
        session
            .send_block(request)
            .context("send blockchain request")?;
        if let Some((cursor, pool)) = self.gossip.as_mut().zip(pool) {
            cursor
                .send_pending(pool, &mut session.connection)
                .context("send mempool messages")?;
        }
        let result = loop {
            let (channel, wire) = session.receive().context("receive blockchain reply")?;
            if channel != Channel::Blocks {
                let (cursor, pool) = self
                    .gossip
                    .as_ref()
                    .zip(pool)
                    .context("unexpected peer channel")?;
                cursor.receive(pool, channel, &wire)?;
                continue;
            }
            let reply = bc::Message::decode(wire.as_slice())?
                .sum
                .context("empty blockchain message")?;
            match reply {
                Sum::StatusRequest(_) => {
                    session.send_block(Sum::StatusResponse(bc::StatusResponse {
                        base: 0,
                        height: 0,
                    }))?;
                }
                Sum::BlockRequest(block) => {
                    ensure!(block.height >= 0, "negative requested height");
                    session.send_block(Sum::NoBlockResponse(bc::NoBlockResponse {
                        height: block.height,
                    }))?;
                }
                Sum::StatusResponse(status) if !expects_status => {
                    block_range(status)?;
                }
                reply => break response(reply, wire)?,
            }
        };
        self.session = Some(session);
        Ok(result)
    }
}

impl Session {
    fn connect(
        endpoint: Endpoint,
        chain_id: &chain::Id,
        timeout: Duration,
        channels: &[Channel],
    ) -> Result<(DefaultNodeInfo, Self)> {
        ensure!(!timeout.is_zero(), "peer timeout must be positive");
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("peer timeout is too large")?;
        let stream = TcpStream::connect_timeout(&endpoint.address, timeout)
            .with_context(|| format!("connect to {}", endpoint.address))?;
        let key = SigningKey::new(OsRng);
        let info = local_info(&key, chain_id, "127.0.0.1:26656".into(), channels);
        Self::handshake(
            stream,
            deadline,
            timeout,
            key,
            &info,
            channels,
            Some(endpoint.id),
        )
    }

    fn handshake(
        stream: TcpStream,
        deadline: Instant,
        timeout: Duration,
        key: SigningKey,
        info: &DefaultNodeInfo,
        channels: &[Channel],
        expected_id: Option<node::Id>,
    ) -> Result<(DefaultNodeInfo, Self)> {
        stream.set_nodelay(true)?;
        let read_deadline = Arc::new(Mutex::new(deadline));
        let write_deadline = Arc::new(Mutex::new(deadline));
        let mut connection = SecretConnection::new(
            DeadlineStream::new(stream, read_deadline.clone(), write_deadline.clone())?,
            key,
            Version::V0_34,
        )
        .context("Tendermint Secret Connection handshake")?;
        let authenticated_id = connection.remote_pubkey().peer_id();
        if let Some(expected_id) = expected_id {
            ensure!(
                authenticated_id == expected_id,
                "peer public key does not match configured ID {expected_id}"
            );
        }
        write_frame(&mut connection, info).context("send local NodeInfo")?;
        let remote: DefaultNodeInfo =
            read_frame(&mut connection, MAX_NODE_INFO_SIZE).context("receive peer NodeInfo")?;
        let remote_id: node::Id = remote
            .default_node_id
            .parse()
            .context("invalid NodeInfo ID")?;
        ensure!(
            remote_id == authenticated_id,
            "NodeInfo ID differs from authenticated ID"
        );
        ensure!(
            remote.network == info.network,
            "unexpected peer chain {:?}",
            remote.network
        );
        ensure!(
            remote
                .protocol_version
                .as_ref()
                .is_some_and(|v| v.block == BLOCK_PROTOCOL),
            "unsupported peer block protocol"
        );
        let shared = channels
            .iter()
            .copied()
            .filter(|channel| remote.channels.contains(&(*channel as u8)))
            .collect::<Vec<_>>();
        ensure!(
            if expected_id.is_some() {
                shared.len() == channels.len()
            } else {
                !shared.is_empty()
            },
            "peer did not advertise the requested Tendermint channels"
        );
        Ok((
            remote,
            Self {
                read_deadline,
                write_deadline,
                timeout,
                connection,
                receiver: Receiver::new(&shared),
            },
        ))
    }

    fn reset_deadline(&self) -> Result<()> {
        self.set_deadline(self.timeout)
    }

    fn set_deadline(&self, timeout: Duration) -> Result<()> {
        wire::set_deadline(&self.read_deadline, timeout)?;
        wire::set_deadline(&self.write_deadline, timeout)
    }

    fn send_block(&mut self, sum: Sum) -> Result<()> {
        self.send(Channel::Blocks, &bc::Message { sum: Some(sum) })
    }

    fn send(&mut self, channel: Channel, message: &impl Message) -> Result<()> {
        send(&mut self.connection, channel, message)
    }

    fn receive(&mut self) -> Result<(Channel, Vec<u8>)> {
        self.receiver.receive(&mut self.connection, usize::MAX)
    }
}

fn local_info(
    key: &SigningKey,
    chain_id: &chain::Id,
    listen_addr: String,
    channels: &[Channel],
) -> DefaultNodeInfo {
    DefaultNodeInfo {
        protocol_version: Some(ProtocolVersion {
            p2p: 8,
            block: BLOCK_PROTOCOL,
            app: 8,
        }),
        default_node_id: PublicKey::from(key).peer_id().to_string(),
        listen_addr,
        network: chain_id.to_string(),
        version: "0.34.19".into(),
        channels: channels.iter().map(|channel| *channel as u8).collect(),
        moniker: "minter-rust".into(),
        other: Some(DefaultNodeInfoOther {
            tx_index: "off".into(),
            rpc_address: String::new(),
        }),
    }
}

fn block_range(status: bc::StatusResponse) -> Result<BlockRange> {
    ensure!(
        0 <= status.base && status.base <= status.height,
        "invalid peer block range"
    );
    Ok(BlockRange {
        base: status.base.try_into()?,
        height: status.height.try_into()?,
    })
}
