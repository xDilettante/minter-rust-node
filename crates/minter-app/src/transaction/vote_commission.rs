use super::{Outcome, TransactionState};
use crate::{
    coins,
    commission::Fees,
    pool,
    state::{PublicKey, display_key},
    updates::{CommissionVotes, Vote},
};
use alloy_rlp::Decodable;
use anyhow::Result;
use minter_protocol::Address;

pub(crate) struct VoteCommission {
    pub key: PublicKey,
    height: u64,
    prices: Fees,
}

impl VoteCommission {
    pub fn decode(mut fields: &[u8]) -> Result<Self> {
        let key = PublicKey::decode(&mut fields)?;
        let height = u64::decode(&mut fields)?;
        let mut prices = Fees::decode_fields(fields)?;
        // Go's transaction orders MintToken before BurnToken. The IAVL
        // record orders them the other way around.
        std::mem::swap(&mut prices.mint_token, &mut prices.burn_token);
        Ok(Self {
            key,
            height,
            prices,
        })
    }

    pub fn plan(
        &self,
        coins: &coins::View<'_>,
        pools: &pool::View<'_>,
        candidates: &TransactionState<'_>,
        sender: Address,
    ) -> Result<Result<Vote, Outcome>> {
        if !self.prices.more.is_empty() {
            return Ok(Err(Outcome::reject(
                106,
                "More or less parameters than expected".into(),
                [],
            )));
        }
        let current = coins.height + 1;
        if self.height < current {
            return Ok(Err(Outcome::reject(
                120,
                "Vote is produced for the past state".into(),
                [
                    ("block", current.to_string()),
                    ("current_block", self.height.to_string()),
                ],
            )));
        }
        if CommissionVotes::contains(
            candidates.commissions,
            coins.store,
            coins.height,
            self.height,
            self.key,
        )? {
            return Ok(Err(Outcome::reject(
                121,
                "Commission price vote with such public key and height already exists".into(),
                [
                    ("public_key", display_key(self.key)),
                    ("height", self.height.to_string()),
                ],
            )));
        }
        let price_coin = self.prices.price_coin;
        if price_coin != 0 {
            if coins.coin(price_coin)?.is_none() {
                return Ok(Err(Outcome::reject(
                    102,
                    "Coin to sell not exists".into(),
                    [("coin_id", price_coin.to_string())],
                )));
            }
            if pools.load(price_coin, 0)?.is_none() {
                return Ok(Err(Outcome::reject(
                    701,
                    "swap pool not found".into(),
                    [("coin0", price_coin.to_string()), ("coin1", "0".into())],
                )));
            }
        }
        if let Some(outcome) = candidates.owner_rejection(self.key, sender)? {
            return Ok(Err(outcome));
        }
        Ok(Ok(Vote {
            height: self.height,
            key: self.key,
            payload: self.prices.encode(),
        }))
    }
}
