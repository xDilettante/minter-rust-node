//! Collect Tendermint proposals, block parts and authenticated votes at the chain head.

use super::{AppliedBlock, NativeNode, proposer};
use crate::validator_signer::{self, ValidatorSigner, parse_proposal, parse_vote};
use anyhow::{Context, Result, ensure};
use minter_p2p::{
    ReceivedBlock,
    consensus::{
        BlockParts, EvidenceKey, Gossip, PendingEvidence, Reactor, bit_array, part_header,
    },
    evidence as evidence_wire, verify,
};
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tendermint::{
    Time, account,
    block::{self, Commit, CommitSig},
    chain,
    crypto::Sha256,
    node,
    validator::Set,
    vote::{SignedVote, Type, Vote},
};
use tendermint_proto::v0_34::{
    consensus::{self as cs, message::Sum},
    types::{self, SignedMsgType},
    version,
};

const TRACKED_BLOCKS: usize = 4;
const MAX_CATCHUP_ROUNDS: usize = 2;
const MAX_CONSENSUS_PEERS: usize = 32;
const NEW_HEIGHT: u32 = 1;
const PROPOSE: u32 = 3;
const PREVOTE: u32 = 4;
const PREVOTE_WAIT: u32 = 5;
const PRECOMMIT: u32 = 6;
const PRECOMMIT_WAIT: u32 = 7;
const COMMIT_DELAY: Duration = Duration::from_millis(500);
const GOSSIP_PARTS: usize = 16;
const MAX_VOTE_CLAIMS: usize = 16;

#[derive(Clone)]
pub struct NativeConsensus {
    node: NativeNode,
    round: Arc<Mutex<Round>>,
    signer: Arc<Mutex<Option<ValidatorSigner>>>,
    signing: bool,
}

struct Round {
    height: i64,
    validators: Set,
    proposer_validators: Set,
    chain: chain::Id,
    last_commit_round: i32,
    max_block_bytes: usize,
    number: i32,
    step: u32,
    deadline: Instant,
    time_iota: Duration,
    locked: Option<(i32, block::Id, Time)>,
    last_commit: Option<Commit>,
    last_parts: Option<(Commit, BlockParts)>,
    proposals: BTreeMap<i32, types::Proposal>,
    blocks: BTreeMap<block::parts::Header, BlockParts>,
    provisional: Option<BlockParts>,
    votes: BTreeMap<i32, RoundVotes>,
    peers: BTreeMap<node::Id, PeerRounds>,
    failed: bool,
}

#[derive(Default)]
struct PeerRounds {
    connections: usize,
    rounds: BTreeSet<i32>,
}

#[derive(Default)]
struct RoundVotes {
    local: bool,
    prevotes: Votes,
    precommits: Votes,
}

#[derive(Default)]
struct Votes {
    by_index: BTreeMap<usize, Vote>,
    second: BTreeMap<usize, Vote>,
    extra: BTreeMap<(Option<block::Id>, usize), Vote>,
    claimed: BTreeMap<node::Id, Option<block::Id>>,
    claimed_blocks: BTreeSet<Option<block::Id>>,
    power: BTreeMap<Option<block::Id>, u64>,
    quorum: Option<Option<block::Id>>,
    total_seen: u64,
}

#[derive(Clone, PartialEq, Message)]
struct SavedRound {
    #[prost(int64, tag = "1")]
    height: i64,
    #[prost(int32, tag = "2")]
    round: i32,
    #[prost(uint32, tag = "3")]
    step: u32,
    #[prost(message, optional, tag = "4")]
    proposal: Option<types::Proposal>,
    #[prost(bytes, tag = "5")]
    proposal_block: Vec<u8>,
    #[prost(message, optional, tag = "6")]
    locked_block: Option<types::BlockId>,
    #[prost(message, optional, tag = "7")]
    locked_time: Option<tendermint_proto::google::protobuf::Timestamp>,
    #[prost(int32, tag = "8")]
    locked_round: i32,
    #[prost(bytes, tag = "9")]
    locked_block_bytes: Vec<u8>,
    #[prost(message, repeated, tag = "10")]
    prevote_certificates: Vec<recovery::SavedPrevotes>,
}

impl NativeConsensus {
    pub fn new(node: NativeNode, signer: Option<ValidatorSigner>) -> Result<Self> {
        node.lock()?.ensure_history_context()?;
        let mut round = Self::next_round(&node)?;
        if let Some(signer) = &signer {
            ensure!(
                signer.chain_id() == &round.chain,
                "validator chain ID differs from the node"
            );
            round.step = NEW_HEIGHT;
            round.deadline = Instant::now() + COMMIT_DELAY;
            round.recover(signer)?;
        }
        Ok(Self {
            node,
            round: Arc::new(Mutex::new(round)),
            signing: signer.is_some(),
            signer: Arc::new(Mutex::new(signer)),
        })
    }

    fn next_round(node: &NativeNode) -> Result<Round> {
        let sync = node.lock()?;
        let cursor = sync.cursor()?;
        let last_commit: Option<Commit> = sync
            .app
            .store
            .native_block_at(cursor.height)?
            .map(|bytes| {
                super::BlockRecord::decode(bytes.as_slice())
                    .map(|record| record.commit.context("missing stored commit"))?
                    .and_then(|commit| Ok(commit.try_into()?))
            })
            .transpose()?;
        ensure!(
            cursor.has_proposer_state(),
            "live consensus requires verified proposer priorities"
        );
        Ok(Round {
            height: cursor
                .height
                .checked_add(1)
                .context("consensus height overflow")?
                .try_into()?,
            validators: cursor.validators.clone(),
            proposer_validators: cursor.validators.clone(),
            chain: sync.chain_id().clone(),
            last_commit_round: last_commit
                .as_ref()
                .map_or(-1, |commit| commit.round.into()),
            max_block_bytes: cursor.params.block.max_bytes.try_into()?,
            number: 0,
            step: PROPOSE,
            deadline: Instant::now(),
            time_iota: Duration::from_millis(cursor.params.block.time_iota_ms.try_into()?),
            locked: None,
            last_commit,
            last_parts: None,
            proposals: BTreeMap::new(),
            blocks: BTreeMap::new(),
            provisional: None,
            votes: BTreeMap::from([
                (
                    0,
                    RoundVotes {
                        local: true,
                        ..Default::default()
                    },
                ),
                (
                    1,
                    RoundVotes {
                        local: true,
                        ..Default::default()
                    },
                ),
            ]),
            peers: BTreeMap::new(),
            failed: false,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Round>> {
        self.round
            .lock()
            .map_err(|_| anyhow::anyhow!("consensus round lock"))
    }

    pub fn tick(&self) -> Result<()> {
        let mut round = self.lock()?;
        if round.failed {
            anyhow::bail!("native consensus stopped after a signing or commit failure");
        }
        if !self.signing {
            return Ok(());
        }
        self.advance_local(&mut round)?;
        let number = round.number;
        self.publish_commit(&mut round, number)?;
        Ok(())
    }

    fn advance_local(&self, round: &mut Round) -> Result<()> {
        let mut signer = self
            .signer
            .lock()
            .map_err(|_| anyhow::anyhow!("consensus signer lock"))?;
        let Some(signer) = signer.as_mut() else {
            return Ok(());
        };
        if let Some(index) = round.local_validator(signer)
            && let Err(error) = self.advance(round, signer, index)
        {
            round.failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn receive_last_precommit(&self, round: &mut Round, wire: types::Vote) -> Result<()> {
        let Some(commit) = round.last_commit.as_ref().filter(|commit| {
            wire.r#type == SignedMsgType::Precommit as i32
                && wire.height == i64::from(commit.height)
                && wire.round == i32::from(commit.round)
        }) else {
            return Ok(());
        };
        let mut node = self.node.lock()?;
        let previous = node
            .cursor()?
            .previous
            .as_ref()
            .context("missing previous validator set")?;
        ensure!(
            previous.id == commit.block_id,
            "previous commit differs from the chain head"
        );
        let index = usize::try_from(wire.validator_index)?;
        let validator = previous
            .validators
            .validators
            .get(index)
            .context("invalid previous validator index")?;
        let vote = parse_vote(wire, validator.address)?;
        let signed = SignedVote::from_vote(vote.clone(), round.chain.clone())
            .context("unsigned previous precommit")?;
        validator
            .verify_signature::<verify::GoEd25519>(&signed.sign_bytes(), signed.signature())?;
        if !commit
            .signatures
            .get(index)
            .is_some_and(CommitSig::is_absent)
            || vote.block_id.is_some_and(|id| id != commit.block_id)
        {
            return Ok(());
        }
        let mut updated = commit.clone();
        let timestamp = vote
            .timestamp
            .context("previous precommit has no timestamp")?;
        updated.signatures[index] = if vote.block_id.is_some() {
            CommitSig::BlockIdFlagCommit {
                validator_address: vote.validator_address,
                timestamp,
                signature: vote.signature,
            }
        } else {
            CommitSig::BlockIdFlagNil {
                validator_address: vote.validator_address,
                timestamp,
                signature: vote.signature,
            }
        };
        verify::verify_commit(
            &round.chain,
            updated.height,
            updated.block_id,
            &previous.validators,
            &updated,
        )?;
        let height = updated.height.value();
        let raw = node
            .app
            .store
            .native_block_at(height)?
            .context("missing previous block record")?;
        let mut record = super::BlockRecord::decode(raw.as_slice())?;
        record.commit = Some(updated.clone().into());
        if let Err(error) =
            node.app
                .store
                .update_native_block_record(height, &raw, &record.encode_to_vec())
        {
            round.failed = true;
            return Err(error).context("persist late precommit");
        }
        if let Some((commit, _)) = &mut round.last_parts
            && commit.height == updated.height
        {
            *commit = updated.clone();
        }
        round.last_commit = Some(updated);
        Ok(())
    }

    fn advance(&self, round: &mut Round, signer: &mut ValidatorSigner, index: usize) -> Result<()> {
        let now = Instant::now();
        if round.step == NEW_HEIGHT && now >= round.deadline {
            round.enter_propose(0, now)?;
        }
        if round.step == NEW_HEIGHT {
            return Ok(());
        }
        let number = round.number;
        match round.step {
            PROPOSE => {
                let selected = round.selected_proposer(number)?;
                if selected == account::Id::from(signer.public_key())
                    && !round.proposals.contains_key(&number)
                {
                    self.make_proposal(round, signer, selected)?;
                }
                let candidate = self.valid_proposal(round, number)?;
                if candidate.is_some() || now >= round.deadline {
                    if let Some((locked_round, locked, _)) = round.locked
                        && round.votes.iter().any(|(&n, votes)| {
                            n > locked_round
                                && n <= number
                                && votes.prevotes.quorum.is_some_and(|id| id != Some(locked))
                        })
                    {
                        round.locked = None;
                    }
                    let selected = round.locked.map(|(_, id, time)| (id, time)).or(candidate);
                    round.sign_vote(signer, index, Type::Prevote, selected)?;
                    round.step = PREVOTE;
                }
            }
            PREVOTE | PREVOTE_WAIT => {
                let quorum = round
                    .votes
                    .get(&number)
                    .and_then(|votes| votes.prevotes.quorum);
                let candidate = if let Some(Some(id)) = quorum {
                    if let Some((_, locked, time)) =
                        round.locked.filter(|(_, locked, _)| *locked == id)
                    {
                        Some((locked, time))
                    } else {
                        self.valid_block(round, id)?
                    }
                } else {
                    None
                };
                if quorum == Some(None)
                    || candidate.is_some()
                    || (round.step == PREVOTE_WAIT && now >= round.deadline)
                {
                    if quorum.is_some() {
                        round.locked = candidate.map(|(id, time)| (number, id, time));
                    }
                    round.sign_vote(signer, index, Type::Precommit, candidate)?;
                    round.step = PRECOMMIT;
                } else if round.step == PREVOTE && round.has_two_thirds(number, Type::Prevote) {
                    round.step = PREVOTE_WAIT;
                    round.deadline = now + Duration::from_millis(1000 + 500 * number as u64);
                }
            }
            PRECOMMIT | PRECOMMIT_WAIT => {
                if round.step == PRECOMMIT_WAIT && now >= round.deadline {
                    round.enter_propose(
                        number.checked_add(1).context("consensus round exhausted")?,
                        now,
                    )?;
                } else if round.step == PRECOMMIT
                    && round.has_two_thirds(number, Type::Precommit)
                    && round.quorum_block(number).is_none()
                {
                    round.step = PRECOMMIT_WAIT;
                    round.deadline = now + Duration::from_millis(1000 + 500 * number as u64);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn valid_proposal(&self, round: &Round, number: i32) -> Result<Option<(block::Id, Time)>> {
        let Some(proposal) = round.proposals.get(&number) else {
            return Ok(None);
        };
        let Some(id) = proposal
            .block_id
            .clone()
            .map(block::Id::try_from)
            .transpose()?
        else {
            return Ok(None);
        };
        if proposal.pol_round >= 0
            && round
                .votes
                .get(&proposal.pol_round)
                .and_then(|votes| votes.prevotes.quorum)
                != Some(Some(id))
        {
            return Ok(None);
        }
        self.valid_block(round, id)
    }

    fn valid_block(&self, round: &Round, id: block::Id) -> Result<Option<(block::Id, Time)>> {
        let Some(parts) = round.parts(id.part_set_header) else {
            return Ok(None);
        };
        let Ok(Some(block)) = parts.block() else {
            return Ok(None);
        };
        if block.id() != id {
            return Ok(None);
        }
        let node = self.node.lock()?;
        let cursor = node.cursor()?;
        if node
            .validate_candidate(cursor, block)
            .and_then(|()| node.validate_live_evidence(cursor, block))
            .is_err()
        {
            return Ok(None);
        }
        Ok(Some((id, block.block().header.time)))
    }

    fn make_proposal(
        &self,
        round: &mut Round,
        signer: &mut ValidatorSigner,
        address: account::Id,
    ) -> Result<()> {
        let (fresh, id, pol_round) = if let Some((valid, id)) = round.best_valid_block()
            && self.valid_block(round, id)?.is_some()
        {
            (None, id, valid)
        } else {
            let sync = self.node.lock()?;
            let cursor = sync.cursor()?;
            let last = round.last_commit.clone().unwrap_or_default();
            let time = if let Some(previous) = &cursor.previous {
                verify::median_time(&last, &previous.validators)?
            } else {
                cursor.genesis_time.context("missing genesis time")?
            };
            // Go MaxDataBytes includes the evidence list and current validator count.
            let data_capacity = round.max_block_bytes.saturating_sub(
                731 + 111
                    * cursor
                        .validators
                        .validators
                        .len()
                        .max(last.signatures.len()),
            );
            let evidence = sync
                .evidence
                .proposed(data_capacity.min(cursor.params.evidence.max_bytes.try_into()?));
            let txs = sync.mempool.as_ref().map_or_else(Vec::new, |pool| {
                pool.proposed(
                    data_capacity.saturating_sub(evidence.encoded_len()),
                    cursor.params.block.max_gas,
                )
            });
            let tx_ids = txs.iter().map(super::Hasher::digest).collect::<Vec<_>>();
            let data_hash =
                tendermint::merkle::simple_hash_from_byte_vectors::<super::Hasher>(&tx_ids);
            let evidence_payloads = evidence
                .evidence
                .iter()
                .map(evidence_wire::payload)
                .collect::<Result<Vec<_>>>()?;
            let evidence_hash = tendermint::merkle::simple_hash_from_byte_vectors::<super::Hasher>(
                &evidence_payloads,
            );
            let header = types::Header {
                version: Some(version::Consensus {
                    block: 11,
                    app: cursor
                        .app_version
                        .unwrap_or_else(|| super::params_app_version(&cursor.params)),
                }),
                chain_id: round.chain.to_string(),
                height: round.height,
                time: Some(time.into()),
                last_block_id: Some(
                    cursor
                        .previous
                        .as_ref()
                        .map_or_else(block::Id::default, |block| block.id)
                        .into(),
                ),
                last_commit_hash: verify::commit_hash(round.last_commit.as_ref())
                    .as_bytes()
                    .to_vec(),
                data_hash: data_hash.to_vec(),
                validators_hash: cursor.validators.hash().as_bytes().to_vec(),
                next_validators_hash: cursor.next_validators.hash().as_bytes().to_vec(),
                consensus_hash: super::consensus_hash(&cursor.params)?.as_bytes().to_vec(),
                app_hash: cursor
                    .previous
                    .as_ref()
                    .map_or_else(|| cursor.genesis_app_hash.clone(), |_| cursor.root.to_vec()),
                last_results_hash: cursor.results_hash.as_bytes().to_vec(),
                evidence_hash: evidence_hash.to_vec(),
                proposer_address: address.as_bytes().to_vec(),
            };
            let block = ReceivedBlock::from_proto(types::Block {
                header: Some(header),
                data: Some(types::Data { txs }),
                evidence: Some(evidence),
                last_commit: Some(last.into()),
            })?;
            sync.validate_candidate(cursor, &block)?;
            let id = block.id();
            (Some(BlockParts::from_block(block)?), id, -1)
        };
        let mut proposal = types::Proposal {
            r#type: SignedMsgType::Proposal as i32,
            height: round.height,
            round: round.number,
            pol_round,
            block_id: Some(id.into()),
            timestamp: Some(Time::now().into()),
            signature: Vec::new(),
        };
        let parts = fresh
            .as_ref()
            .or_else(|| round.parts(id.part_set_header))
            .context("missing local proposal parts")?;
        let block = parts.block()?.context("incomplete local proposal")?;
        let saved = round.saved(PROPOSE, Some(&proposal), Some(block));
        signer.sign_proposal_with_round(&mut proposal, Some(&saved))?;
        round.proposals.insert(round.number, proposal);
        if let Some(parts) = fresh {
            round.set_parts(parts);
        }
        println!(
            "Signed Tendermint proposal at {}/{}",
            round.height, round.number
        );
        Ok(())
    }

    fn publish_commit(&self, round: &mut Round, number: i32) -> Result<()> {
        match self.try_commit(round, number) {
            Ok(Some(applied)) => println!("{}", serde_json::to_string(&applied)?),
            Ok(None) => {}
            Err(error) => {
                round.failed = true;
                return Err(error);
            }
        }
        Ok(())
    }

    fn try_commit(&self, round: &mut Round, number: i32) -> Result<Option<AppliedBlock>> {
        let Some(votes) = round.votes.get(&number) else {
            return Ok(None);
        };
        let Some(Some(id)) = votes.precommits.quorum else {
            return Ok(None);
        };
        let Some(parts) = round.parts(id.part_set_header) else {
            return Ok(None);
        };
        let Some(block) = parts.block()? else {
            return Ok(None);
        };
        ensure!(block.id() == id, "precommit quorum names another block");
        let commit = votes.precommits.commit(round, number, id)?;
        let detected = round.conflicting_evidence(block.block().header.time)?;
        let result = {
            let mut node = self.node.lock()?;
            let cursor = node.cursor()?;
            node.validate_candidate(cursor, block)?;
            node.validate_live_evidence(cursor, block)?;
            verify::verify_commit(
                &round.chain,
                block.block().header.height,
                block.id(),
                &round.validators,
                &commit,
            )?;
            let result = node.execute_committed(block, &commit, None, detected);
            if result.is_err() {
                node.app.pending = None;
            }
            result?
        };
        let mut next = Self::next_round(&self.node)?;
        next.peers = std::mem::take(&mut round.peers);
        for peer in next.peers.values_mut() {
            peer.rounds.clear();
        }
        next.last_parts = round
            .blocks
            .remove(&id.part_set_header)
            .map(|parts| (commit, parts));
        if self.signing {
            next.step = NEW_HEIGHT;
            next.deadline = Instant::now() + COMMIT_DELAY;
        }
        *round = next;
        Ok(Some(result))
    }
}

impl Reactor for NativeConsensus {
    fn peer_connected(&self, peer: node::Id) -> Result<()> {
        let mut round = self.lock()?;
        ensure!(
            round.peers.contains_key(&peer) || round.peers.len() < MAX_CONSENSUS_PEERS,
            "too many native consensus peers"
        );
        round.peers.entry(peer).or_default().connections += 1;
        Ok(())
    }

    fn peer_disconnected(&self, peer: node::Id) {
        if let Ok(mut round) = self.lock() {
            if let Some(state) = round.peers.get_mut(&peer)
                && state.connections > 1
            {
                state.connections -= 1;
                return;
            }
            round.peers.remove(&peer);
            for votes in round.votes.values_mut() {
                votes.prevotes.claimed.remove(&peer);
                votes.precommits.claimed.remove(&peer);
            }
            round.release_unowned_rounds();
        }
    }

    fn receive_evidence(&self, message: types::EvidenceList) -> Result<()> {
        let mut node = self.node.lock()?;
        for item in message.evidence {
            node.receive_evidence(item)?;
        }
        Ok(())
    }

    fn pending_evidence(
        &self,
        peer_height: u64,
        after: Option<EvidenceKey>,
    ) -> Result<Option<PendingEvidence>> {
        let node = self.node.lock()?;
        let max_age = node.cursor()?.params.evidence.max_age_num_blocks;
        Ok(node.evidence.for_peer(peer_height, max_age, after))
    }

    fn round_step(&self) -> Result<cs::NewRoundStep> {
        let round = self.lock()?;
        Ok(cs::NewRoundStep {
            height: round.height,
            round: round.number,
            step: round.step,
            seconds_since_start_time: 0,
            last_commit_round: round.last_commit_round,
        })
    }

    fn receive(&self, peer: node::Id, message: Sum) -> Result<Option<Sum>> {
        let mut round = self.lock()?;
        if round.failed {
            anyhow::bail!("native consensus stopped after a signing or commit failure");
        }
        let affected = match message {
            Sum::VoteSetMaj23(claim) => return round.vote_claim(peer, claim),
            Sum::Proposal(message) => {
                let proposal = message.proposal.context("missing consensus proposal")?;
                let number = proposal.round;
                if proposal.height != round.height || number != round.number {
                    return Ok(None);
                }
                round.receive_proposal(proposal)?;
                Some(number)
            }
            Sum::BlockPart(message) => {
                if message.height != round.height || !round.tracks(message.round) {
                    return Ok(None);
                }
                round.receive_part(message.round, message.part.context("missing block part")?)?;
                Some(message.round)
            }
            Sum::Vote(message) => {
                let vote = message.vote.context("missing consensus vote")?;
                if vote.height == round.height - 1 {
                    self.receive_last_precommit(&mut round, vote)?;
                    return Ok(None);
                }
                let number = vote.round;
                if vote.height != round.height || number < 0 {
                    return Ok(None);
                }
                round.receive_vote(vote, Some(peer))?;
                if number >= round.number
                    && round.has_two_thirds(number, Type::Precommit)
                    && round.quorum_block(number).is_none()
                    && round.step != PRECOMMIT_WAIT
                {
                    round.enter_propose(number, Instant::now())?;
                    round.step = PRECOMMIT_WAIT;
                    round.deadline =
                        Instant::now() + Duration::from_millis(1000 + 500 * number as u64);
                } else if number > round.number && round.has_two_thirds(number, Type::Prevote) {
                    round.enter_propose(number, Instant::now())?;
                }
                Some(number)
            }
            _ => None,
        };
        if affected.is_some() {
            if self.signing {
                self.advance_local(&mut round)?;
            }
            let certified = round
                .votes
                .iter()
                .find_map(|(&number, votes)| votes.precommits.quorum.flatten().map(|_| number));
            if let Some(number) = certified {
                self.publish_commit(&mut round, number)?;
            }
        }
        Ok(None)
    }

    fn pending(&self, peer: &cs::NewRoundStep, gossip: &mut Gossip) -> Result<Vec<Sum>> {
        let mut round = self.lock()?;
        let mut messages = Vec::new();
        if peer.height == round.height {
            if peer.step == NEW_HEIGHT
                && let Some(commit) = &round.last_commit
            {
                messages.extend(commit.signatures.iter().enumerate().filter_map(
                    |(index, signature)| {
                        commit_vote(index, signature, commit)
                            .map(|vote| Sum::Vote(cs::Vote { vote: Some(vote) }))
                    },
                ));
            }
            if let Some(proposal) = round.proposals.get(&peer.round).filter(|_| self.signing) {
                messages.push(Sum::Proposal(cs::Proposal {
                    proposal: Some(proposal.clone()),
                }));
                if proposal.pol_round >= 0
                    && let Some(votes) = round.votes.get(&proposal.pol_round)
                {
                    messages.extend(votes.prevotes.gossip(
                        round.height,
                        proposal.pol_round,
                        Type::Prevote,
                    ));
                }
            }
            if let Some(parts) = round
                .block_for_round(peer.round)
                .and_then(|id| round.parts(id.part_set_header))
            {
                let committed = round.quorum_block(peer.round).is_some();
                if committed
                    || round
                        .votes
                        .get(&peer.round)
                        .and_then(|votes| votes.prevotes.quorum)
                        .flatten()
                        .is_some()
                {
                    messages.push(valid_block_message(
                        peer.height,
                        peer.round,
                        parts,
                        committed,
                    ));
                }
                if self.signing {
                    messages.extend(gossip.parts(peer.height, peer.round, parts, GOSSIP_PARTS));
                }
            }
            if let Some((number, id)) = round
                .certified_block()
                .filter(|(number, _)| *number != peer.round)
                && let Some(parts) = round.parts(id.part_set_header)
            {
                messages.push(valid_block_message(peer.height, number, parts, true));
            }
            if let Some(votes) = round.votes.get(&peer.round) {
                if self.signing {
                    messages.extend(
                        votes
                            .prevotes
                            .gossip(round.height, peer.round, Type::Prevote),
                    );
                    messages.extend(votes.precommits.gossip(
                        round.height,
                        peer.round,
                        Type::Precommit,
                    ));
                } else {
                    messages.extend(votes.prevotes.majority(
                        round.height,
                        peer.round,
                        Type::Prevote,
                    ));
                    messages.extend(votes.precommits.majority(
                        round.height,
                        peer.round,
                        Type::Precommit,
                    ));
                }
            }
        } else if peer.height > 0 && peer.height < round.height {
            if round
                .last_parts
                .as_ref()
                .is_none_or(|(commit, _)| i64::from(commit.height) != peer.height)
            {
                let stored = self
                    .node
                    .lock()?
                    .app
                    .store
                    .native_block_at(peer.height.try_into()?)?;
                round.last_parts = stored
                    .map(|raw| -> Result<_> {
                        let record = super::BlockRecord::decode(raw.as_slice())?;
                        let block = ReceivedBlock::decode(record.block_response)?;
                        let commit: Commit = record
                            .commit
                            .context("missing catch-up commit")?
                            .try_into()?;
                        ensure!(
                            i64::from(commit.height) == peer.height
                                && commit.block_id == block.id(),
                            "stored catch-up commit differs from block"
                        );
                        Ok((commit, BlockParts::from_block(block)?))
                    })
                    .transpose()?;
            }
            let Some((commit, parts)) = &round.last_parts else {
                return Ok(messages);
            };
            messages.push(majority_message(
                peer.height,
                commit.round.into(),
                Type::Precommit,
                Some(commit.block_id),
            ));
            messages.extend(commit.signatures.iter().enumerate().filter_map(
                |(index, signature)| {
                    commit_vote(index, signature, commit)
                        .map(|vote| Sum::Vote(cs::Vote { vote: Some(vote) }))
                },
            ));
            messages.push(valid_block_message(
                peer.height,
                commit.round.into(),
                parts,
                true,
            ));
            messages.extend(gossip.parts(peer.height, peer.round, parts, GOSSIP_PARTS));
        }
        Ok(messages)
    }
}

impl Round {
    fn vote_claim(&mut self, peer: node::Id, claim: cs::VoteSetMaj23) -> Result<Option<Sum>> {
        if claim.height != self.height || !self.tracks(claim.round) {
            return Ok(None);
        }
        let kind = Type::try_from(claim.r#type)?;
        let id = evidence_wire::block_id(claim.block_id)?;
        let known =
            claim.round <= self.number.saturating_add(1) || self.votes.contains_key(&claim.round);
        let votes = if known {
            let round = self.votes.entry(claim.round).or_default();
            let pool = match kind {
                Type::Prevote => &mut round.prevotes,
                Type::Precommit => &mut round.precommits,
            };
            pool.claim(peer, id)?;
            bit_array(
                self.validators.validators.len(),
                pool.by_index
                    .keys()
                    .copied()
                    .filter(|&index| pool.for_block(index, id).is_some()),
            )
        } else {
            bit_array(0, [])
        };
        Ok(Some(Sum::VoteSetBits(cs::VoteSetBits {
            height: claim.height,
            round: claim.round,
            r#type: claim.r#type,
            block_id: Some(id.unwrap_or_default().into()),
            votes: Some(votes),
        })))
    }

    fn local_validator(&self, signer: &ValidatorSigner) -> Option<usize> {
        let address = account::Id::from(signer.public_key());
        self.validators
            .validators
            .iter()
            .position(|v| v.address == address)
    }

    fn parts(&self, header: block::parts::Header) -> Option<&BlockParts> {
        self.blocks.get(&header)
    }

    fn enter_propose(&mut self, number: i32, now: Instant) -> Result<()> {
        ensure!(
            number >= self.number && number >= 0,
            "consensus round regressed"
        );
        if number > self.number {
            proposer::increment(&mut self.proposer_validators, (number - self.number) as u32)?;
        }
        self.number = number;
        self.step = PROPOSE;
        self.deadline = now + Duration::from_millis(3000 + 500 * number as u64);
        self.votes.entry(number).or_default().local = true;
        if let Some(next) = number.checked_add(1) {
            self.votes.entry(next).or_default().local = true;
        }
        let local: BTreeSet<_> = self
            .votes
            .iter()
            .filter_map(|(&n, votes)| votes.local.then_some(n))
            .collect();
        for peer in self.peers.values_mut() {
            peer.rounds.retain(|r| !local.contains(r));
        }
        Ok(())
    }

    fn selected_proposer(&self, number: i32) -> Result<account::Id> {
        ensure!(
            number == self.number,
            "requested proposer outside the current round"
        );
        Ok(self
            .proposer_validators
            .proposer
            .as_ref()
            .context("missing round proposer")?
            .address)
    }

    fn has_two_thirds(&self, number: i32, kind: Type) -> bool {
        self.votes.get(&number).is_some_and(|votes| {
            let votes = match kind {
                Type::Prevote => &votes.prevotes,
                Type::Precommit => &votes.precommits,
            };
            votes.total_seen * 3 > self.validators.total_voting_power.value() * 2
        })
    }

    fn best_valid_block(&self) -> Option<(i32, block::Id)> {
        for (&number, votes) in self.votes.range(..self.number).rev() {
            if let Some(Some(id)) = votes.prevotes.quorum
                && let Some(parts) = self.parts(id.part_set_header)
                && matches!(parts.block(), Ok(Some(block)) if block.id() == id)
            {
                return Some((number, id));
            }
        }
        None
    }

    fn sign_vote(
        &mut self,
        signer: &mut ValidatorSigner,
        index: usize,
        kind: Type,
        candidate: Option<(block::Id, Time)>,
    ) -> Result<()> {
        let time = self
            .locked
            .map(|(_, _, time)| time)
            .or_else(|| candidate.map(|(_, time)| time));
        let now = Time::now();
        let timestamp = if let Some(time) = time {
            now.max(
                time.checked_add(self.time_iota)
                    .context("vote time overflow")?,
            )
        } else {
            now
        };
        let mut wire = types::Vote {
            r#type: kind.into(),
            height: self.height,
            round: self.number,
            block_id: Some(
                candidate
                    .map_or_else(block::Id::default, |(id, _)| id)
                    .into(),
            ),
            timestamp: Some(timestamp.into()),
            validator_address: self.validators.validators[index]
                .address
                .as_bytes()
                .to_vec(),
            validator_index: index.try_into()?,
            signature: Vec::new(),
        };
        let step = if kind == Type::Prevote {
            PREVOTE
        } else {
            PRECOMMIT
        };
        let proposal = self.proposals.get(&self.number);
        let proposed_block = proposal
            .and_then(|proposal| proposal.block_id.clone())
            .and_then(|id| block::Id::try_from(id).ok())
            .and_then(|id| {
                self.parts(id.part_set_header)?
                    .block()
                    .ok()
                    .flatten()
                    .filter(|block| block.id() == id)
            });
        let saved = self.saved(step, proposal, proposed_block);
        signer.sign_vote_with_round(&mut wire, Some(&saved))?;
        if self.number > 0 {
            println!(
                "{}",
                serde_json::json!({
                "event": "validator_vote_signed", "consensusHeight": self.height,
                    "round": self.number, "voteType": i32::from(kind),
                })
            );
        }
        self.receive_vote(wire, None)
    }

    fn tracks(&self, number: i32) -> bool {
        number >= 0
            && (number == self.number
                || number == self.number.saturating_add(1)
                || self.votes.contains_key(&number))
    }

    fn admit_round(&mut self, peer: node::Id, number: i32) -> bool {
        if self.votes.get(&number).is_some_and(|votes| votes.local) {
            return true;
        }
        if number < 0
            || (!self.peers.contains_key(&peer) && self.peers.len() >= MAX_CONSENSUS_PEERS)
        {
            return false;
        }
        let peer = self.peers.entry(peer).or_default();
        if peer.rounds.len() >= MAX_CATCHUP_ROUNDS {
            return self.votes.contains_key(&number);
        }
        peer.rounds.insert(number);
        true
    }

    fn release_unowned_rounds(&mut self) {
        let owned: BTreeSet<_> = self
            .peers
            .values()
            .flat_map(|peer| peer.rounds.iter().copied())
            .collect();
        let protected = [
            self.certified_block().map(|(number, _)| number),
            self.locked.map(|(number, _, _)| number),
            self.proposals
                .get(&self.number)
                .map(|p| p.pol_round)
                .filter(|r| *r >= 0),
            self.votes
                .iter()
                .rev()
                .find_map(|(&n, v)| v.prevotes.quorum.map(|_| n)),
            self.votes
                .iter()
                .rev()
                .find_map(|(&n, v)| v.prevotes.quorum.flatten().map(|_| n)),
        ];
        self.votes.retain(|number, votes| {
            votes.local || owned.contains(number) || protected.contains(&Some(*number))
        });
    }

    fn receive_proposal(&mut self, wire: types::Proposal) -> Result<()> {
        if self.proposals.contains_key(&wire.round) {
            return Ok(());
        }
        let proposal = parse_proposal(wire.clone())?;
        ensure!(
            wire.round == self.number,
            "proposal is outside the current round"
        );
        let proposer = self
            .proposer_validators
            .proposer
            .as_ref()
            .context("missing round proposer")?;
        let signature = proposal
            .signature
            .as_ref()
            .context("unsigned consensus proposal")?;
        let mut sign_bytes = Vec::new();
        proposal.to_signable_bytes(self.chain.clone(), &mut sign_bytes)?;
        proposer.verify_signature::<verify::GoEd25519>(&sign_bytes, signature)?;
        let block_id = proposal.block_id.context("missing proposal block ID")?;
        let number = wire.round;
        let preferred = self.quorum_block(number).unwrap_or(block_id);
        self.proposals.insert(number, wire);
        if let Err(error) = self.expect_parts(preferred.part_set_header) {
            self.proposals.remove(&number);
            return Err(error);
        }
        Ok(())
    }

    fn receive_part(&mut self, number: i32, part: types::Part) -> Result<()> {
        let header = part_header(&part)?;
        if !self.blocks.contains_key(&header)
            && self
                .block_for_round(number)
                .is_some_and(|id| id.part_set_header == header)
        {
            self.expect_parts(header)?;
        }
        if let Some(parts) = self.blocks.get_mut(&header) {
            parts.insert(part)?;
        } else if number == self.number {
            // Go may send catch-up parts before their precommits on another
            // channel. An untrusted header never replaces an authenticated set.
            if self
                .provisional
                .as_ref()
                .is_none_or(|parts| parts.header() != header)
            {
                self.provisional = Some(BlockParts::new(header, self.max_block_bytes)?);
            }
            if let Some(parts) = &mut self.provisional {
                parts.insert(part)?;
            }
        }
        Ok(())
    }

    fn expect_parts(&mut self, header: block::parts::Header) -> Result<()> {
        if self.blocks.contains_key(&header) {
            return Ok(());
        }
        let parts = if self
            .provisional
            .as_ref()
            .is_some_and(|parts| parts.header() == header)
        {
            self.provisional
                .take()
                .context("missing provisional part set")?
        } else {
            BlockParts::new(header, self.max_block_bytes)?
        };
        self.set_parts(parts);
        Ok(())
    }

    fn set_parts(&mut self, parts: BlockParts) {
        let header = parts.header();
        if self.blocks.len() >= TRACKED_BLOCKS && !self.blocks.contains_key(&header) {
            let keep = [
                self.locked.map(|(_, id, _)| id),
                self.block_for_round(self.number),
                self.votes
                    .values()
                    .find_map(|votes| votes.precommits.quorum.flatten()),
                self.best_valid_block().map(|(_, id)| id),
            ];
            let Some(old) = self.blocks.keys().copied().find(|header| {
                keep.iter()
                    .flatten()
                    .all(|id| id.part_set_header != *header)
            }) else {
                return;
            };
            self.blocks.remove(&old);
        }
        self.blocks.insert(header, parts);
    }

    fn block_for_round(&self, number: i32) -> Option<block::Id> {
        self.quorum_block(number)
            .or_else(|| self.votes.get(&number)?.prevotes.quorum.flatten())
            .or_else(|| {
                self.proposals
                    .get(&number)?
                    .block_id
                    .clone()?
                    .try_into()
                    .ok()
            })
    }

    fn quorum_block(&self, number: i32) -> Option<block::Id> {
        self.votes.get(&number)?.precommits.quorum.flatten()
    }

    fn certified_block(&self) -> Option<(i32, block::Id)> {
        self.votes
            .iter()
            .find_map(|(&number, votes)| votes.precommits.quorum.flatten().map(|id| (number, id)))
    }

    fn receive_vote(&mut self, wire: types::Vote, peer: Option<node::Id>) -> Result<()> {
        let index = usize::try_from(wire.validator_index)?;
        let validator = self
            .validators
            .validators
            .get(index)
            .context("invalid consensus validator index")?;
        let vote = parse_vote(wire, validator.address)?;
        let number = i32::from(vote.round);
        ensure!(
            i64::from(vote.height) == self.height,
            "vote height differs from the active height"
        );
        let vote = SignedVote::from_vote(vote.clone(), self.chain.clone())
            .context("unsigned consensus vote")
            .and_then(|signed| {
                validator.verify_signature::<verify::GoEd25519>(
                    &signed.sign_bytes(),
                    signed.signature(),
                )?;
                Ok(vote)
            })?;
        let power = validator.power();
        let total = self.validators.total_voting_power.value();
        if peer.is_some_and(|peer| !self.admit_round(peer, number)) {
            return Ok(());
        }
        let ballots = self.votes.entry(number).or_default();
        let pool = match vote.vote_type {
            Type::Prevote => &mut ballots.prevotes,
            Type::Precommit => &mut ballots.precommits,
        };
        if pool.insert(index, vote, power, total) {
            println!(
                "Observed conflicting Tendermint votes at {}/{number}",
                self.height
            );
        }
        if let Some(block) = self
            .quorum_block(number)
            .or_else(|| self.votes.get(&number)?.prevotes.quorum.flatten())
        {
            self.expect_parts(block.part_set_header)?;
        }
        Ok(())
    }

    fn conflicting_evidence(&self, time: Time) -> Result<Vec<types::Evidence>> {
        let mut evidence = Vec::new();
        let mut remaining = evidence_wire::MAX_MESSAGE;
        for votes in self
            .votes
            .values()
            .flat_map(|round| [&round.prevotes, &round.precommits])
        {
            for (&index, second) in &votes.second {
                let first = &votes.by_index[&index];
                let item = evidence_wire::duplicate_vote(first, second, time, &self.validators)?;
                let bytes =
                    item.encoded_len() + 1 + prost::length_delimiter_len(item.encoded_len());
                if bytes > remaining {
                    return Ok(evidence);
                }
                remaining -= bytes;
                evidence.push(item);
            }
        }
        Ok(evidence)
    }
}

impl Votes {
    fn claim(&mut self, peer: node::Id, id: Option<block::Id>) -> Result<()> {
        if let Some(previous) = self.claimed.get(&peer) {
            ensure!(*previous == id, "changed peer vote quorum claim");
        } else if self.claimed.len() < MAX_VOTE_CLAIMS
            && (self.claimed_blocks.contains(&id) || self.claimed_blocks.len() < MAX_VOTE_CLAIMS)
        {
            self.claimed.insert(peer, id);
            // A disconnect releases the peer slot, not this pool's lifetime ID quota.
            self.claimed_blocks.insert(id);
        }
        Ok(())
    }

    fn majority(&self, height: i64, number: i32, kind: Type) -> Option<Sum> {
        self.quorum
            .map(|id| majority_message(height, number, kind, id))
    }

    fn gossip(&self, height: i64, number: i32, kind: Type) -> impl Iterator<Item = Sum> + '_ {
        self.majority(height, number, kind)
            .into_iter()
            .chain(self.canonical().map(|(_, vote)| vote_message(vote)))
    }

    fn insert(&mut self, index: usize, vote: Vote, power: u64, total: u64) -> bool {
        let id = vote.block_id;
        let mut conflict = false;
        if let Some(first) = self.by_index.get(&index) {
            if first.block_id == id || self.for_block(index, id).is_some() {
                return false;
            }
            if let std::collections::btree_map::Entry::Vacant(slot) = self.second.entry(index) {
                slot.insert(vote);
                conflict = true;
            } else if self.quorum == Some(id) || self.claimed.values().any(|&block| block == id) {
                self.extra.insert((id, index), vote);
            } else {
                return false;
            }
        } else {
            self.by_index.insert(index, vote);
            self.total_seen += power;
        }
        let sum = self.power.entry(id).or_default();
        *sum += power;
        if self.quorum.is_none() && *sum * 3 > total * 2 {
            self.quorum = Some(id);
        }
        conflict
    }

    fn for_block(&self, index: usize, id: Option<block::Id>) -> Option<&Vote> {
        self.by_index
            .get(&index)
            .filter(|vote| vote.block_id == id)
            .or_else(|| self.second.get(&index).filter(|vote| vote.block_id == id))
            .or_else(|| self.extra.get(&(id, index)))
    }

    fn canonical(&self) -> impl Iterator<Item = (&usize, &Vote)> {
        self.by_index.iter().map(|(index, first)| {
            let vote = self
                .quorum
                .and_then(|id| self.for_block(*index, id))
                .unwrap_or(first);
            (index, vote)
        })
    }

    fn commit(&self, round: &Round, number: i32, id: block::Id) -> Result<Commit> {
        let mut signatures = vec![CommitSig::BlockIdFlagAbsent; round.validators.validators.len()];
        for (&index, vote) in self.canonical() {
            let timestamp = vote.timestamp.context("precommit has no timestamp")?;
            let validator_address = vote.validator_address;
            let signature = vote.signature.clone();
            signatures[index] = match vote.block_id {
                Some(block) if block == id => CommitSig::BlockIdFlagCommit {
                    validator_address,
                    timestamp,
                    signature,
                },
                None => CommitSig::BlockIdFlagNil {
                    validator_address,
                    timestamp,
                    signature,
                },
                Some(_) => continue,
            };
        }
        Ok(Commit {
            height: round.height.try_into()?,
            round: number.try_into()?,
            block_id: id,
            signatures,
        })
    }
}

fn commit_vote(index: usize, signature: &CommitSig, commit: &Commit) -> Option<types::Vote> {
    let (block_id, address, timestamp, signature) = match signature {
        CommitSig::BlockIdFlagAbsent => return None,
        CommitSig::BlockIdFlagCommit {
            validator_address,
            timestamp,
            signature,
        } => (commit.block_id, validator_address, timestamp, signature),
        CommitSig::BlockIdFlagNil {
            validator_address,
            timestamp,
            signature,
        } => (
            block::Id::default(),
            validator_address,
            timestamp,
            signature,
        ),
    };
    Some(types::Vote {
        r#type: SignedMsgType::Precommit as i32,
        height: commit.height.into(),
        round: commit.round.into(),
        block_id: Some(block_id.into()),
        timestamp: Some((*timestamp).into()),
        validator_address: address.as_bytes().to_vec(),
        validator_index: index as i32,
        signature: signature.as_ref()?.as_bytes().to_vec(),
    })
}

fn vote_message(vote: &Vote) -> Sum {
    Sum::Vote(cs::Vote {
        vote: Some(vote.clone().into()),
    })
}

fn majority_message(height: i64, round: i32, kind: Type, id: Option<block::Id>) -> Sum {
    Sum::VoteSetMaj23(cs::VoteSetMaj23 {
        height,
        round,
        r#type: kind.into(),
        block_id: Some(id.unwrap_or_default().into()),
    })
}

fn valid_block_message(height: i64, round: i32, parts: &BlockParts, is_commit: bool) -> Sum {
    Sum::NewValidBlock(cs::NewValidBlock {
        height,
        round,
        is_commit,
        block_part_set_header: Some(parts.header().into()),
        block_parts: Some(parts.availability()),
    })
}

mod recovery;

#[cfg(test)]
mod tests;
