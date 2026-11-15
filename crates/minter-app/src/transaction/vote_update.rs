use super::{Outcome, TransactionState, tag};
use crate::{
    state::{PublicKey, display_key, finish},
    updates::{self, NetworkVotes, Vote},
};
use alloy_rlp::{Decodable, Header};
use anyhow::Result;
use minter_protocol::Address;
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) struct VoteUpdate {
    version: Vec<u8>,
    key: PublicKey,
    height: u64,
}

impl VoteUpdate {
    pub fn decode(mut fields: &[u8]) -> Result<Self> {
        let version = Header::decode_bytes(&mut fields, false)?.to_vec();
        let key = PublicKey::decode(&mut fields)?;
        let height = u64::decode(&mut fields)?;
        finish(fields)?;
        Ok(Self {
            version,
            key,
            height,
        })
    }

    pub fn plan(
        &self,
        view: &TransactionState<'_>,
        sender: Address,
    ) -> Result<Result<Vote, Outcome>> {
        if !updates::valid_name(&self.version) {
            return Ok(Err(Outcome::reject(122, "wrong version name".into(), [])));
        }
        let current = view.stakes.height + 1;
        if self.height < current {
            return Ok(Err(Outcome::reject(
                120,
                "vote is produced for the past state".into(),
                [
                    ("block", current.to_string()),
                    ("current_block", self.height.to_string()),
                ],
            )));
        }
        if NetworkVotes::contains(
            view.updates,
            view.stakes.store,
            view.stakes.height,
            self.height,
            self.key,
        )? {
            return Ok(Err(Outcome::reject(
                121,
                "Update vote with such public key and height already exists".into(),
                [
                    ("public_key", display_key(self.key)),
                    ("height", self.height.to_string()),
                ],
            )));
        }
        if let Some(outcome) = view.owner_rejection(self.key, sender)? {
            return Ok(Err(outcome));
        }
        Ok(Ok(Vote {
            height: self.height,
            key: self.key,
            payload: self.version.clone(),
        }))
    }

    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        tags.push(tag("tx.public_key", &hex::encode(self.key), true));
    }
}
