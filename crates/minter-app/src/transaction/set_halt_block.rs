use super::{Operation, Outcome, TransactionState};
use crate::{
    halts::{Vote, Votes},
    state::{PublicKey, display_key, finish},
};
use alloy_rlp::Decodable;
use anyhow::Result;
use minter_protocol::Address;

impl Vote {
    pub fn decode(mut fields: &[u8]) -> Result<Self> {
        let key = PublicKey::decode(&mut fields)?;
        let height = u64::decode(&mut fields)?;
        finish(fields)?;
        Ok(Self { key, height })
    }

    pub fn plan(
        self,
        view: &TransactionState<'_>,
        sender: Address,
    ) -> Result<Result<Operation, Outcome>> {
        let current = view.stakes.height + 1;
        if self.height < current {
            return Ok(Err(Outcome::reject(
                120,
                format!("Halt height should be equal or bigger than current: {current}"),
                [
                    ("block", self.height.to_string()),
                    ("current_block", display_key(self.key)),
                ],
            )));
        }
        if Votes::contains(view.halts, view.stakes.store, view.stakes.height, self)? {
            let mut duplicate = Outcome::reject(
                117,
                "Halt with such public key and height already exists".into(),
                [
                    ("public_key", display_key(self.key)),
                    ("height", self.height.to_string()),
                ],
            );
            // The Go ABCI code and the code in its JSON error differ.
            duplicate.code = 118;
            return Ok(Err(duplicate));
        }
        if let Some(outcome) = view.owner_rejection(self.key, sender)? {
            return Ok(Err(outcome));
        }
        Ok(Ok(Operation::SetHaltBlock(self)))
    }
}
