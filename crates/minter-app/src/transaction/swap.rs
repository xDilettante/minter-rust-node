use super::{GasPayment, Outcome, coin::MAX_SUPPLY, insufficient, tag};
use crate::{
    coins,
    pool::{self, Pool, SwapStep},
    state::{Account, Amount, amount, finish},
};
use alloy_rlp::Decodable;
use anyhow::{Context, Result, bail};
use minter_protocol::Address;
use std::collections::BTreeSet;
use tendermint_proto::v0_34::abci;

pub(super) enum SwapAmount {
    Sell(Amount),
    Buy(Amount),
    All,
}

pub(super) struct PoolTrade {
    pub coins: Vec<u32>,
    pub amount: SwapAmount,
    limit: Amount,
}

pub(crate) struct SwapRoute {
    pub steps: Vec<SwapStep>,
    pub buying: bool,
    whole_balance: Option<Amount>,
}

impl PoolTrade {
    pub fn decode(kind: u8, mut fields: &[u8]) -> Result<Self> {
        let coins = Vec::<u32>::decode(&mut fields)?;
        let size = match kind {
            0x17 => SwapAmount::Sell(amount(&mut fields)?),
            0x18 => SwapAmount::Buy(amount(&mut fields)?),
            0x19 => SwapAmount::All,
            _ => bail!("unsupported swap type"),
        };
        let limit = amount(&mut fields)?;
        finish(fields)?;
        Ok(Self {
            coins,
            amount: size,
            limit,
        })
    }

    pub fn check_path(&self, pools: &pool::View<'_>) -> Result<Option<Outcome>> {
        if self.coins.len() < 2 {
            return Ok(Some(Outcome::reject(106, "Incorrect tx data".into(), [])));
        }
        if self.coins.len() > 5 {
            return Ok(Some(Outcome::reject(
                709,
                "maximum allowed length of the exchange chain is 5".into(),
                [],
            )));
        }
        for pair in self.coins.windows(2) {
            let [from, to] = [pair[0], pair[1]];
            if from == to {
                return Ok(Some(Outcome::reject(
                    301,
                    "\"From\" coin equals to \"to\" coin".into(),
                    [
                        ("coin_id_to_sell", from.to_string()),
                        ("coin_id_to_buy", to.to_string()),
                    ],
                )));
            }
            if pools.load(from, to)?.is_none() {
                return Ok(Some(Outcome::reject(
                    701,
                    "swap pool not exists".into(),
                    [("coin0", from.to_string()), ("coin1", to.to_string())],
                )));
            }
        }
        Ok(None)
    }

    pub fn plan(
        &self,
        view: &coins::View<'_>,
        pools: &pool::View<'_>,
        sender_address: Address,
        sender: &mut Account,
        gas: &GasPayment,
        check: bool,
    ) -> Result<Result<SwapRoute, Outcome>> {
        let from = self.coins[0];
        let mut whole_balance = None;
        let size = match &self.amount {
            SwapAmount::Sell(amount) | SwapAmount::Buy(amount) => amount.clone(),
            SwapAmount::All => {
                let balance = sender.balance(view.store, view.height, from)?;
                if balance <= gas.amount() {
                    let symbol = view.symbol(from)?.context("missing swap input coin")?;
                    let available = num_bigint::BigInt::from(balance.clone())
                        - num_bigint::BigInt::from(gas.amount().clone());
                    return Ok(Err(insufficient(sender_address, from, &symbol, &available)));
                }
                whole_balance = Some(balance.clone());
                balance - gas.amount()
            }
        };
        let pairs = self
            .coins
            .windows(2)
            .map(|pair| {
                let [from, to] = [pair[0], pair[1]];
                let pool = pools
                    .load(from, to)?
                    .context("validated pool disappeared")?;
                Ok((pool, from, to))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut steps = match self.route(view, pools, &pairs, gas, size.clone(), true)? {
            Ok(steps) => steps,
            Err(error) => return Ok(Err(error)),
        };
        let buying = matches!(self.amount, SwapAmount::Buy(_));
        let first =
            if buying { steps.last() } else { steps.first() }.context("empty swap route")?;
        if whole_balance.is_none() {
            let needed = if from == gas.coin() {
                &first.input + gas.amount()
            } else {
                first.input.clone()
            };
            let lacks_input = sender.balance(view.store, view.height, from)? < &needed;
            let lacks_gas = sender.balance(view.store, view.height, gas.coin())? < gas.amount();
            if lacks_input && (buying || !lacks_gas || from == gas.coin()) {
                let symbol = view.symbol(from)?.context("missing swap input coin")?;
                return Ok(Err(insufficient(sender_address, from, &symbol, &needed)));
            }
            if lacks_gas {
                let coin = gas.coin();
                let symbol = view.symbol(coin)?.context("missing gas coin")?;
                return Ok(Err(insufficient(
                    sender_address,
                    coin,
                    &symbol,
                    gas.amount(),
                )));
            }
        }
        if !check
            && gas
                .swap()
                .is_some_and(|fee| pairs.iter().any(|(pool, _, _)| fee.uses_pool(pool)))
        {
            steps = self
                .route(view, pools, &pairs, gas, size, false)?
                .map_err(|error| anyhow::anyhow!("executing quoted swap: {}", error.log))?;
        }
        Ok(Ok(SwapRoute {
            steps,
            buying,
            whole_balance,
        }))
    }

    fn route(
        &self,
        view: &coins::View<'_>,
        pools: &pool::View<'_>,
        pairs: &[(Pool, u32, u32)],
        gas: &GasPayment,
        mut amount: Amount,
        verify_limits: bool,
    ) -> Result<Result<Vec<SwapStep>, Outcome>> {
        let buying = matches!(self.amount, SwapAmount::Buy(_));
        let last = pairs.len() - 1;
        let mut visited = BTreeSet::new();
        let mut steps = Vec::with_capacity(pairs.len());
        let one = Amount::from(1u32);
        let max_supply = Amount::from(MAX_SUPPLY);
        for index in 0..pairs.len() {
            let (pool, from, to) = &pairs[if buying { last - index } else { index }];
            let [from, to] = [*from, *to];
            if !visited.insert(pool.id) {
                return Ok(Err(Outcome::reject(
                    710,
                    format!(
                        "Forbidden to repeat the pool in the route, pool duplicate {}",
                        pool.id
                    ),
                    [("pool_id", pool.id.to_string())],
                )));
            }
            let (pool, overlay) = if let Some(fee) = gas.swap().filter(|fee| fee.uses_pool(pool)) {
                fee.pool_for_route(pools, pool, from, verify_limits)?
            } else {
                (pool.clone(), Vec::new())
            };
            let step = if buying {
                let maximum = if index == last && verify_limits {
                    &self.limit
                } else {
                    &max_supply
                };
                let step = match pool.clone().buy(pools, from, to, &amount, &overlay)? {
                    Some(step) => step,
                    None => {
                        let (reserve_in, reserve_out) = pool.reserves(from, to);
                        let symbol = view.symbol(to)?.context("missing swap output coin")?;
                        return Ok(Err(Outcome::reject(
                            703,
                            format!(
                                "You wanted to buy {amount} {symbol}, but swap pool has reserve {reserve_out} {symbol}"
                            ),
                            [
                                ("coin0", from.to_string()),
                                ("reserve0", reserve_in.to_string()),
                                ("amount0_in", maximum.to_string()),
                                ("coin1", to.to_string()),
                                ("reserve1", reserve_out.to_string()),
                                ("amount1_out", amount.to_string()),
                            ],
                        )));
                    }
                };
                if step.input.bits() == 0 {
                    let (reserve_in, reserve_out) = pool.reserves(from, to);
                    let symbol_in = view.symbol(from)?.context("missing swap input coin")?;
                    let symbol_out = view.symbol(to)?.context("missing swap output coin")?;
                    return Ok(Err(Outcome::reject(
                        703,
                        format!(
                            "swap pool has reserves {reserve_in} {symbol_in} and {reserve_out} {symbol_out}, you wanted buy {amount} {symbol_in}"
                        ),
                        [
                            ("coin0", from.to_string()),
                            ("reserve0", reserve_in.to_string()),
                            ("coin1", to.to_string()),
                            ("reserve1", reserve_out.to_string()),
                            ("amount1_out", amount.to_string()),
                        ],
                    )));
                }
                if verify_limits && &step.input > maximum {
                    let symbol = view.symbol(from)?.context("missing swap input coin")?;
                    return Ok(Err(Outcome::reject(
                        302,
                        format!(
                            "You wanted to sell maximum {maximum} {symbol}, but currently you need to spend {} {symbol} to complete tx",
                            step.input
                        ),
                        [
                            ("maximum_value_to_sell", maximum.to_string()),
                            ("needed_spend_value", step.input.to_string()),
                            ("coin_symbol", symbol),
                            ("coin_id", from.to_string()),
                        ],
                    )));
                }
                step
            } else {
                let step = pool.sell(pools, from, to, amount, &overlay)?;
                let minimum = if index == last && verify_limits {
                    (&self.limit).max(&one)
                } else {
                    &one
                };
                if &step.output < minimum {
                    let in_symbol = view.symbol(from)?.context("missing swap input coin")?;
                    let out_symbol = view.symbol(to)?.context("missing swap output coin")?;
                    return Ok(Err(minimum_output(
                        from,
                        &in_symbol,
                        &out_symbol,
                        minimum,
                        &step.output,
                    )));
                }
                step
            };
            amount = if buying {
                step.input.clone()
            } else {
                step.output.clone()
            };
            steps.push(step);
        }
        Ok(Ok(steps))
    }
}

impl SwapRoute {
    pub fn tag_success(&self, tags: &mut Vec<abci::EventAttribute>) -> Result<()> {
        let first = self.steps.first().context("empty swap route")?;
        let last = self.steps.last().context("empty swap route")?;
        let (first, last) = if self.buying {
            (last, first)
        } else {
            (first, last)
        };
        let returned = if self.buying {
            &first.input
        } else {
            &last.output
        };
        tags.extend([
            tag("tx.coin_to_buy", &last.coin_out.to_string(), true),
            tag("tx.coin_to_sell", &first.coin_in.to_string(), true),
            tag("tx.return", &returned.to_string(), false),
        ]);
        if let Some(balance) = &self.whole_balance {
            tags.push(tag("tx.sell_amount", &balance.to_string(), false));
        }
        let mut changes: Vec<_> = self.steps.iter().map(SwapStep::event).collect();
        if self.buying {
            changes.reverse();
        }
        tags.push(tag("tx.pools", &serde_json::to_string(&changes)?, false));
        Ok(())
    }
}

pub(super) fn minimum_output(
    from: u32,
    in_symbol: &str,
    out_symbol: &str,
    minimum: &Amount,
    output: &Amount,
) -> Outcome {
    Outcome::reject(
        303,
        format!(
            "You wanted to buy minimum {minimum} {out_symbol}, but currently you buy only {output} {out_symbol}"
        ),
        [
            ("minimum_value_to_buy", minimum.to_string()),
            ("will_get_value", output.to_string()),
            ("coin_symbol", in_symbol.into()),
            ("coin_id", from.to_string()),
        ],
    )
}
