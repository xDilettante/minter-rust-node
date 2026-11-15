//! Tendermint 0.34 evidence encoding and proof verification.

mod light;

use crate::verify::GoEd25519;
use anyhow::{Context, Result, ensure};
use prost::Message;
use tendermint::{
    Hash, Time,
    block::{self, Commit},
    chain,
    crypto::{Sha256, default::Sha256 as Hasher},
    validator,
    vote::{SignedVote, Vote},
};
use tendermint_proto::v0_34::types::{self, evidence::Sum};

pub const MAX_MESSAGE: usize = 1024 * 1024;

/// Go measures the incoming protobuf list before FromProto/ToProto normalization.
pub fn input_size(evidence: &types::EvidenceList) -> usize {
    let mut list = evidence.clone();
    for item in &mut list.evidence {
        match &mut item.sum {
            Some(Sum::LightClientAttackEvidence(proof)) => light::message_fields(proof),
            Some(Sum::DuplicateVoteEvidence(proof)) => {
                proof.timestamp.get_or_insert(light::ZERO_TIME);
                for vote in [&mut proof.vote_a, &mut proof.vote_b].into_iter().flatten() {
                    light::vote_fields(vote);
                }
            }
            None => {}
        }
    }
    list.encoded_len()
}

pub(crate) fn canonical_commit(commit: &mut types::Commit) -> Result<()> {
    light::basic_commit(commit)?;
    light::normalize_commit(commit);
    Ok(())
}

pub(crate) fn canonical_header(header: &mut types::Header) {
    light::header_fields(header);
}

/// Go ABCI conversion derives each light validator's address from its key.
pub fn to_abci(item: &types::Evidence) -> Result<Vec<tendermint_proto::v0_34::abci::Evidence>> {
    use tendermint_proto::v0_34::abci;
    let item = canonicalize(item)?;
    Ok(match item.sum.context("missing evidence type")? {
        Sum::DuplicateVoteEvidence(proof) => vec![abci::Evidence {
            r#type: abci::EvidenceType::DuplicateVote.into(),
            validator: Some(abci::Validator {
                address: proof
                    .vote_a
                    .as_ref()
                    .context("missing VoteA")?
                    .validator_address
                    .clone()
                    .into(),
                power: proof.validator_power,
            }),
            height: proof.vote_a.context("missing VoteA")?.height,
            time: proof.timestamp,
            total_voting_power: proof.total_voting_power,
        }],
        Sum::LightClientAttackEvidence(proof) => proof
            .byzantine_validators
            .iter()
            .map(|val| {
                Ok(abci::Evidence {
                    r#type: abci::EvidenceType::LightClientAttack.into(),
                    validator: Some(abci::Validator {
                        address: light::validator_address(val)?.into(),
                        power: val.voting_power,
                    }),
                    height: proof.common_height,
                    time: proof.timestamp,
                    total_voting_power: proof.total_voting_power,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

pub fn height(item: &types::Evidence) -> Result<u64> {
    match item.sum.as_ref().context("missing evidence type")? {
        Sum::DuplicateVoteEvidence(proof) => Ok(proof
            .vote_a
            .as_ref()
            .context("missing VoteA")?
            .height
            .try_into()?),
        Sum::LightClientAttackEvidence(proof) => Ok(proof.common_height.try_into()?),
    }
}

pub fn time(item: &types::Evidence) -> Result<Time> {
    let timestamp = match item.sum.as_ref().context("missing evidence type")? {
        Sum::DuplicateVoteEvidence(proof) => proof.timestamp,
        Sum::LightClientAttackEvidence(proof) => proof.timestamp,
    };
    Ok(timestamp
        .context("missing evidence timestamp")?
        .try_into()?)
}

/// Bytes hashed as a leaf in the block's EvidenceHash.
pub fn payload(item: &types::Evidence) -> Result<Vec<u8>> {
    let item = canonicalize(item)?;
    Ok(match item.sum.as_ref().context("missing evidence type")? {
        Sum::DuplicateVoteEvidence(proof) => proof.encode_to_vec(),
        Sum::LightClientAttackEvidence(proof) => proof.encode_to_vec(),
    })
}

/// Go computes evidence identity from the proof, independently of the outer list.
pub fn hash(item: &types::Evidence) -> Result<[u8; 32]> {
    let canonical = canonicalize(item)?;
    let item = &canonical;
    match item.sum.as_ref().context("missing evidence type")? {
        Sum::DuplicateVoteEvidence(proof) => {
            let mut proof = proof.clone();
            for vote in [&mut proof.vote_a, &mut proof.vote_b] {
                let vote = vote.as_mut().context("missing duplicate vote")?;
                vote.block_id
                    .get_or_insert_with(Default::default)
                    .part_set_header
                    .get_or_insert_with(Default::default);
            }
            Ok(Hasher::digest(proof.encode_to_vec()))
        }
        Sum::LightClientAttackEvidence(proof) => {
            let header = proof
                .conflicting_block
                .as_ref()
                .and_then(|block| block.signed_header.as_ref())
                .and_then(|header| header.header.clone())
                .context("missing conflicting header")?;
            let hash = light::header_hash(&header)?;
            // Tendermint 0.34 copies the first 31 hash bytes, then writes a
            // signed varint after the zero byte at offset 31.
            let mut bytes = [0; 42];
            bytes[..31].copy_from_slice(&hash[..31]);
            let mut height = height(item)? << 1;
            let mut offset = 32;
            while height >= 128 {
                bytes[offset] = height as u8 | 0x80;
                height >>= 7;
                offset += 1;
            }
            bytes[offset] = height as u8;
            Ok(Hasher::digest(&bytes[..=offset]))
        }
    }
}

/// Go FromProto/ValidateBasic followed by ToProto, preserving wire validator order.
pub fn canonicalize(item: &types::Evidence) -> Result<types::Evidence> {
    let mut item = item.clone();
    match item.sum.as_mut().context("missing evidence type")? {
        Sum::LightClientAttackEvidence(proof) => {
            light::basic(proof)?;
            light::normalize(proof);
        }
        Sum::DuplicateVoteEvidence(proof) => {
            let a = proof.vote_a.as_mut().context("missing VoteA")?;
            light::basic_vote(a)?;
            let b = proof.vote_b.as_mut().context("missing VoteB")?;
            light::basic_vote(b)?;
            ensure!(
                block_key(block_id(a.block_id.clone())?) < block_key(block_id(b.block_id.clone())?),
                "duplicate votes in invalid order"
            );
            proof.timestamp.get_or_insert(light::ZERO_TIME);
            let _: Time = proof.timestamp.context("missing timestamp")?.try_into()?;
        }
    }
    Ok(item)
}

pub fn block_id(raw: Option<types::BlockId>) -> Result<Option<block::Id>> {
    let mut raw = raw.unwrap_or_default();
    raw.part_set_header.get_or_insert_with(Default::default);
    let id: block::Id = raw.try_into()?;
    if id == block::Id::default() {
        return Ok(None);
    }
    ensure!(
        id.hash != Hash::None
            && id.part_set_header.hash != Hash::None
            && id.part_set_header.total > 0,
        "incomplete vote BlockID"
    );
    Ok(Some(id))
}

fn block_key(id: Option<block::Id>) -> Vec<u8> {
    let id = id.unwrap_or_default();
    let parts: types::PartSetHeader = id.part_set_header.into();
    let mut key = id.hash.as_bytes().to_vec();
    key.extend(parts.encode_to_vec());
    key
}

pub fn duplicate_vote(
    a: &Vote,
    b: &Vote,
    time: Time,
    validators: &validator::Set,
) -> Result<types::Evidence> {
    let validator = validators
        .validator(a.validator_address)
        .context("missing accused validator")?;
    let (a, b) = if block_key(a.block_id) < block_key(b.block_id) {
        (a, b)
    } else {
        (b, a)
    };
    let wire = |vote: &Vote| {
        let mut raw: types::Vote = vote.clone().into();
        raw.block_id = Some(vote.block_id.unwrap_or_default().into());
        raw
    };
    Ok(types::Evidence {
        sum: Some(Sum::DuplicateVoteEvidence(types::DuplicateVoteEvidence {
            vote_a: Some(wire(a)),
            vote_b: Some(wire(b)),
            total_voting_power: validators.total_voting_power.into(),
            validator_power: validator.power.into(),
            timestamp: Some(time.into()),
        })),
    })
}

pub fn verify_duplicate(
    proof: &types::DuplicateVoteEvidence,
    chain: &chain::Id,
    validators: &validator::Set,
) -> Result<()> {
    let parse = |raw: &Option<types::Vote>| -> Result<Vote> {
        let mut raw = raw.clone().context("missing duplicate vote")?;
        let id = block_id(raw.block_id.clone())?;
        raw.block_id = id.map(Into::into);
        let vote = Vote::try_from(raw)?;
        ensure!(
            vote.height.value() != 0 && vote.block_id == id,
            "invalid duplicate vote"
        );
        Ok(vote)
    };
    let a = parse(&proof.vote_a)?;
    let b = parse(&proof.vote_b)?;
    ensure!(
        (a.height, a.round, a.vote_type, a.validator_address)
            == (b.height, b.round, b.vote_type, b.validator_address)
            && block_key(a.block_id) < block_key(b.block_id),
        "invalid or unordered conflicting votes"
    );
    let validator = validators
        .validator(a.validator_address)
        .context("accused address was not a validator")?;
    ensure!(
        proof.validator_power == i64::from(validator.power)
            && proof.total_voting_power == i64::from(validators.total_voting_power),
        "evidence voting power differs from the historical set"
    );
    for vote in [a, b] {
        let signed =
            SignedVote::from_vote(vote, chain.clone()).context("missing evidence signature")?;
        validator.verify_signature::<GoEd25519>(&signed.sign_bytes(), signed.signature())?;
    }
    Ok(())
}

/// Verify light evidence with the Go 0.34.19 wire and threshold rules.
pub fn verify_light_attack(
    proof: &types::LightClientAttackEvidence,
    common: &validator::Set,
    trusted_header: &block::Header,
    trusted_commit: &Commit,
) -> Result<()> {
    light::verify(proof, common, trusted_header, trusted_commit)
}
