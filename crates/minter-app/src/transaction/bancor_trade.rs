use super::{GasPayment, Outcome, Transaction, gas::reserve_underflow, insufficient, tag};
use crate::{
    bancor,
    coins::{Supply, View},
    state::{Account, Amount, amount, finish},
};
use alloy_rlp::Decodable;
use anyhow::{Context, Result, ensure};
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) struct BancorTrade {
    pub from: u32,
    to: u32,
    pub kind: BancorAmount,
    limit: Amount,
}

pub(crate) enum BancorAmount {
    Sell(Amount),
    SellAll,
    Buy(Amount),
}

pub(crate) struct BancorChange {
    pub from: u32,
    pub to: u32,
    pub sold: Amount,
    pub bought: Amount,
    pub reserve: Amount,
    exact_buy: bool,
    sell_all_balance: Option<Amount>,
}

struct ReserveCoin {
    id: u32,
    symbol: String,
    ratio: u32,
    supply: Supply,
    maximum: Amount,
}

impl BancorTrade {
    pub fn decode(kind: u8, mut fields: &[u8]) -> Result<Self> {
        let first = u32::decode(&mut fields)?;
        let (from, to, kind) = match kind {
            0x02 => {
                let value = amount(&mut fields)?;
                (first, u32::decode(&mut fields)?, BancorAmount::Sell(value))
            }
            0x03 => (first, u32::decode(&mut fields)?, BancorAmount::SellAll),
            0x04 => {
                let value = amount(&mut fields)?;
                (u32::decode(&mut fields)?, first, BancorAmount::Buy(value))
            }
            _ => unreachable!("Bancor transaction kind"),
        };
        let limit = amount(&mut fields)?;
        finish(fields)?;
        Ok(Self {
            from,
            to,
            kind,
            limit,
        })
    }

    pub fn check_data(&self, view: &View<'_>) -> Result<Option<Outcome>> {
        for (id, missing, no_reserve) in [
            (
                self.from,
                "Coin to sell not exists",
                "sell coin has no reserve",
            ),
            (
                self.to,
                "Coin to buy not exists",
                "coin to buy has no reserve",
            ),
        ] {
            if id == 0 {
                continue;
            }
            let Some(coin) = view.coin(id)? else {
                return Ok(Some(Outcome::reject(
                    102,
                    missing.into(),
                    [("coin_id", id.to_string())],
                )));
            };
            if coin.ratio == 0 {
                return Ok(Some(Outcome::reject(
                    200,
                    no_reserve.into(),
                    [
                        ("coin_symbol", coin.full_symbol()),
                        ("coin_id", id.to_string()),
                    ],
                )));
            }
        }
        if self.from == self.to {
            let symbol = view.symbol(self.from)?.context("missing conversion coin")?;
            return Ok(Some(Outcome::reject(
                301,
                "\"From\" coin equals to \"to\" coin".into(),
                [
                    ("coin_id_to_sell", self.from.to_string()),
                    ("coin_to_sell", symbol.clone()),
                    ("coin_id_to_buy", self.to.to_string()),
                    ("coin_to_buy", symbol),
                ],
            )));
        }
        Ok(None)
    }

    pub fn plan(
        &self,
        view: &View<'_>,
        tx: &Transaction<'_>,
        sender: &mut Account,
        gas: &GasPayment,
    ) -> Result<Result<BancorChange, Outcome>> {
        let mut sell_all_balance = None;
        let sale = match &self.kind {
            BancorAmount::Sell(value) => {
                if let Some(outcome) = self.check_sale(view, tx, sender, gas, value)? {
                    return Ok(Err(outcome));
                }
                value.clone()
            }
            BancorAmount::SellAll => {
                let balance = sender.balance(view.store, view.height, self.from)?;
                if balance <= gas.amount() {
                    let symbol = view.symbol(self.from)?.context("missing conversion coin")?;
                    let mut outcome = insufficient(tx.sender, self.from, &symbol, gas.amount());
                    outcome.log.insert(0, '1');
                    return Ok(Err(outcome));
                }
                sell_all_balance = Some(balance.clone());
                balance - gas.amount()
            }
            BancorAmount::Buy(_) => Amount::ZERO,
        };

        let from = ReserveCoin::load(view, self.from, gas)?;
        let to = ReserveCoin::load(view, self.to, gas)?;
        let (sold, bought, reserve, exact_buy) = if let BancorAmount::Buy(wanted) = &self.kind {
            let deposit = if let Some(coin) = &to {
                if let Some(outcome) = coin.supply_overflow(wanted) {
                    return Ok(Err(outcome));
                }
                bancor::purchase_amount(
                    &coin.supply.volume,
                    &coin.supply.reserve,
                    coin.ratio,
                    wanted,
                )?
            } else {
                wanted.clone()
            };
            let sold = if let Some(coin) = &from {
                if coin.supply.reserve < deposit {
                    return Ok(Err(coin.insufficient_reserve(view.base_symbol(), &deposit)));
                }
                let sold = bancor::sale_amount(
                    &coin.supply.volume,
                    &coin.supply.reserve,
                    coin.ratio,
                    &deposit,
                )?;
                // The Go exact-buy path checks the reserve floor using the input coin amount.
                if let Some(outcome) = coin.reserve_underflow(&sold) {
                    return Ok(Err(outcome));
                }
                sold
            } else {
                deposit.clone()
            };
            if sold > self.limit {
                return Ok(Err(self.limit_error(view, true, &sold)?));
            }
            if let Some(outcome) = self.check_sale(view, tx, sender, gas, &sold)? {
                return Ok(Err(outcome));
            }
            (sold, wanted.clone(), deposit, true)
        } else {
            let deposit = if let Some(coin) = &from {
                if coin.supply.volume < sale {
                    return Ok(Err(coin.insufficient_reserve(view.base_symbol(), &sale)));
                }
                let returned = bancor::sale_return(
                    &coin.supply.volume,
                    &coin.supply.reserve,
                    coin.ratio,
                    &sale,
                )?;
                if let Some(outcome) = coin.reserve_underflow(&returned) {
                    return Ok(Err(outcome));
                }
                returned
            } else {
                sale.clone()
            };
            let bought = if let Some(coin) = &to {
                let bought = bancor::purchase_return(
                    &coin.supply.volume,
                    &coin.supply.reserve,
                    coin.ratio,
                    &deposit,
                )?;
                if let Some(outcome) = coin.supply_overflow(&bought) {
                    return Ok(Err(outcome));
                }
                bought
            } else {
                deposit.clone()
            };
            if bought < self.limit {
                return Ok(Err(self.limit_error(view, false, &bought)?));
            }
            (sale, bought, deposit, false)
        };
        Ok(Ok(BancorChange {
            from: self.from,
            to: self.to,
            sold,
            bought,
            reserve,
            exact_buy,
            sell_all_balance,
        }))
    }

    fn check_sale(
        &self,
        view: &View<'_>,
        tx: &Transaction<'_>,
        sender: &mut Account,
        gas: &GasPayment,
        sold: &Amount,
    ) -> Result<Option<Outcome>> {
        let mut gas_needed = gas.amount().clone();
        if gas.coin() == self.from {
            gas_needed += sold;
        } else if let Some(outcome) = tx.balance_rejection(view, sender, self.from, sold)? {
            return Ok(Some(outcome));
        }
        tx.balance_rejection(view, sender, gas.coin(), &gas_needed)
    }

    fn limit_error(&self, view: &View<'_>, buying: bool, actual: &Amount) -> Result<Outcome> {
        let verb = if buying {
            "sell maximum"
        } else {
            "buy minimum"
        };
        let mut outcome = Outcome::reject(
            302,
            format!(
                "You wanted to {verb} {}, but currently you need to spend {actual} to complete tx",
                self.limit
            ),
            [
                ("maximum_value_to_sell", self.limit.to_string()),
                ("needed_spend_value", actual.to_string()),
                (
                    "coin_symbol",
                    view.symbol(self.from)?.context("missing conversion coin")?,
                ),
                ("coin_id", self.from.to_string()),
            ],
        );
        if !buying {
            // Go serializes the maximum-spend error in Info for both limits.
            outcome.code = 303;
        }
        Ok(outcome)
    }
}

impl ReserveCoin {
    fn load(view: &View<'_>, id: u32, gas: &GasPayment) -> Result<Option<Self>> {
        if id == 0 {
            return Ok(None);
        }
        let model = view.coin(id)?.context("missing conversion coin")?;
        let mut supply = view.supply(id)?.into_owned();
        if let GasPayment::Reserve {
            coin,
            amount,
            reward,
        } = gas
            && id == *coin
        {
            ensure!(
                &supply.volume >= amount && &supply.reserve >= reward,
                "commission exceeds conversion coin supply"
            );
            supply.volume -= amount;
            supply.reserve -= reward;
        }
        Ok(Some(Self {
            id,
            supply,
            symbol: model.full_symbol(),
            ratio: model.ratio,
            maximum: model.max_supply.clone(),
        }))
    }

    fn supply_overflow(&self, delta: &Amount) -> Option<Outcome> {
        let total = &self.supply.volume + delta;
        (total > self.maximum).then(|| {
            Outcome::reject(
                112,
                "maximum supply reached".into(),
                [
                    ("delta", delta.to_string()),
                    ("coin_supply", self.supply.volume.to_string()),
                    ("current_supply", total.to_string()),
                    ("max_coin_supply", self.maximum.to_string()),
                    ("coin_symbol", self.symbol.clone()),
                    ("coin_id", self.id.to_string()),
                ],
            )
        })
    }

    fn insufficient_reserve(&self, base: &str, needed: &Amount) -> Outcome {
        let reserve = &self.supply.reserve;
        Outcome::reject(
            103,
            format!(
                "Coin reserve balance is not sufficient for transaction. Has: {reserve} {base}, required {needed} {base}"
            ),
            [
                ("coin_symbol", self.symbol.clone()),
                ("coin_id", self.id.to_string()),
                ("has_bip_value", reserve.to_string()),
                ("required_bip_value", needed.to_string()),
            ],
        )
    }

    fn reserve_underflow(&self, delta: &Amount) -> Option<Outcome> {
        reserve_underflow(self.id, &self.symbol, &self.supply.reserve, delta)
    }
}

impl BancorChange {
    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        let returned = if self.exact_buy {
            &self.sold
        } else {
            &self.bought
        };
        tags.extend([
            tag("tx.coin_to_buy", &self.to.to_string(), true),
            tag("tx.coin_to_sell", &self.from.to_string(), true),
            tag("tx.return", &returned.to_string(), false),
            tag("tx.reserve", &self.reserve.to_string(), false),
        ]);
        if let Some(balance) = &self.sell_all_balance {
            tags.push(tag("tx.sell_amount", &balance.to_string(), false));
        }
    }
}
