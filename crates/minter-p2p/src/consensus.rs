//! Tendermint 0.34 consensus channels and block parts.

use crate::{ReceivedBlock, wire::Channel};
use anyhow::{Context, Result, ensure};
use prost::Message;
use std::sync::OnceLock;
use tendermint::{
    Hash, block,
    crypto::default::Sha256,
    merkle::{self, MerkleHash},
    node,
};
use tendermint_proto::v0_34::libs::bits::BitArray;
use tendermint_proto::v0_34::{
    consensus::{self as cs, message::Sum},
    types,
};

pub(crate) const CHANNELS: &[Channel] = &[
    Channel::RoundState,
    Channel::ConsensusData,
    Channel::ConsensusVote,
    Channel::VoteSetBits,
];
pub(crate) const MAX_MESSAGE: usize = 1024 * 1024;
pub const PART_SIZE: usize = 65_536;
const MAX_PARTS: usize = 100 * 1024 * 1024 / PART_SIZE + 1;

pub type EvidenceKey = (u64, [u8; 32]);
pub type PendingEvidence = (EvidenceKey, types::Evidence);

pub trait Reactor: Sync {
    fn peer_connected(&self, _peer: node::Id) -> Result<()> {
        Ok(())
    }
    fn peer_disconnected(&self, _peer: node::Id) {}
    fn round_step(&self) -> Result<cs::NewRoundStep>;
    fn receive(&self, peer: node::Id, message: Sum) -> Result<Option<Sum>>;
    fn pending(&self, peer_step: &cs::NewRoundStep, gossip: &mut Gossip) -> Result<Vec<Sum>>;
    fn receive_evidence(&self, message: types::EvidenceList) -> Result<()>;
    fn pending_evidence(
        &self,
        peer_height: u64,
        after: Option<EvidenceKey>,
    ) -> Result<Option<PendingEvidence>>;
}

pub fn bit_array(size: usize, indices: impl IntoIterator<Item = usize>) -> BitArray {
    let mut elems = vec![0; size.div_ceil(64)];
    for index in indices {
        elems[index / 64] |= 1 << (index % 64);
    }
    BitArray {
        bits: size as i64,
        elems,
    }
}

#[derive(Default)]
pub struct Gossip {
    position: Option<(i64, i32, block::parts::Header)>,
    part: usize,
}

impl Gossip {
    pub fn parts(&mut self, height: i64, round: i32, set: &BlockParts, limit: usize) -> Vec<Sum> {
        let total = set.header().total as usize;
        let position = (height, round, set.header());
        if self.position != Some(position) {
            self.position = Some(position);
            self.part = 0;
        }
        let mut messages = Vec::new();
        for _ in 0..total.min(limit) {
            if let Some(part) = set.part(self.part) {
                messages.push(Sum::BlockPart(cs::BlockPart {
                    height,
                    round,
                    part: Some(part.clone()),
                }));
            }
            self.part = (self.part + 1) % total;
        }
        messages
    }
}

pub(crate) fn channel(message: &Sum) -> Channel {
    match message {
        Sum::NewRoundStep(_) | Sum::NewValidBlock(_) | Sum::HasVote(_) | Sum::VoteSetMaj23(_) => {
            Channel::RoundState
        }
        Sum::Proposal(_) | Sum::ProposalPol(_) | Sum::BlockPart(_) => Channel::ConsensusData,
        Sum::Vote(_) => Channel::ConsensusVote,
        Sum::VoteSetBits(_) => Channel::VoteSetBits,
    }
}

pub(crate) fn decode(expected: Channel, bytes: &[u8]) -> Result<Sum> {
    let message = cs::Message::decode(bytes)?
        .sum
        .context("empty consensus message")?;
    ensure!(
        expected == channel(&message),
        "consensus message on the wrong channel"
    );
    Ok(message)
}

/// A bounded PartSet for a signed proposal or a block with a verified quorum.
pub struct BlockParts {
    header: block::parts::Header,
    parts: Vec<Option<types::Part>>,
    count: usize,
    bytes: usize,
    max_bytes: usize,
    decoded: OnceLock<Result<ReceivedBlock, String>>,
}

impl BlockParts {
    pub fn availability(&self) -> BitArray {
        bit_array(
            self.parts.len(),
            self.parts
                .iter()
                .enumerate()
                .filter_map(|(index, part)| part.is_some().then_some(index)),
        )
    }

    pub fn new(header: block::parts::Header, max_bytes: usize) -> Result<Self> {
        let total = header.total as usize;
        ensure!(
            total > 0
                && total <= MAX_PARTS
                && total <= max_bytes.div_ceil(PART_SIZE)
                && header.hash != Hash::None,
            "invalid block part set header"
        );
        Ok(Self {
            header,
            parts: vec![None; total],
            count: 0,
            bytes: 0,
            max_bytes,
            decoded: OnceLock::new(),
        })
    }

    pub fn header(&self) -> block::parts::Header {
        self.header
    }

    pub fn insert(&mut self, part: types::Part) -> Result<bool> {
        let index = part.index as usize;
        ensure!(index < self.parts.len(), "block part index outside the set");
        if self.parts[index].is_some() {
            return Ok(false);
        }
        let bytes = part.bytes.len();
        ensure!(
            bytes > 0
                && bytes <= PART_SIZE
                && (index + 1 == self.parts.len() || bytes == PART_SIZE)
                && self.bytes + bytes <= self.max_bytes,
            "invalid consensus block part size"
        );
        ensure!(
            part_header(&part)? == self.header,
            "invalid block part proof root"
        );
        self.parts[index] = Some(part);
        self.count += 1;
        self.bytes += bytes;
        Ok(true)
    }

    pub fn block(&self) -> Result<Option<&ReceivedBlock>> {
        if self.count != self.parts.len() {
            return Ok(None);
        }
        self.decoded
            .get_or_init(|| self.decode_block().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map(Some)
            .map_err(|error| anyhow::anyhow!("invalid proposal block: {error}"))
    }

    fn decode_block(&self) -> Result<ReceivedBlock> {
        let mut raw = Vec::with_capacity(self.bytes);
        for part in &self.parts {
            raw.extend_from_slice(&part.as_ref().context("missing block part")?.bytes);
        }
        let proto = types::Block::decode(raw.as_slice())?;
        ensure!(
            proto.encode_to_vec() == raw,
            "noncanonical Tendermint 0.34 block"
        );
        let block = ReceivedBlock::from_proto(proto)?;
        ensure!(
            block.id().part_set_header == self.header,
            "block parts differ from the decoded block"
        );
        Ok(block)
    }

    pub fn part(&self, index: usize) -> Option<&types::Part> {
        self.parts.get(index)?.as_ref()
    }

    /// Produce the 64 KiB parts and RFC 6962 proofs for a Tendermint block.
    pub fn from_block(block: ReceivedBlock) -> Result<Self> {
        let bytes = block.encoded_block();
        let chunks = bytes.chunks(PART_SIZE).collect::<Vec<_>>();
        let mut hasher = Sha256::default();
        let leaves = chunks
            .iter()
            .map(|chunk| hasher.leaf_hash(chunk))
            .collect::<Vec<_>>();
        let mut paths = vec![Vec::new(); leaves.len()];
        let root = make_proofs(&mut hasher, &leaves, &mut paths);
        let header = block.id().part_set_header;
        ensure!(
            header.hash == Hash::Sha256(root) && header.total as usize == chunks.len(),
            "serialized block differs from its part set"
        );
        let parts = chunks
            .into_iter()
            .zip(paths)
            .enumerate()
            .map(|(index, (bytes, aunts))| {
                Some(types::Part {
                    index: index as u32,
                    bytes: bytes.to_vec(),
                    proof: Some(tendermint_proto::v0_34::crypto::Proof {
                        total: i64::from(header.total),
                        index: index as i64,
                        leaf_hash: leaves[index].to_vec(),
                        aunts,
                    }),
                })
            })
            .collect();
        Ok(Self {
            header,
            parts,
            count: leaves.len(),
            bytes: bytes.len(),
            max_bytes: bytes.len(),
            decoded: OnceLock::from(Ok(block)),
        })
    }
}

fn make_proofs(
    hasher: &mut Sha256,
    leaves: &[merkle::Hash],
    paths: &mut [Vec<Vec<u8>>],
) -> merkle::Hash {
    if leaves.len() == 1 {
        return leaves[0];
    }
    let split = leaves.len().next_power_of_two() / 2;
    let (left, right) = paths.split_at_mut(split);
    let left_root = make_proofs(hasher, &leaves[..split], left);
    let right_root = make_proofs(hasher, &leaves[split..], right);
    for path in left {
        path.push(right_root.to_vec());
    }
    for path in right {
        path.push(left_root.to_vec());
    }
    hasher.inner_hash(left_root, right_root)
}

/// Derive the claimed PartSet header when catch-up parts arrive before votes.
pub fn part_header(part: &types::Part) -> Result<block::parts::Header> {
    let proof = part.proof.as_ref().context("missing block part proof")?;
    let total = usize::try_from(proof.total).context("invalid block part count")?;
    let index = part.index as usize;
    ensure!(
        (1..=MAX_PARTS).contains(&total)
            && index < total
            && proof.index == i64::from(part.index)
            && proof.aunts.len() <= 100
            && (1..=PART_SIZE).contains(&part.bytes.len()),
        "invalid block part proof metadata"
    );
    let mut hasher = Sha256::default();
    let leaf = hasher.leaf_hash(&part.bytes);
    ensure!(proof.leaf_hash == leaf, "invalid block part leaf hash");
    let root = proof_root(&mut hasher, total, index, leaf, &proof.aunts)?;
    Ok(block::parts::Header::new(
        total.try_into()?,
        Hash::Sha256(root),
    )?)
}

fn proof_root(
    hasher: &mut Sha256,
    total: usize,
    index: usize,
    leaf: merkle::Hash,
    aunts: &[Vec<u8>],
) -> Result<merkle::Hash> {
    if total == 1 {
        ensure!(aunts.is_empty(), "unexpected extra block proof aunts");
        return Ok(leaf);
    }
    let (sibling, lower) = aunts.split_last().context("short block proof")?;
    let sibling: merkle::Hash = sibling
        .as_slice()
        .try_into()
        .context("invalid block proof aunt")?;
    let split = total.next_power_of_two() / 2;
    if index < split {
        let left = proof_root(hasher, split, index, leaf, lower)?;
        Ok(hasher.inner_hash(left, sibling))
    } else {
        let right = proof_root(hasher, total - split, index - split, leaf, lower)?;
        Ok(hasher.inner_hash(sibling, right))
    }
}
