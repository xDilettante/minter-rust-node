//! Durable Tendermint 0.34 Ed25519 signing for proposals, prevotes and precommits.

use anyhow::{Context, Result, ensure};
use ed25519_consensus::{Signature, SigningKey};
use minter_state::SigningJournal;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read, path::Path};
use tendermint::{
    Hash, Proposal, PublicKey, account, block, chain,
    proposal::CanonicalProposal,
    vote::{CanonicalVote, Type, Vote},
};
use tendermint_config::PrivValidatorKey;
use tendermint_proto::{
    google::protobuf::Timestamp,
    v0_34::types::{self, SignedMsgType},
};
use zeroize::Zeroizing;

pub(crate) const PROPOSE: u8 = 1;
pub(crate) const PREVOTE: u8 = 2;
pub(crate) const PRECOMMIT: u8 = 3;
const MAX_GO_FILE: u64 = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("invalid signing request: {0}")]
    Invalid(#[source] anyhow::Error),
    #[error("signing height, round or step regressed")]
    Regression,
    #[error("conflicting data at the last signing height, round and step")]
    Conflict,
    #[error("private validator journal failed: {0:#}")]
    Storage(#[source] anyhow::Error),
}

pub struct ValidatorSigner {
    key: Zeroizing<SigningKey>,
    journal: SigningJournal,
    state: State,
    failed: bool,
    _key_lock: File,
}

#[derive(Deserialize, Serialize)]
struct State {
    chain_id: chain::Id,
    public_key: PublicKey,
    last: LastSign,
}

// Go's priv_validator_state.json. Keep the exact original bytes for a retry.
#[derive(Deserialize, Serialize)]
struct LastSign {
    #[serde(with = "tendermint_proto::serializers::from_str")]
    height: i64,
    round: i32,
    step: u8,
    #[serde(default, with = "tendermint_proto::serializers::bytes::hexstring")]
    signbytes: Vec<u8>,
    #[serde(default, with = "tendermint_proto::serializers::bytes::base64string")]
    signature: Vec<u8>,
}

impl ValidatorSigner {
    /// Import the last signing state and bind it to the given key and chain.
    /// The Go signer must be stopped before its state is copied.
    pub fn import(
        journal: &Path,
        key_file: &Path,
        go_state: &Path,
        chain_id: chain::Id,
    ) -> Result<()> {
        let key_lock = lock_key(key_file)?;
        let key = read_key(&key_lock)?;
        let state = State {
            chain_id,
            public_key: key.verification_key().into(),
            last: serde_json::from_str(&read_go_file(
                &File::open(go_state).context("open Go private validator state")?,
            )?)
            .context("decode Go private validator state")?,
        };
        state.validate(&key)?;
        SigningJournal::create(journal, &serde_json::to_vec(&state)?)
    }

    pub fn open(journal: &Path, key_file: &Path, chain_id: &chain::Id) -> Result<Self> {
        let key_lock = lock_key(key_file)?;
        let (journal, bytes) = SigningJournal::open(journal)?;
        let state: State = serde_json::from_slice(&bytes).context("decode signing journal")?;
        ensure!(
            &state.chain_id == chain_id,
            "signing journal chain ID differs"
        );
        let key = read_key(&key_lock)?;
        state.validate(&key)?;
        Ok(Self {
            key,
            journal,
            state,
            failed: false,
            _key_lock: key_lock,
        })
    }

    pub fn public_key(&self) -> PublicKey {
        self.state.public_key
    }

    pub fn chain_id(&self) -> &chain::Id {
        &self.state.chain_id
    }

    pub(crate) fn position(&self) -> (i64, i32, u8) {
        self.state.last.position()
    }

    pub(crate) fn consensus_round(&self) -> Result<Option<Vec<u8>>> {
        self.journal.consensus_round()
    }

    pub(crate) fn signed_proposal(&self, block: &types::BlockId) -> Result<types::Proposal> {
        let last = &self.state.last;
        ensure!(last.step == PROPOSE, "last signature is not a proposal");
        let message: types::CanonicalProposal = decode_canonical(&last.signbytes)?;
        Ok(types::Proposal {
            r#type: SignedMsgType::Proposal as i32,
            height: last.height,
            round: last.round,
            pol_round: message.pol_round.try_into()?,
            block_id: Some(block.clone()),
            timestamp: message.timestamp,
            signature: last.signature.clone(),
        })
    }

    pub(crate) fn signed_vote(&self, index: i32) -> Result<types::Vote> {
        let last = &self.state.last;
        ensure!(
            matches!(last.step, PREVOTE | PRECOMMIT),
            "last signature is not a vote"
        );
        let message: types::CanonicalVote = decode_canonical(&last.signbytes)?;
        let block: block::Id = message
            .block_id
            .map(TryInto::try_into)
            .transpose()?
            .unwrap_or_default();
        Ok(types::Vote {
            r#type: message.r#type,
            height: last.height,
            round: last.round,
            block_id: Some(block.into()),
            timestamp: message.timestamp,
            validator_address: account::Id::from(self.state.public_key).as_bytes().to_vec(),
            validator_index: index,
            signature: last.signature.clone(),
        })
    }

    pub fn sign_proposal(&mut self, proposal: &mut types::Proposal) -> Result<(), SignError> {
        self.sign_proposal_with_round(proposal, None)
    }

    pub(crate) fn sign_proposal_with_round(
        &mut self,
        proposal: &mut types::Proposal,
        round: Option<&[u8]>,
    ) -> Result<(), SignError> {
        let domain = parse_proposal(proposal.clone()).map_err(SignError::Invalid)?;
        let canonical: types::CanonicalProposal =
            CanonicalProposal::new(domain, self.state.chain_id.clone()).into();
        let hrs = (proposal.height, proposal.round, PROPOSE);
        (proposal.timestamp, proposal.signature) =
            self.sign(hrs, canonical, round, |value| &mut value.timestamp)?;
        Ok(())
    }

    pub fn sign_vote(&mut self, vote: &mut types::Vote) -> Result<(), SignError> {
        self.sign_vote_with_round(vote, None)
    }

    pub(crate) fn sign_vote_with_round(
        &mut self,
        vote: &mut types::Vote,
        round: Option<&[u8]>,
    ) -> Result<(), SignError> {
        let address = account::Id::from(self.state.public_key);
        let domain = parse_vote(vote.clone(), address).map_err(SignError::Invalid)?;
        let step = match domain.vote_type {
            Type::Prevote => PREVOTE,
            Type::Precommit => PRECOMMIT,
        };
        let canonical: types::CanonicalVote =
            CanonicalVote::new(domain, self.state.chain_id.clone()).into();
        let hrs = (vote.height, vote.round, step);
        (vote.timestamp, vote.signature) =
            self.sign(hrs, canonical, round, |value| &mut value.timestamp)?;
        Ok(())
    }

    fn sign<M: Message + Default>(
        &mut self,
        hrs: (i64, i32, u8),
        mut canonical: M,
        round: Option<&[u8]>,
        timestamp: impl Fn(&mut M) -> &mut Option<Timestamp>,
    ) -> Result<(Option<Timestamp>, Vec<u8>), SignError> {
        if self.failed {
            return Err(SignError::Storage(anyhow::anyhow!("reopen the journal")));
        }
        let last = &self.state.last;
        match hrs.cmp(&last.position()) {
            std::cmp::Ordering::Less => Err(SignError::Regression),
            std::cmp::Ordering::Equal => {
                let mut prior = M::decode_length_delimited(last.signbytes.as_slice())
                    .context("decode last signed message")
                    .map_err(SignError::Storage)?;
                let signed_time = *timestamp(&mut prior);
                *timestamp(&mut canonical) = signed_time;
                if canonical.encode_length_delimited_to_vec() != last.signbytes {
                    return Err(SignError::Conflict);
                }
                Ok((signed_time, last.signature.clone()))
            }
            std::cmp::Ordering::Greater => {
                let signbytes = canonical.encode_length_delimited_to_vec();
                let signature = self.key.sign(&signbytes).to_bytes().to_vec();
                self.state.last = LastSign {
                    height: hrs.0,
                    round: hrs.1,
                    step: hrs.2,
                    signbytes,
                    signature: signature.clone(),
                };
                // An ambiguous write failure must stop this signer until a disk reopen.
                self.failed = true;
                let bytes = serde_json::to_vec(&self.state)
                    .context("encode signing state")
                    .map_err(SignError::Storage)?;
                self.journal
                    .persist_with_round(&bytes, round)
                    .map_err(SignError::Storage)?;
                self.failed = false;
                Ok((*timestamp(&mut canonical), signature))
            }
        }
    }
}

impl LastSign {
    fn position(&self) -> (i64, i32, u8) {
        (self.height, self.round, self.step)
    }
}

impl State {
    fn validate(&self, key: &SigningKey) -> Result<()> {
        ensure!(
            self.public_key == key.verification_key().into(),
            "signing journal belongs to another validator"
        );
        let last = &self.last;
        if last.position() == (0, 0, 0) {
            ensure!(
                last.signbytes.is_empty() && last.signature.is_empty(),
                "initial signing state contains a signature"
            );
            return Ok(());
        }
        ensure!(
            last.height > 0 && last.round >= 0,
            "invalid last signing position"
        );
        let signature = Signature::try_from(last.signature.as_slice())?;
        key.verification_key()
            .verify(&signature, &last.signbytes)
            .context("invalid signature in the last signing state")?;
        let (chain, height, round, kind, timestamp) = match last.step {
            PROPOSE => {
                let message: types::CanonicalProposal = decode_canonical(&last.signbytes)?;
                (
                    message.chain_id,
                    message.height,
                    message.round,
                    message.r#type,
                    message.timestamp,
                )
            }
            PREVOTE | PRECOMMIT => {
                let message: types::CanonicalVote = decode_canonical(&last.signbytes)?;
                (
                    message.chain_id,
                    message.height,
                    message.round,
                    message.r#type,
                    message.timestamp,
                )
            }
            _ => anyhow::bail!("invalid step in the last signing state"),
        };
        let expected = match last.step {
            PROPOSE => SignedMsgType::Proposal,
            PREVOTE => SignedMsgType::Prevote,
            _ => SignedMsgType::Precommit,
        };
        ensure!(
            chain == self.chain_id.as_str()
                && height == last.height
                && round == i64::from(last.round)
                && kind == expected as i32,
            "last signed message differs from the signing state"
        );
        tendermint::Time::try_from(timestamp.context("last signed message has no timestamp")?)?;
        Ok(())
    }
}

fn decode_canonical<M: Message + Default>(bytes: &[u8]) -> Result<M> {
    let message = M::decode_length_delimited(bytes).context("decode Go canonical sign bytes")?;
    ensure!(
        message.encode_length_delimited_to_vec() == bytes,
        "last signing state contains non-canonical bytes"
    );
    Ok(message)
}

pub(crate) fn parse_proposal(proposal: types::Proposal) -> Result<Proposal> {
    let proposal = Proposal::try_from(proposal)?;
    ensure!(
        proposal.height.value() > 0 && proposal.timestamp.is_some(),
        "proposal requires a positive height and timestamp"
    );
    ensure!(
        proposal
            .pol_round
            .is_none_or(|round| round < proposal.round),
        "proposal POL round must precede its round"
    );
    complete_block(
        proposal
            .block_id
            .context("proposal is missing a block ID")?,
    )?;
    Ok(proposal)
}

pub(crate) fn parse_vote(vote: types::Vote, address: account::Id) -> Result<Vote> {
    let vote = Vote::try_from(vote)?;
    ensure!(vote.height.value() > 0, "vote requires a positive height");
    ensure!(
        vote.validator_address == address,
        "vote validator address differs"
    );
    if let Some(block) = vote.block_id {
        complete_block(block)?;
    }
    Ok(vote)
}

fn complete_block(id: block::Id) -> Result<()> {
    ensure!(
        id.hash != Hash::None
            && id.part_set_header.total > 0
            && id.part_set_header.hash != Hash::None,
        "incomplete block ID"
    );
    Ok(())
}

fn read_go_file(file: &File) -> Result<Zeroizing<String>> {
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_GO_FILE + 1)
        .read_to_string(&mut text)
        .context("read private validator file")?;
    ensure!(
        text.len() as u64 <= MAX_GO_FILE,
        "oversized private validator file"
    );
    Ok(text)
}

fn lock_key(path: &Path) -> Result<File> {
    let mut options = File::options();
    // Open with write access for a portable exclusive lock.
    options.read(true).write(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 0);
    let key = options
        .open(path)
        .context("open private validator key for an exclusive lock")?;
    key.try_lock()
        .context("private validator key is already in use")?;
    Ok(key)
}

fn read_key(file: &File) -> Result<Zeroizing<SigningKey>> {
    let json = read_go_file(file)?;
    let key = PrivValidatorKey::parse_json(json.as_str())
        .map_err(|_| anyhow::anyhow!("invalid Tendermint private validator key file"))?;
    let public_key = key.priv_key.public_key();
    ensure!(
        key.pub_key == public_key && key.address == account::Id::from(public_key),
        "private validator key, public key and address disagree"
    );
    Ok(Zeroizing::new(
        key.priv_key
            .ed25519_signing_key()
            .context("private validator key must be Ed25519")?
            .clone()
            .try_into()?,
    ))
}

#[cfg(test)]
mod tests;
