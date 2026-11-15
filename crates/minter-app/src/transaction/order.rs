use super::{GasPayment, Outcome, display_address, insufficient, tag};
use crate::{
    coins,
    orders::{MIN_ORDER_VOLUME, Order},
    pool,
    state::{Account, Amount, amount, finish},
};
use alloy_rlp::Decodable;
use anyhow::{Context, Result};
use minter_protocol::Address;
use tendermint_proto::v0_34::abci::EventAttribute;

pub(super) enum OrderTransaction {
    Place(Placement),
    Cancel(u32),
}

pub(super) struct Placement {
    coin_to_sell: u32,
    value_to_sell: Amount,
    coin_to_buy: u32,
    value_to_buy: Amount,
}

pub(crate) enum OrderAction {
    Place { pool_id: u32, order: Order },
    Cancel { pool_id: u32, order_id: u32 },
}

impl OrderTransaction {
    pub fn decode(place: bool, mut fields: &[u8]) -> Result<Self> {
        let data = if place {
            Self::Place(Placement {
                coin_to_sell: u32::decode(&mut fields)?,
                value_to_sell: amount(&mut fields)?,
                coin_to_buy: u32::decode(&mut fields)?,
                value_to_buy: amount(&mut fields)?,
            })
        } else {
            Self::Cancel(u32::decode(&mut fields)?)
        };
        finish(fields)?;
        Ok(data)
    }

    pub fn check_data(&self, pools: &pool::View<'_>) -> Result<Option<Outcome>> {
        let Self::Place(data) = self else {
            return Ok(None);
        };
        if data.coin_to_buy == data.coin_to_sell {
            return Ok(Some(Outcome::reject(
                301,
                "\"From\" coin equals to \"to\" coin".into(),
                [
                    ("coin_id_to_sell", data.coin_to_buy.to_string()),
                    ("coin_to_sell", data.coin_to_sell.to_string()),
                ],
            )));
        }
        let minimum = Amount::from(MIN_ORDER_VOLUME);
        if data.value_to_sell < minimum || data.value_to_buy < minimum {
            return Ok(Some(Outcome::reject(
                714,
                format!("minimum volume is {MIN_ORDER_VOLUME}"),
                [
                    ("volume0", data.value_to_buy.to_string()),
                    ("Volume1", data.value_to_sell.to_string()),
                ],
            )));
        }
        if pools.load(data.coin_to_sell, data.coin_to_buy)?.is_none() {
            return Ok(Some(Outcome::reject(
                701,
                "swap pool not found".into(),
                [
                    ("coin0", data.coin_to_sell.to_string()),
                    ("coin1", data.coin_to_buy.to_string()),
                ],
            )));
        }
        Ok(None)
    }

    pub fn plan(
        &self,
        view: &coins::View<'_>,
        pools: &pool::View<'_>,
        address: Address,
        account: &mut Account,
        gas: &GasPayment,
    ) -> Result<Result<OrderAction, Outcome>> {
        let pays_in_escrow = matches!(self, Self::Place(data) if data.coin_to_sell == gas.coin());
        if !pays_in_escrow && account.balance(view.store, view.height, gas.coin())? < gas.amount() {
            let symbol = view.symbol(gas.coin())?.context("missing gas coin")?;
            return Ok(Err(insufficient(
                address,
                gas.coin(),
                &symbol,
                gas.amount(),
            )));
        }
        match self {
            Self::Place(data) => {
                let need = if pays_in_escrow {
                    &data.value_to_sell + gas.amount()
                } else {
                    data.value_to_sell.clone()
                };
                if account.balance(view.store, view.height, data.coin_to_sell)? < &need {
                    let symbol = view
                        .symbol(data.coin_to_sell)?
                        .context("missing order coin")?;
                    return Ok(Err(insufficient(
                        address,
                        data.coin_to_sell,
                        &symbol,
                        &need,
                    )));
                }
                let mut pool = pools
                    .load(data.coin_to_sell, data.coin_to_buy)?
                    .context("order pool disappeared")?;
                if let Some(fee) = gas.swap().filter(|fee| fee.uses_pool(&pool)) {
                    (pool, _) = fee.pool_for_route(pools, &pool, data.coin_to_sell, true)?;
                }
                let (reserve_sell, reserve_buy) =
                    pool.reserves(data.coin_to_sell, data.coin_to_buy);
                let upper = reserve_sell * &data.value_to_buy;
                let quoted = reserve_buy * &data.value_to_sell;
                if quoted > upper || &quoted * 5u32 < upper {
                    let current = ratio_string(reserve_sell, reserve_buy);
                    let lower = ratio_string(reserve_sell, &(reserve_buy * 5u32));
                    let price = ratio_string(&data.value_to_sell, &data.value_to_buy);
                    return Ok(Err(Outcome::reject(
                        713,
                        format!(
                            "order price is {price}, but must not exceed {current} and more than {lower}"
                        ),
                        [
                            ("min_price", current),
                            ("max_price", lower),
                            ("order_price", price),
                        ],
                    )));
                }
                let is_buy = data.coin_to_buy > data.coin_to_sell;
                let (want_buy, want_sell) = if is_buy {
                    (data.value_to_sell.clone(), data.value_to_buy.clone())
                } else {
                    (data.value_to_buy.clone(), data.value_to_sell.clone())
                };
                Ok(Ok(OrderAction::Place {
                    pool_id: pool.id,
                    order: Order {
                        id: pools.next_order_id()?,
                        want_buy,
                        want_sell,
                        is_buy,
                        pair: [
                            data.coin_to_buy.min(data.coin_to_sell),
                            data.coin_to_buy.max(data.coin_to_sell),
                        ],
                        owner: address,
                        height: view
                            .height
                            .checked_add(1)
                            .context("order height overflow")?,
                    },
                }))
            }
            Self::Cancel(id) => {
                let Some(order) = pools.order(*id)? else {
                    return Ok(Err(missing(*id, "limit order not found")));
                };
                let [from, to] = order.pair;
                if order.owner != address {
                    return Ok(Err(Outcome::reject(
                        712,
                        "Sender is not owner of this order".into(),
                        [
                            ("coin0", from.to_string()),
                            ("coin1", to.to_string()),
                            ("id", id.to_string()),
                            ("owner", display_address(order.owner)),
                        ],
                    )));
                }
                let pool = pools
                    .load(from, to)?
                    .context("missing pool for open order")?;
                if let Some(fee) = gas.swap().filter(|fee| fee.uses_pool(&pool)) {
                    let (_, updates) = fee.pool_for_route(pools, &pool, from, true)?;
                    if updates.iter().any(|update| update.deletes(*id)) {
                        return Ok(Err(missing(
                            *id,
                            "this limit order will be canceled upon payment of the commission on this transaction",
                        )));
                    }
                }
                Ok(Ok(OrderAction::Cancel {
                    pool_id: pool.id,
                    order_id: *id,
                }))
            }
        }
    }
}

impl OrderAction {
    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        match self {
            Self::Place { pool_id, order } => tags.extend([
                tag("tx.pool_id", &pool_id.to_string(), false),
                tag("tx.order_id", &order.id.to_string(), false),
            ]),
            Self::Cancel { pool_id, order_id } => tags.extend([
                tag("tx.order_id", &order_id.to_string(), true),
                tag("tx.pair_id", &pool_id.to_string(), true),
            ]),
        }
    }
}

fn missing(id: u32, log: &str) -> Outcome {
    Outcome::reject(711, log.into(), [("id", id.to_string())])
}

fn ratio_string(numerator: &Amount, denominator: &Amount) -> String {
    let scale = Amount::from(10u32).pow(34);
    let rounded = (numerator * &scale * 2u32 + denominator) / (denominator * 2u32);
    format!(
        "{}.{:0>34}",
        &rounded / &scale,
        (rounded % scale).to_string()
    )
}
