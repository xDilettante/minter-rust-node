use super::{Outcome, display_address, tag};
use crate::{
    coins::{Coin, Supply, Symbol, View},
    state::{Amount, amount, finish},
};
use alloy_rlp::{Decodable, Header};
use anyhow::{Context, Result};
use minter_protocol::Address;
use tendermint_proto::v0_34::abci::EventAttribute;

const SYMBOL_PATTERN: &str = "^[A-Z0-9]{3,10}$";
pub(super) const MAX_SUPPLY: u128 = 10u128.pow(33);
const MIN_COIN_SUPPLY: u64 = 10u64.pow(18);
pub(super) const MIN_RESERVE: u128 = 10u128.pow(22);

pub(crate) struct Creation {
    pub coin: Coin,
    pub supply: Supply,
}

pub(crate) struct SupplyChange {
    pub id: u32,
    pub value: Amount,
    pub mint: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct EditOwner {
    pub symbol: Symbol,
    pub owner: Address,
}

pub(crate) struct OwnerChange {
    pub edit: EditOwner,
    pub id: u32,
}

pub(crate) struct Recreation {
    pub id: u32,
    pub previous_id: u32,
    pub previous: Coin,
    pub history: Vec<u32>,
}

impl Recreation {
    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        tags.extend([
            tag("tx.coin_symbol", &self.previous.symbol.name(), true),
            tag("tx.coin_id", &self.id.to_string(), true),
            tag("tx.old_coin_symbol", &self.previous.full_symbol(), false),
            tag("tx.old_coin_id", &self.previous_id.to_string(), true),
        ]);
    }
}

impl EditOwner {
    pub fn decode(mut fields: &[u8]) -> Result<Self> {
        let symbol = Symbol(<[u8; 10]>::decode(&mut fields)?);
        let owner = Address::decode(&mut fields)?;
        finish(fields)?;
        Ok(Self { symbol, owner })
    }

    pub fn plan(&self, view: &View<'_>, sender: Address) -> Result<Result<OwnerChange, Outcome>> {
        let Some(id) = view.current_id(self.symbol)? else {
            let symbol = self.symbol.name().into_owned();
            let log = format!("Coin {symbol} not exists");
            return Ok(Err(if symbol.is_empty() {
                Outcome::reject(102, log, [])
            } else {
                Outcome::reject(102, log, [("coin_symbol", symbol)])
            }));
        };
        let owner = view.owner(self.symbol)?;
        if owner != Some(sender) {
            return Ok(Err(not_owner(self.symbol, owner)));
        }
        Ok(Ok(OwnerChange { edit: *self, id }))
    }
}

impl OwnerChange {
    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        tags.extend([
            tag("tx.coin_symbol", &self.edit.symbol.name(), true),
            tag("tx.coin_id", &self.id.to_string(), true),
        ]);
    }
}

fn not_owner(symbol: Symbol, owner: Option<Address>) -> Outcome {
    let log = "Sender is not owner of coin".into();
    let symbol = symbol.name().into_owned();
    if let Some(owner) = owner {
        return Outcome::reject(
            206,
            log,
            [("coin_symbol", symbol), ("owner", display_address(owner))],
        );
    }
    let mut outcome = Outcome::reject(206, log, [("coin_symbol", symbol)]);
    outcome.info.pop();
    outcome.info.push_str(",\"owner\":null}");
    outcome
}

impl SupplyChange {
    pub fn decode(mut fields: &[u8], mint: bool) -> Result<Self> {
        let id = u32::decode(&mut fields)?;
        let value = amount(&mut fields)?;
        finish(fields)?;
        Ok(Self { id, value, mint })
    }

    pub fn rejection(&self, view: &View<'_>, sender: Address) -> Result<Option<Outcome>> {
        if self.id == 0 {
            return Ok(Some(self.disabled(view.base_symbol())));
        }
        let Some(coin) = view.coin(self.id)? else {
            return Ok(Some(Outcome::reject(
                102,
                "Coin not exists".into(),
                [("coin_id", self.id.to_string())],
            )));
        };
        if (self.mint && !coin.mintable) || (!self.mint && !coin.burnable) {
            return Ok(Some(self.disabled(&coin.full_symbol())));
        }
        let supply = view.supply(self.id)?;
        let max = &coin.max_supply;
        let volume = &supply.volume;
        if self.mint {
            if volume + &self.value > *max {
                return Ok(Some(Outcome::reject(
                    206,
                    format!("Coin supply should not exceed the limit: {max}"),
                    [
                        ("max_coin_supply", max.to_string()),
                        ("coin_supply", volume.to_string()),
                        ("add_amount", self.value.to_string()),
                    ],
                )));
            }
            let owner = view.owner(coin.symbol)?;
            if coin.version != 0 || owner != Some(sender) {
                return Ok(Some(not_owner(coin.symbol, owner)));
            }
        } else if &self.value >= volume {
            return Ok(Some(Outcome::reject(
                206,
                "Coin volume should be more than 1".into(),
                [
                    ("max_coin_supply", max.to_string()),
                    ("min_coin_supply", "1".into()),
                    ("coin_supply", volume.to_string()),
                    ("sub_amount", self.value.to_string()),
                ],
            )));
        }
        Ok(None)
    }

    fn disabled(&self, symbol: &str) -> Outcome {
        let (code, log) = if self.mint {
            (801, "Coin not mintable")
        } else {
            (802, "Coin not burnable")
        };
        Outcome::reject(
            code,
            log.into(),
            [
                ("coin_symbol", symbol.into()),
                ("coin_id", self.id.to_string()),
            ],
        )
    }
}

impl Creation {
    pub fn decode(mut fields: &[u8], token: bool) -> Result<Self> {
        let name = Header::decode_bytes(&mut fields, false)?.to_vec();
        let symbol = Symbol(<[u8; 10]>::decode(&mut fields)?);
        let volume = amount(&mut fields)?;
        let (reserve, ratio) = if token {
            (Amount::default(), 0)
        } else {
            (amount(&mut fields)?, u32::decode(&mut fields)?)
        };
        let max_supply = amount(&mut fields)?;
        let (mintable, burnable) = if token {
            (bool::decode(&mut fields)?, bool::decode(&mut fields)?)
        } else {
            (false, false)
        };
        finish(fields)?;
        Ok(Self {
            coin: Coin {
                name,
                ratio,
                max_supply,
                version: 0,
                symbol,
                mintable,
                burnable,
            },
            supply: Supply { volume, reserve },
        })
    }

    pub fn rejection(&self, view: &View<'_>, token: bool) -> Result<Option<Outcome>> {
        if let Some(outcome) = self.name_rejection() {
            return Ok(Some(outcome));
        }
        let symbol = self.coin.symbol.bytes();
        if !(3..=10).contains(&symbol.len())
            || !symbol
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            || symbol.iter().all(u8::is_ascii_digit)
        {
            let log = format!(
                "Invalid coin symbol. Should be {SYMBOL_PATTERN} and must contain characters"
            );
            let pattern = ("pattern", SYMBOL_PATTERN.to_owned());
            let rejection = if symbol.is_empty() {
                Outcome::reject(203, log, [pattern])
            } else {
                Outcome::reject(
                    203,
                    log,
                    [
                        pattern,
                        ("coin_symbol", self.coin.symbol.name().into_owned()),
                    ],
                )
            };
            return Ok(Some(rejection));
        }
        if let Some(id) = view.current_id(self.coin.symbol)? {
            return Ok(Some(Outcome::reject(
                201,
                "Coin already exists".into(),
                [
                    ("coin_symbol", self.coin.symbol.name().into_owned()),
                    ("coin_id", id.to_string()),
                ],
            )));
        }
        Ok(if token {
            self.token_rejection()
        } else {
            self.coin_rejection(false)
        })
    }

    pub fn plan_recreation(
        &self,
        view: &View<'_>,
        sender: Address,
        token: bool,
    ) -> Result<Result<Recreation, Outcome>> {
        if let Some(outcome) = self.name_rejection().or_else(|| {
            if token {
                self.token_rejection()
            } else {
                self.coin_rejection(true)
            }
        }) {
            return Ok(Err(outcome));
        }
        let Some(previous_id) = view.current_id(self.coin.symbol)? else {
            let symbol = self.coin.symbol.name().into_owned();
            return Ok(Err(if symbol.is_empty() {
                Outcome::reject(102, "Coin  not exists".into(), [])
            } else {
                Outcome::reject(
                    102,
                    format!("Coin {symbol} not exists"),
                    [("coin_symbol", symbol)],
                )
            }));
        };
        let owner = view.owner(self.coin.symbol)?;
        if owner != Some(sender) {
            return Ok(Err(not_owner(self.coin.symbol, owner)));
        }
        let mut previous = view
            .coin(previous_id)?
            .context("missing current coin")?
            .into_owned();
        let history = view.history(self.coin.symbol)?;
        let mut latest = 0;
        for &id in &history {
            let version = view.coin(id)?.context("missing historical coin")?.version;
            latest = latest.max(version);
        }
        previous.version = latest.wrapping_add(1);
        Ok(Ok(Recreation {
            id: view.next_id()?,
            previous_id,
            previous,
            history,
        }))
    }

    fn name_rejection(&self) -> Option<Outcome> {
        let name_len = self.coin.name.len();
        if name_len > 64 {
            return Some(Outcome::reject(
                204,
                "Coin name is invalid. Allowed up to 64 bytes.".into(),
                [
                    ("max_bytes", "64".into()),
                    ("got_bytes", name_len.to_string()),
                ],
            ));
        }
        None
    }

    fn token_rejection(&self) -> Option<Outcome> {
        let volume = &self.supply.volume;
        let max = &self.coin.max_supply;
        let log = if !self.coin.mintable && volume != max {
            "Maximum supply cannot be more than the initial amount, if the token is not mintable"
                .into()
        } else if volume.bits() == 0 || volume > max {
            format!("Coin amount should be between 1 and {max}")
        } else if max > &Amount::from(MAX_SUPPLY) {
            format!("Max coin supply should be less {MAX_SUPPLY}")
        } else {
            return None;
        };
        Some(Outcome::reject(
            205,
            log,
            [
                ("max_coin_supply", MAX_SUPPLY.to_string()),
                ("min_coin_supply", "1".into()),
                ("current_coin_supply", max.to_string()),
                ("current_initial_amount", volume.to_string()),
            ],
        ))
    }

    fn coin_rejection(&self, recreating: bool) -> Option<Outcome> {
        let volume = &self.supply.volume;
        let reserve = &self.supply.reserve;
        let max = &self.coin.max_supply;
        let invalid_amount = volume < &Amount::from(MIN_COIN_SUPPLY) || volume > max;
        let log = if recreating && invalid_amount {
            format!("Coin supply should be between {MIN_COIN_SUPPLY} and {max}")
        } else if max > &Amount::from(MAX_SUPPLY) {
            format!("Max coin supply should be less than {MAX_SUPPLY}")
        } else if invalid_amount {
            format!("Coin supply should be between {MIN_COIN_SUPPLY} and {max}")
        } else if reserve < &Amount::from(MIN_RESERVE) {
            // This Go branch encodes a map, so its fields are lexically ordered.
            return Some(Outcome::reject(
                205,
                format!("Coin reserve should be greater than or equal to {MIN_RESERVE}"),
                [
                    ("current_initial_reserve", reserve.to_string()),
                    ("min_initial_reserve", MIN_RESERVE.to_string()),
                ],
            ));
        } else if !(10..=100).contains(&self.coin.ratio) {
            return Some(Outcome::reject(
                202,
                "Constant Reserve Ratio should be between 10 and 100".into(),
                [
                    ("max_crr", "100".into()),
                    ("min_crr", "10".into()),
                    ("got_crr", self.coin.ratio.to_string()),
                ],
            ));
        } else {
            return None;
        };
        Some(Outcome::reject(
            205,
            log,
            [
                ("max_coin_supply", MAX_SUPPLY.to_string()),
                ("min_coin_supply", MIN_COIN_SUPPLY.to_string()),
                ("current_coin_supply", max.to_string()),
                ("min_initial_reserve", MIN_RESERVE.to_string()),
                ("current_initial_reserve", reserve.to_string()),
                ("current_initial_amount", volume.to_string()),
            ],
        ))
    }
}
