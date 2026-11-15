use super::{
    Outcome,
    coin::{MAX_SUPPLY, MIN_RESERVE},
    tag,
};
use crate::{
    bancor,
    coins::View,
    pool::{self, SwapStep},
    state::Amount,
};
use anyhow::{Context, Result};
use num_bigint::BigInt;
use num_traits::Zero;
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) enum GasPayment {
    Base(Amount),
    Reserve {
        coin: u32,
        amount: Amount,
        reward: Amount,
    },
    Pool(Box<SwapStep>),
}

impl GasPayment {
    pub fn coin(&self) -> u32 {
        match self {
            Self::Base(_) => 0,
            Self::Reserve { coin, .. } => *coin,
            Self::Pool(step) => step.coin_in,
        }
    }

    pub fn amount(&self) -> &Amount {
        match self {
            Self::Base(amount) | Self::Reserve { amount, .. } => amount,
            Self::Pool(step) => &step.input,
        }
    }

    pub fn reward(&self) -> &Amount {
        match self {
            Self::Base(amount) | Self::Reserve { reward: amount, .. } => amount,
            Self::Pool(step) => &step.output,
        }
    }

    pub fn swap(&self) -> Option<&SwapStep> {
        match self {
            Self::Pool(step) => Some(step),
            _ => None,
        }
    }

    pub fn choose(
        view: &View<'_>,
        pools: &pool::View<'_>,
        coin: u32,
        reward: Amount,
    ) -> Result<Result<Self, Outcome>> {
        if coin == 0 {
            return Ok(Ok(Self::Base(reward)));
        }
        if reward.is_zero() {
            return Ok(Ok(Self::Reserve {
                coin,
                amount: Amount::ZERO,
                reward,
            }));
        }
        let model = view.coin(coin)?.context("missing gas coin")?;
        let symbol = model.full_symbol();
        let reserve = if model.ratio == 0 {
            Err("Gas coin has no reserve".to_owned())
        } else {
            let supply = view.supply(coin)?;
            if let Some(rejection) = reserve_underflow(coin, &symbol, &supply.reserve, &reward) {
                Err(rejection.log)
            } else {
                Ok(bancor::sale_amount(
                    &supply.volume,
                    &supply.reserve,
                    model.ratio,
                    &reward,
                )?)
            }
        };
        let pool = pools.load(coin, 0)?;
        let maximum = Amount::from(MAX_SUPPLY);
        let pool = if let Some(pool) = pool {
            match pool.clone().buy(pools, coin, 0, &reward, &[])? {
                Some(step) if !step.input.is_zero() && step.input <= maximum => {
                    Ok((pool, step.input))
                }
                Some(step) if step.input.is_zero() => {
                    let (reserve_in, reserve_out) = pool.reserves(coin, 0);
                    Err(format!(
                        "swap pool has reserves {reserve_in} {symbol} and {reserve_out} {}, you wanted buy {reward} {symbol}",
                        view.base_symbol()
                    ))
                }
                Some(step) => Err(format!(
                    "You wanted to sell maximum {maximum} {symbol}, but currently you need to spend {} {symbol} to complete tx",
                    step.input,
                )),
                None => {
                    let base = view.base_symbol();
                    let (_, reserve_out) = pool.reserves(coin, 0);
                    Err(format!(
                        "You wanted to buy {reward} {base}, but swap pool has reserve {reserve_out} {base}"
                    ))
                }
            }
        } else {
            Err(format!(
                "swap pool between coins {symbol} and {} not exists",
                view.base_symbol()
            ))
        };
        match (pool, reserve) {
            (Ok((_, pool_amount)), Ok(amount)) if amount < pool_amount => Ok(Ok(Self::Reserve {
                coin,
                amount,
                reward,
            })),
            (Ok((pool, amount)), _) => {
                let step = pool.sell(pools, coin, 0, amount, &[])?;
                ensure_output(&step)?;
                Ok(Ok(Self::Pool(Box::new(step))))
            }
            (Err(_), Ok(amount)) => Ok(Ok(Self::Reserve {
                coin,
                amount,
                reward,
            })),
            (Err(pool), Err(bancor)) => Ok(Err(Outcome::reject(
                119,
                format!("Not possible to pay commission in coin {symbol}"),
                [("pool", pool), ("bancor", bancor)],
            ))),
        }
    }

    pub fn limit_failed(
        self,
        view: &View<'_>,
        pools: &pool::View<'_>,
        price_coin: u32,
        balance: &Amount,
    ) -> Result<Result<Self, Outcome>> {
        if balance >= self.amount() {
            return Ok(Ok(self));
        }
        match self {
            Self::Base(_) => Ok(Ok(Self::Base(balance.clone()))),
            Self::Pool(step) => {
                let coin = step.coin_in;
                let pool = pools.load(coin, 0)?.context("gas pool disappeared")?;
                let step = pool.sell(pools, coin, 0, balance.clone(), &[])?;
                if step.output.is_zero() && price_coin != 0 {
                    let symbol = view.symbol(coin)?.context("missing gas coin")?;
                    return Ok(Err(super::minimum_output(
                        coin,
                        &symbol,
                        view.base_symbol(),
                        &Amount::from(1u32),
                        &step.output,
                    )));
                }
                ensure_output(&step)?;
                Ok(Ok(Self::Pool(Box::new(step))))
            }
            Self::Reserve { coin, .. } => {
                let model = view.coin(coin)?.context("missing gas coin")?;
                let supply = view.supply(coin)?;
                let symbol = model.full_symbol();
                if balance > &supply.volume {
                    let base = view.base_symbol();
                    return Ok(Err(Outcome::reject(
                        103,
                        format!(
                            "Coin reserve balance is not sufficient for transaction. Has: {} {base}, required {balance} {base}",
                            supply.reserve
                        ),
                        [
                            ("coin_symbol", symbol),
                            ("coin_id", coin.to_string()),
                            ("has_bip_value", supply.reserve.to_string()),
                            ("required_bip_value", balance.to_string()),
                        ],
                    )));
                }
                let reward =
                    bancor::sale_return(&supply.volume, &supply.reserve, model.ratio, balance)?;
                if let Some(rejection) = reserve_underflow(coin, &symbol, &supply.reserve, &reward)
                {
                    return Ok(Err(rejection));
                }
                Ok(Ok(Self::Reserve {
                    coin,
                    amount: balance.clone(),
                    reward,
                }))
            }
        }
    }

    pub fn tag_commission(&self, tags: &mut Vec<EventAttribute>) -> Result<EventAttribute> {
        let (conversion, details) = match self {
            Self::Pool(step) => ("pool", serde_json::to_string(&step.event())?),
            _ => ("bancor", "bancor".into()),
        };
        tags.extend([
            tag(
                "tx.commission_in_base_coin",
                &self.reward().to_string(),
                false,
            ),
            tag("tx.commission_conversion", conversion, true),
            tag("tx.commission_amount", &self.amount().to_string(), false),
        ]);
        Ok(tag("tx.commission_details", &details, false))
    }

    pub fn tag_failure(&self, tags: &mut Vec<EventAttribute>) -> Result<()> {
        match self {
            Self::Pool(step) => tags.push(tag(
                "tx.commission_details",
                &serde_json::to_string(&step.event())?,
                false,
            )),
            Self::Reserve { reward, .. } => {
                tags.push(tag("tx.fail_fee_reserve", &reward.to_string(), false))
            }
            Self::Base(_) => {}
        }
        tags.extend([
            tag("tx.fail_fee", &self.amount().to_string(), false),
            tag("tx.fail", "1", true),
        ]);
        Ok(())
    }
}

pub(super) fn reserve_underflow(
    coin: u32,
    symbol: &str,
    reserve: &Amount,
    reward: &Amount,
) -> Option<Outcome> {
    let minimum = Amount::from(MIN_RESERVE);
    let needed = &minimum + reward;
    (reserve < &needed).then(|| {
        Outcome::reject(
            116,
            format!("coin {symbol} reserve is too small ({reserve}, required at least {needed})"),
            [
                ("delta", reward.to_string()),
                ("coin_reserve", reserve.to_string()),
                (
                    "current_reserve",
                    (BigInt::from(reserve.clone()) - BigInt::from(reward.clone())).to_string(),
                ),
                ("min_coin_reserve", minimum.to_string()),
                ("coin_symbol", symbol.into()),
                ("coin_id", coin.to_string()),
            ],
        )
    })
}

fn ensure_output(step: &SwapStep) -> Result<()> {
    anyhow::ensure!(!step.output.is_zero(), "gas sale has no output");
    Ok(())
}
