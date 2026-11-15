use super::{GasPayment, Outcome, Transaction, coin::MAX_SUPPLY, liquidity::share_ticker, tag};
use crate::{
    coins::{Coin, Supply, View},
    pool,
    state::{Account, Amount, amount, finish},
};
use alloy_rlp::Decodable;
use anyhow::{Context, Result};
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) const LOCKED_LIQUIDITY: u32 = 1000;

pub(crate) struct CreatePool {
    pair: [u32; 2],
    volumes: [Amount; 2],
}

pub(crate) struct PoolCreation {
    pub pair: [u32; 2],
    pub volumes: [Amount; 2],
    pub pool_id: u32,
    pub coin_id: u32,
    pub minted: Amount,
}

impl CreatePool {
    pub fn decode(mut fields: &[u8]) -> Result<Self> {
        let pair = [u32::decode(&mut fields)?, u32::decode(&mut fields)?];
        let volumes = [amount(&mut fields)?, amount(&mut fields)?];
        finish(fields)?;
        Ok(Self { pair, volumes })
    }

    pub fn rejection(&self, coins: &View<'_>, pools: &pool::View<'_>) -> Result<Option<Outcome>> {
        let [coin0, coin1] = self.pair;
        if coin0 == coin1 {
            return Ok(Some(Outcome::reject(
                301,
                "First coin equals to second coin".into(),
                [
                    ("coin_id_to_sell", coin0.to_string()),
                    ("coin_to_sell", coin1.to_string()),
                ],
            )));
        }
        if pools.load(coin0, coin1)?.is_some() {
            return Ok(Some(Outcome::reject(
                708,
                "swap pool already exist".into(),
                [("coin0", coin0.to_string()), ("coin1", coin1.to_string())],
            )));
        }
        for coin in self.pair {
            if coins.symbol(coin)?.is_none() {
                return Ok(Some(Outcome::reject(
                    102,
                    "Coin not exists".into(),
                    [("coin_id", coin.to_string())],
                )));
            }
        }
        Ok(None)
    }

    pub fn plan(
        &self,
        coins: &View<'_>,
        pools: &pool::View<'_>,
        tx: &Transaction<'_>,
        sender: &mut Account,
        gas: &GasPayment,
    ) -> Result<Result<PoolCreation, Outcome>> {
        let [coin0, coin1] = self.pair;
        let liquidity = (&self.volumes[0] * &self.volumes[1]).sqrt();
        let locked = Amount::from(LOCKED_LIQUIDITY);
        if liquidity <= locked {
            let symbol0 = coins.symbol(coin0)?.context("missing first pool coin")?;
            let symbol1 = coins.symbol(coin1)?.context("missing second pool coin")?;
            return Ok(Err(Outcome::reject(
                704,
                format!(
                    "You wanted to add less than minimum liquidity, you should add 10 {symbol0} and 10 or more {symbol1}"
                ),
                [
                    ("coin0", coin0.to_string()),
                    ("needed_amount0", "10".into()),
                    ("coin1", coin1.to_string()),
                    ("needed_amount1", "10".into()),
                ],
            )));
        }
        for (coin, volume) in self.pair.into_iter().zip(&self.volumes) {
            let needed = if coin == gas.coin() {
                volume + gas.amount()
            } else {
                volume.clone()
            };
            if let Some(outcome) = tx.balance_rejection(coins, sender, coin, &needed)? {
                return Ok(Err(outcome));
            }
        }
        if !self.pair.contains(&gas.coin())
            && let Some(outcome) = tx.balance_rejection(coins, sender, gas.coin(), gas.amount())?
        {
            return Ok(Err(outcome));
        }
        let mut pair = self.pair;
        let mut volumes = self.volumes.clone();
        if coin0 > coin1 {
            pair.swap(0, 1);
            volumes.swap(0, 1);
        }
        Ok(Ok(PoolCreation {
            pair,
            volumes,
            pool_id: pools.next_pool_id()?,
            coin_id: coins.next_id()?,
            minted: liquidity - locked,
        }))
    }
}

impl PoolCreation {
    pub fn token(&self) -> (Coin, Supply) {
        (
            Coin {
                name: format!("Liquidity Pool {}", self.pair_name()).into_bytes(),
                ratio: 0,
                max_supply: Amount::from(MAX_SUPPLY),
                version: 0,
                symbol: share_ticker(self.pool_id),
                mintable: true,
                burnable: true,
            },
            Supply {
                volume: &self.minted + LOCKED_LIQUIDITY,
                reserve: Amount::ZERO,
            },
        )
    }

    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        tags.extend([
            tag("tx.liquidity", &self.minted.to_string(), false),
            tag("tx.pool_token", &share_ticker(self.pool_id).name(), true),
            tag("tx.pool_token_id", &self.coin_id.to_string(), true),
            tag("tx.pair_ids", &self.pair_name(), true),
            tag("tx.pool_id", &self.pool_id.to_string(), true),
        ]);
    }

    fn pair_name(&self) -> String {
        format!("{}-{}", self.pair[0], self.pair[1])
    }
}
