use super::{Outcome, TransactionState, display_address, tag};
use crate::{
    coins, frozen, stakes,
    state::{Account, Amount, PublicKey, amount, display_key, finish},
};
use alloy_rlp::Decodable;
use anyhow::{Context, Result};
use minter_protocol::Address;
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) enum StakeTransaction {
    Declare(Candidacy),
    Delegate(Delegation),
    Unbond(Withdrawal),
    Move(Withdrawal),
    Edit(CandidateEdit),
    EditPublicKey { key: PublicKey, new: PublicKey },
    EditCommission { key: PublicKey, commission: u32 },
    CandidateStatus { key: PublicKey, online: bool },
    LockStake,
    Lock { due: u32, coin: u32, value: Amount },
}

#[derive(Clone)]
pub(crate) struct Candidacy {
    pub owner: Address,
    pub key: PublicKey,
    pub commission: u32,
    pub coin: u32,
    pub value: Amount,
}

pub(crate) struct Delegation {
    key: PublicKey,
    pub coin: u32,
    pub value: Amount,
}

pub(crate) struct Withdrawal {
    from: PublicKey,
    to: Option<PublicKey>,
    coin: u32,
    value: Amount,
}

#[derive(Clone, Copy)]
pub(crate) struct CandidateEdit {
    pub key: PublicKey,
    pub reward: Address,
    pub owner: Address,
    pub control: Address,
}

pub(crate) enum StakeChange {
    Declare(Candidacy),
    Delegate {
        key: PublicKey,
        id: u32,
        coin: u32,
        value: Amount,
        waiting: Option<Amount>,
    },
    Withdraw {
        from: PublicKey,
        to: Option<PublicKey>,
        source_id: u32,
        destination_id: u32,
        coin: u32,
        value: Amount,
        waiting: Option<Amount>,
        due: u64,
    },
    CandidateStatus {
        key: PublicKey,
        online: bool,
        jailed: bool,
    },
    Edit(CandidateEdit),
    EditPublicKey {
        key: PublicKey,
        new: PublicKey,
        blocked: bool,
    },
    EditCommission {
        key: PublicKey,
        commission: u32,
    },
    LockStake(u64),
    Lock {
        due: u64,
        coin: u32,
        value: Amount,
    },
}

impl StakeTransaction {
    pub fn decode(kind: u8, mut fields: &[u8]) -> Result<Self> {
        let data = match kind {
            0x06 => Self::Declare(Candidacy {
                owner: Address::decode(&mut fields)?,
                key: PublicKey::decode(&mut fields)?,
                commission: u32::decode(&mut fields)?,
                coin: u32::decode(&mut fields)?,
                value: amount(&mut fields)?,
            }),
            0x07 => Self::Delegate(Delegation {
                key: PublicKey::decode(&mut fields)?,
                coin: u32::decode(&mut fields)?,
                value: amount(&mut fields)?,
            }),
            0x08 | 0x1b => {
                let from = PublicKey::decode(&mut fields)?;
                let to = (kind == 0x1b)
                    .then(|| PublicKey::decode(&mut fields))
                    .transpose()?;
                let data = Withdrawal {
                    from,
                    to,
                    coin: u32::decode(&mut fields)?,
                    value: amount(&mut fields)?,
                };
                if to.is_some() {
                    Self::Move(data)
                } else {
                    Self::Unbond(data)
                }
            }
            0x0a | 0x0b => Self::CandidateStatus {
                key: PublicKey::decode(&mut fields)?,
                online: kind == 0x0a,
            },
            0x0e => Self::Edit(CandidateEdit {
                key: PublicKey::decode(&mut fields)?,
                reward: Address::decode(&mut fields)?,
                owner: Address::decode(&mut fields)?,
                control: Address::decode(&mut fields)?,
            }),
            0x14 => Self::EditPublicKey {
                key: PublicKey::decode(&mut fields)?,
                new: PublicKey::decode(&mut fields)?,
            },
            0x1a => Self::EditCommission {
                key: PublicKey::decode(&mut fields)?,
                commission: u32::decode(&mut fields)?,
            },
            0x25 => Self::LockStake,
            0x26 => Self::Lock {
                due: u32::decode(&mut fields)?,
                coin: u32::decode(&mut fields)?,
                value: amount(&mut fields)?,
            },
            _ => unreachable!("stake transaction type"),
        };
        finish(fields)?;
        Ok(data)
    }

    pub fn plan(
        &self,
        view: &coins::View<'_>,
        state: &TransactionState<'_>,
        sender: &Account,
        owner: Address,
    ) -> Result<Result<StakeChange, Outcome>> {
        let height = view.height + 1;
        let testnet = view.network == 2;
        let stakes = &state.stakes;
        match self {
            Self::Declare(data) => data.plan(view, stakes, owner),
            Self::Delegate(data) => data.plan(view, stakes, owner),
            Self::Edit(data) => Ok(match state.owner_rejection(data.key, owner)? {
                Some(outcome) => Err(outcome),
                None => Ok(StakeChange::Edit(*data)),
            }),
            Self::EditPublicKey { key, new } => {
                if let Some(outcome) = state.owner_rejection(*key, owner)? {
                    return Ok(Err(outcome));
                }
                let old_key = display_key(*key);
                let new_key = display_key(*new);
                if key == new {
                    return Ok(Err(Outcome::reject(
                        411,
                        format!("Current public key ({old_key}) equals new public key ({new_key})"),
                        [("public_key", old_key), ("new_public_key", new_key)],
                    )));
                }
                if stakes.id(*new)?.active {
                    return Ok(Err(Outcome::reject(
                        401,
                        format!("Candidate with such public key ({new_key}) already exists"),
                        [("public_key", new_key)],
                    )));
                }
                Ok(Ok(StakeChange::EditPublicKey {
                    key: *key,
                    new: *new,
                    blocked: stakes.is_blocked(*new)?,
                }))
            }
            Self::EditCommission { key, commission } => {
                if let Some(outcome) = state.owner_rejection(*key, owner)? {
                    return Ok(Err(outcome));
                }
                let candidate = stakes
                    .candidate_model(stakes.id(*key)?.id)?
                    .context("missing candidate commission")?;
                let minimum = candidate.commission.saturating_sub(10);
                let maximum = (candidate.commission + 10).min(100);
                if !(minimum..=maximum).contains(commission) {
                    return Ok(Err(Outcome::reject(
                        402,
                        format!(
                            "You want change commission from {} to {commission}, but you can change no more than 10 units, because commission should be between {minimum} and {maximum}",
                            candidate.commission,
                        ),
                        [
                            ("got_commission", commission.to_string()),
                            ("min_commission", minimum.to_string()),
                            ("max_commission", maximum.to_string()),
                        ],
                    )));
                }
                let period = 3 * frozen::unbond_period(view.network);
                let previous = candidate.last_edit_commission();
                // Match Go uint64 arithmetic for imported heights.
                let next = previous.wrapping_add(period);
                if next > height {
                    return Ok(Err(Outcome::reject(
                        413,
                        format!(
                            "You cannot change the commission more than once every {period} blocks, the last change was on block {previous}"
                        ),
                        [
                            ("next_time", (next as i64).to_string()),
                            ("previous_time", (previous as i64).to_string()),
                        ],
                    )));
                }
                Ok(Ok(StakeChange::EditCommission {
                    key: *key,
                    commission: *commission,
                }))
            }
            Self::Unbond(data) => {
                let locked_until = sender.lock_stake_until;
                if locked_until > height {
                    return Ok(Err(Outcome::reject(
                        416,
                        format!(
                            "Unbond blocked for {} address until block {locked_until}. Use StakeMove to change candidate",
                            display_address(owner)
                        ),
                        [("blocked_until_height", locked_until.to_string())],
                    )));
                }
                let delay = frozen::unbond_period(view.network);
                data.plan(view, stakes, owner, height + delay)
            }
            Self::Move(data) => {
                if data.to == Some(data.from) {
                    let key = display_key(data.from);
                    return Ok(Err(Outcome::reject(
                        417,
                        format!("Candidate \"FromPubKey\" equals candidate \"ToPubKey\": {key}"),
                        [("public_key", key)],
                    )));
                }
                let delay = if testnet {
                    frozen::TESTNET_QUARTER_HOUR
                } else {
                    134_400
                };
                data.plan(view, stakes, owner, height + delay)
            }
            Self::CandidateStatus { key, online } => {
                let candidate = stakes.id(*key)?;
                if !candidate.active {
                    let mut outcome = missing_candidate(*key);
                    outcome.log = format!(
                        "Candidate with such public key ({}) not found",
                        display_key(*key)
                    );
                    return Ok(Err(outcome));
                }
                let candidate = stakes
                    .candidate(candidate.id)?
                    .context("missing candidate status")?;
                if owner != candidate.owner && owner != candidate.control {
                    return Ok(Err(Outcome::reject(
                        406,
                        "Sender is not an owner of a candidate".into(),
                        [
                            ("sender", display_address(owner)),
                            ("public_key", display_key(*key)),
                            ("owner", display_address(candidate.owner)),
                            ("control", display_address(candidate.control)),
                        ],
                    )));
                }
                Ok(Ok(StakeChange::CandidateStatus {
                    key: *key,
                    online: *online,
                    jailed: candidate.jailed_until >= height,
                }))
            }
            Self::LockStake => {
                let delay = if testnet { 34_560 } else { 18_921_600 };
                Ok(Ok(StakeChange::LockStake(height + delay)))
            }
            Self::Lock { due, coin, value } => {
                let due = u64::from(*due);
                if due <= height {
                    return Ok(Err(Outcome::reject(
                        123,
                        "Current height is higher than the specified one".into(),
                        [],
                    )));
                }
                if view.symbol(*coin)?.is_none() {
                    return Ok(Err(missing_coin(*coin)));
                }
                Ok(Ok(StakeChange::Lock {
                    due,
                    coin: *coin,
                    value: value.clone(),
                }))
            }
        }
    }
}

impl Candidacy {
    fn plan(
        &self,
        view: &coins::View<'_>,
        stakes: &stakes::View<'_>,
        sender: Address,
    ) -> Result<Result<StakeChange, Outcome>> {
        let symbol = if self.coin == 0 {
            view.base_symbol().to_owned()
        } else {
            let Some(coin) = view.coin(self.coin)? else {
                return Ok(Err(missing_coin(self.coin)));
            };
            let symbol = coin.full_symbol();
            if coin.ratio == 0 {
                return Ok(Err(Outcome::reject(
                    200,
                    "coin has no reserve".into(),
                    [("coin_symbol", symbol), ("coin_id", self.coin.to_string())],
                )));
            }
            symbol
        };
        let key = display_key(self.key);
        if stakes.id(self.key)?.active {
            return Ok(Err(Outcome::reject(
                401,
                format!("Candidate with such public key ({key}) already exists"),
                [("public_key", key)],
            )));
        }
        if stakes.is_blocked(self.key)? {
            return Ok(Err(Outcome::reject(
                410,
                format!("Candidate with such public key ({key}) exists in block list"),
                [("public_key", key)],
            )));
        }
        if self.commission > 100 {
            return Ok(Err(Outcome::reject(
                402,
                "Commission should be between 0 and 100".into(),
                [
                    ("got_commission", self.commission.to_string()),
                    ("min_commission", "0".into()),
                    ("max_commission", "100".into()),
                ],
            )));
        }
        if !stakes.can_declare(view, self.coin, &self.value)? {
            return Ok(Err(Outcome::reject(
                409,
                "Given stake is too low".into(),
                [
                    ("sender", display_address(sender)),
                    ("public_key", key),
                    ("value", self.value.to_string()),
                    ("coin_symbol", symbol),
                    ("coin_id", self.coin.to_string()),
                ],
            )));
        }
        Ok(Ok(StakeChange::Declare(self.clone())))
    }
}

impl Delegation {
    fn plan(
        &self,
        view: &coins::View<'_>,
        stakes: &stakes::View<'_>,
        owner: Address,
    ) -> Result<Result<StakeChange, Outcome>> {
        let symbol = if self.coin == 0 {
            view.base_symbol().to_owned()
        } else {
            let Some(coin) = view.coin(self.coin)? else {
                return Ok(Err(missing_coin(self.coin)));
            };
            let symbol = coin.full_symbol();
            if coin.ratio == 0 {
                return Ok(Err(Outcome::reject(
                    103,
                    "coin has no reserve".into(),
                    [
                        ("coin_symbol", symbol),
                        ("coin_id", self.coin.to_string()),
                        ("has_bip_value", view.supply(self.coin)?.reserve.to_string()),
                    ],
                )));
            }
            symbol
        };
        let candidate = stakes.id(self.key)?;
        let waiting = stakes.waiting(owner, candidate.id, self.coin)?;
        let mut total = self.value.clone();
        if let Some(value) = &waiting {
            total += value;
        }
        if total.bits() == 0 {
            return Ok(Err(Outcome::reject(
                408,
                "Stake should be positive".into(),
                [("stake", total.to_string())],
            )));
        }
        if !candidate.active {
            return Ok(Err(missing_candidate(self.key)));
        }
        if let Some(limit) =
            stakes.delegation_limit(view, candidate.id, owner, self.coin, &total)?
        {
            let (code, log) = match limit {
                stakes::DelegationLimit::Low => (409, "Stake is too low"),
                stakes::DelegationLimit::Concentrated => (
                    415,
                    "Cannot be delegated to a candidate, and his total stake exceeds 20% of the network",
                ),
            };
            return Ok(Err(Outcome::reject(
                code,
                log.into(),
                [
                    ("sender", display_address(owner)),
                    ("public_key", display_key(self.key)),
                    ("value", total.to_string()),
                    ("coin_symbol", symbol),
                    ("coin_id", self.coin.to_string()),
                ],
            )));
        }
        Ok(Ok(StakeChange::Delegate {
            key: self.key,
            id: candidate.id,
            coin: self.coin,
            value: self.value.clone(),
            waiting,
        }))
    }
}

impl Withdrawal {
    fn plan(
        &self,
        view: &coins::View<'_>,
        stakes: &stakes::View<'_>,
        owner: Address,
        due: u64,
    ) -> Result<Result<StakeChange, Outcome>> {
        let Some(symbol) = view.symbol(self.coin)? else {
            return Ok(Err(missing_coin(self.coin)));
        };
        let source = stakes.id(self.from)?;
        let waiting = stakes.waiting(owner, source.id, self.coin)?;
        if waiting.as_ref().is_none_or(|value| value < &self.value) {
            if !source.active {
                return Ok(Err(missing_candidate(self.from)));
            }
            let candidate = stakes
                .candidate(source.id)?
                .context("missing active candidate model")?;
            let active = candidate
                .stake_value(owner, self.coin)
                .filter(|value| value.bits() != 0);
            let waiting = waiting.as_ref().cloned().unwrap_or_default();
            if let Some(active) = active {
                let available = active + waiting;
                if available < self.value {
                    return Ok(Err(Outcome::reject(
                        405,
                        "Insufficient stake for sender account".into(),
                        [
                            ("public_key", display_key(self.from)),
                            ("owner", display_address(owner)),
                            ("coin_symbol", symbol),
                            ("coin_id", self.coin.to_string()),
                            ("stake_value", available.to_string()),
                            ("needed_value", self.value.to_string()),
                        ],
                    )));
                }
            } else if waiting < self.value {
                return Ok(Err(if waiting.bits() == 0 {
                    Outcome::reject(
                        404,
                        "Stake of current user not found".into(),
                        [
                            ("public_key", display_key(self.from)),
                            ("owner", display_address(owner)),
                            ("coin_symbol", symbol),
                            ("coin_id", self.coin.to_string()),
                        ],
                    )
                } else {
                    Outcome::reject(
                        412,
                        "Insufficient amount at waitlist for sender account".into(),
                        [
                            ("waitlist_value", waiting.to_string()),
                            ("needed_value", self.value.to_string()),
                        ],
                    )
                }));
            }
        }
        let destination_id = match self.to {
            Some(key) => stakes.id(key)?.id,
            None => 0,
        };
        Ok(Ok(StakeChange::Withdraw {
            from: self.from,
            to: self.to,
            source_id: source.id,
            destination_id,
            coin: self.coin,
            value: self.value.clone(),
            waiting,
            due,
        }))
    }
}

impl StakeChange {
    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        match self {
            Self::EditPublicKey { key, new, .. } => {
                tags.push(tag("tx.public_key", &hex::encode(key), true));
                tags.push(tag("tx.new_public_key", &hex::encode(new), true));
            }
            Self::Declare(Candidacy { key, coin, .. }) | Self::Delegate { key, coin, .. } => {
                tags.push(tag("tx.public_key", &hex::encode(key), true));
                tags.push(tag("tx.coin_id", &coin.to_string(), true));
            }
            Self::Withdraw {
                from,
                to,
                coin,
                due,
                ..
            } => {
                tags.push(tag("tx.public_key", &hex::encode(from), true));
                if let Some(key) = to {
                    tags.push(tag("tx.to_public_key", &hex::encode(key), true));
                }
                tags.push(tag("tx.coin_id", &coin.to_string(), true));
                tags.push(tag("tx.unlock_block_id", &due.to_string(), false));
            }
            Self::LockStake(due) => tags.push(tag("tx.unlock_block_id", &due.to_string(), false)),
            Self::Lock { coin, .. } => tags.push(tag("tx.coin_id", &coin.to_string(), true)),
            Self::CandidateStatus { key, .. }
            | Self::EditCommission { key, .. }
            | Self::Edit(CandidateEdit { key, .. }) => {
                tags.push(tag("tx.public_key", &hex::encode(key), true))
            }
        }
    }
}

fn missing_coin(coin: u32) -> Outcome {
    Outcome::reject(
        102,
        format!("Coin {coin} not exists"),
        [("coin_id", coin.to_string())],
    )
}

fn missing_candidate(key: PublicKey) -> Outcome {
    Outcome::reject(
        403,
        "Candidate with such public key not found".into(),
        [("public_key", display_key(key))],
    )
}
