use super::{GasPayment, Outcome, Transaction, tag};
use crate::{
    coins::{Symbol, View},
    pool::{self, Pool},
    state::{Account, Amount, amount, finish},
};
use alloy_rlp::Decodable;
use anyhow::{Context, Result, ensure};
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) struct LiquidityTransaction {
    pub pair: [u32; 2],
    pub kind: LiquidityKind,
}

pub(crate) enum LiquidityKind {
    Add {
        volume0: Amount,
        maximum1: Amount,
    },
    Remove {
        shares: Amount,
        minimum: [Amount; 2],
    },
}

pub(crate) struct LiquidityChange {
    pub pool: Pool,
    pub pair: [u32; 2],
    pub volumes: [Amount; 2],
    pub shares: Amount,
    pub share_id: u32,
    share_symbol: String,
    pub adding: bool,
}

impl LiquidityTransaction {
    pub fn decode(add: bool, mut fields: &[u8]) -> Result<Self> {
        let pair = [u32::decode(&mut fields)?, u32::decode(&mut fields)?];
        let kind = if add {
            LiquidityKind::Add {
                volume0: amount(&mut fields)?,
                maximum1: amount(&mut fields)?,
            }
        } else {
            LiquidityKind::Remove {
                shares: amount(&mut fields)?,
                minimum: [amount(&mut fields)?, amount(&mut fields)?],
            }
        };
        finish(fields)?;
        Ok(Self { pair, kind })
    }

    pub fn check_data(&self, view: &View<'_>, pools: &pool::View<'_>) -> Result<Option<Outcome>> {
        let [coin0, coin1] = self.pair;
        if let LiquidityKind::Remove { shares, .. } = &self.kind
            && shares.bits() == 0
        {
            return Ok(Some(Outcome::reject(
                106,
                "Can't remove zero liquidity volume".into(),
                [],
            )));
        }
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
        if matches!(self.kind, LiquidityKind::Add { .. }) {
            if pools.load(coin0, coin1)?.is_none() {
                return Ok(Some(self.missing_pair("swap pool not found")));
            }
            for coin in self.pair {
                if view.symbol(coin)?.is_none() {
                    return Ok(Some(Outcome::reject(
                        102,
                        "Coin not exists".into(),
                        [("coin_id", coin.to_string())],
                    )));
                }
            }
        }
        Ok(None)
    }

    pub fn plan(
        &self,
        view: &View<'_>,
        pools: &pool::View<'_>,
        tx: &Transaction<'_>,
        sender: &mut Account,
        gas: &GasPayment,
        check: bool,
    ) -> Result<Result<LiquidityChange, Outcome>> {
        let [coin0, coin1] = self.pair;
        if let LiquidityKind::Add { volume0, .. } = &self.kind {
            let mut needed = volume0.clone();
            if gas.coin() == coin0 {
                needed += gas.amount();
            }
            if let Some(outcome) = tx.balance_rejection(view, sender, coin0, &needed)? {
                return Ok(Err(outcome));
            }
        }
        let Some(pool) = pools.load(coin0, coin1)? else {
            return Ok(Err(self.missing_pair("swap pool for pair not found")));
        };
        let fee_step = gas.swap().filter(|step| step.uses_pool(&pool));
        let projected = match fee_step {
            Some(step) => step.pool_for_route(pools, &pool, coin0, true)?.0,
            None => pool.clone(),
        };
        let (r0, r1) = projected.reserves(coin0, coin1);
        let share_id = view
            .current_id(share_ticker(pool.id))?
            .context("missing pool token")?;
        let share_symbol = view.symbol(share_id)?.context("missing pool token")?;
        let supply = view.supply(share_id)?;
        let total = &supply.volume;
        ensure!(total.bits() > 0, "empty pool token supply");
        let mut actual = match fee_step {
            Some(step) if !check => step.pool_for_route(pools, &pool, coin0, false)?.0,
            _ => projected.clone(),
        };
        let (actual0, actual1) = actual.reserves(coin0, coin1);

        let (volumes, shares, adding) = match &self.kind {
            LiquidityKind::Add { volume0, maximum1 } => {
                let volume1 = volume0 * r1 / r0;
                if &volume1 > maximum1 {
                    let [symbol0, symbol1] = self.symbols(view)?;
                    return Ok(Err(Outcome::reject(
                        702,
                        format!(
                            "You wanted to add {volume0} {symbol0}, but currently you need to add {volume1} {symbol1} to complete tx"
                        ),
                        [
                            ("coin1", coin1.to_string()),
                            ("maximum_amount1", maximum1.to_string()),
                            ("needed_amount1", volume1.to_string()),
                        ],
                    )));
                }
                if (volume0 * total / r0).bits() == 0 {
                    let [symbol0, symbol1] = self.symbols(view)?;
                    let need0 = r0 / total;
                    let need1 = r1 / total;
                    return Ok(Err(Outcome::reject(
                        704,
                        format!(
                            "You wanted to add less than one liquidity, you should add {need0} {symbol0} and {need1} {symbol1} or more"
                        ),
                        [
                            ("coin0", coin0.to_string()),
                            ("needed_amount0", need0.to_string()),
                            ("coin1", coin1.to_string()),
                            ("needed_amount1", need1.to_string()),
                        ],
                    )));
                }
                let mut needed = volume1;
                if gas.coin() == coin1 {
                    needed += gas.amount();
                }
                if let Some(outcome) = tx.balance_rejection(view, sender, coin1, &needed)? {
                    return Ok(Err(outcome));
                }
                if let Some(outcome) =
                    tx.balance_rejection(view, sender, gas.coin(), gas.amount())?
                {
                    return Ok(Err(outcome));
                }
                let shares = volume0 * total / actual0;
                ensure!(
                    shares.bits() > 0,
                    "fee settlement leaves zero minted liquidity"
                );
                ([volume0.clone(), volume0 * actual1 / actual0], shares, true)
            }
            LiquidityKind::Remove { shares, minimum } => {
                let mut needed = gas.amount().clone();
                if gas.coin() == share_id {
                    needed += shares;
                } else if let Some(outcome) =
                    tx.balance_rejection(view, sender, share_id, shares)?
                {
                    return Ok(Err(outcome));
                }
                if let Some(outcome) = tx.balance_rejection(view, sender, gas.coin(), &needed)? {
                    return Ok(Err(outcome));
                }
                let [v0, v1] = [shares * r0 / total, shares * r1 / total];
                let [min0, min1] = minimum;
                if &v0 < min0 || &v1 < min1 {
                    let [symbol0, symbol1] = self.symbols(view)?;
                    return Ok(Err(Outcome::reject(
                        705,
                        format!(
                            "You wanted to get more {min0} {symbol0} and more {min1} {symbol1}, but currently liquidity {shares} is equal {v0} {symbol0} and {v1} {symbol1}"
                        ),
                        [
                            ("coin0", coin0.to_string()),
                            ("want_amount0", min0.to_string()),
                            ("amount0_out", v0.to_string()),
                            ("coin1", coin1.to_string()),
                            ("want_amount1", min1.to_string()),
                            ("amount1_out", v1.to_string()),
                            ("requested_liquidity", shares.to_string()),
                        ],
                    )));
                }
                let volumes = [shares * actual0 / total, shares * actual1 / total];
                ensure!(
                    volumes.iter().zip(minimum).all(|(v, min)| v >= min),
                    "fee settlement reduces liquidity below burn limits"
                );
                (volumes, shares.clone(), false)
            }
        };
        actual.change_liquidity(self.pair, &volumes, adding)?;
        Ok(Ok(LiquidityChange {
            pool: actual,
            pair: self.pair,
            volumes,
            shares,
            share_id,
            share_symbol,
            adding,
        }))
    }

    fn missing_pair(&self, log: &str) -> Outcome {
        let [coin0, coin1] = self.pair;
        Outcome::reject(
            701,
            log.into(),
            [("coin0", coin0.to_string()), ("coin1", coin1.to_string())],
        )
    }

    fn symbols(&self, view: &View<'_>) -> Result<[String; 2]> {
        let [coin0, coin1] = self.pair;
        Ok([
            view.symbol(coin0)?.context("missing first pool coin")?,
            view.symbol(coin1)?.context("missing second pool coin")?,
        ])
    }
}

impl LiquidityChange {
    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>, details: EventAttribute) {
        let [coin0, coin1] = self.pair;
        let [volume0, volume1] = &self.volumes;
        let token = [
            tag("tx.pool_token", &self.share_symbol, true),
            tag("tx.pool_token_id", &self.share_id.to_string(), true),
        ];
        if self.adding {
            tags.extend([
                details,
                tag("tx.volume1", &volume1.to_string(), false),
                tag("tx.liquidity", &self.shares.to_string(), false),
            ]);
            tags.extend(token);
        } else {
            tags.extend([
                tag("tx.volume0", &volume0.to_string(), false),
                tag("tx.volume1", &volume1.to_string(), false),
            ]);
            tags.extend(token);
            tags.push(details);
        }
        tags.extend([
            tag(
                "tx.pair_ids",
                &format!("{}-{}", coin0.min(coin1), coin0.max(coin1)),
                false,
            ),
            tag("tx.pool_id", &self.pool.id.to_string(), true),
        ]);
    }
}

pub(super) fn share_ticker(id: u32) -> Symbol {
    let name = format!("LP-{id}");
    let mut symbol = [0; 10];
    let len = symbol.len().min(name.len());
    symbol[..len].copy_from_slice(&name.as_bytes()[..len]);
    Symbol(symbol)
}
