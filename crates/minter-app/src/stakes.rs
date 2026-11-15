pub(crate) mod genesis;

use crate::{
    bancor, coins, frozen,
    rewards::{DAO, DEVELOPERS},
    state::{
        Amount, PublicKey, Validator, amount, display_key, finish, full_list, list,
        load_validators, magnitude, put_amount, set,
    },
    transaction::display_address,
};
use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::{Context, Result, ensure};
use minter_api::{Any, pack, pb::StakeKickEvent};
use minter_protocol::Address;
use minter_state::{BlockStore, HistoricalState, Mutation};
use num_bigint::BigInt;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
};

const MAX_STAKES: usize = 1000;
const MAX_CANDIDATES: usize = 100;
const DECLARATION_STAKE_CHECK_COUNT: usize = 192;
const MAX_VALIDATORS: usize = 64;
const ACTIVE_IDS: &[u8] = b"\xd3";
const BLOCKED_KEYS: &[u8] = b"\xc5";
const MAX_ID: &[u8] = b"\xcc";
const DELETED_IDS: &[u8] = b"\xc7";

#[derive(Default)]
pub(crate) struct Candidates {
    candidates: BTreeMap<u32, Candidate>,
    active: BTreeMap<PublicKey, u32>,
    deleted: BTreeMap<PublicKey, u32>,
    blocked: BTreeSet<PublicKey>,
    max_id: u32,
    catalog_dirty: bool,
    removed: BTreeSet<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RetainedCandidateRow {
    key: Vec<u8>,
    value: Vec<u8>,
}

fn candidate_row(key: &[u8]) -> Option<(u32, u8, &[u8])> {
    let [b'c', a, b, c, d, field, suffix @ ..] = key else {
        return None;
    };
    Some((u32::from_le_bytes([*a, *b, *c, *d]), *field, suffix))
}

fn retained_in_range<'a>(
    rows: &'a [RetainedCandidateRow],
    start: &[u8],
    end: &[u8],
) -> &'a [RetainedCandidateRow] {
    let first = rows.partition_point(|row| row.key.as_slice() < start);
    let last = rows.partition_point(|row| row.key.as_slice() < end);
    &rows[first..last]
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub key: PublicKey,
    pub reward_address: Address,
    pub owner: Address,
    pub control: Address,
    pub commission: u32,
    pub jailed_until: u64,
    status: u8,
    id: u32,
    last_edit_commission: u64,
    model_dirty: bool,
    total: Amount,
    total_dirty: bool,
    updates_dirty: bool,
    pub stakes: Vec<Option<Stake>>,
    updates: Vec<Stake>,
}

#[derive(Clone)]
pub(crate) struct Stake {
    pub owner: Address,
    pub coin: u32,
    value: Amount,
    pub bip: Amount,
    dirty: bool,
}

impl Stake {
    pub fn take_value(&mut self) -> Amount {
        self.dirty = true;
        std::mem::take(&mut self.value)
    }

    fn decode(mut fields: &[u8]) -> Result<Self> {
        let stake = Self {
            owner: Address::decode(&mut fields)?,
            coin: u32::decode(&mut fields)?,
            value: amount(&mut fields)?,
            bip: amount(&mut fields)?,
            dirty: false,
        };
        finish(fields)?;
        Ok(stake)
    }

    fn encode(&self) -> Vec<u8> {
        list(|out| {
            self.owner.encode(out);
            self.coin.encode(out);
            put_amount(&self.value, out);
            put_amount(&self.bip, out);
        })
    }
}

fn candidate_key(id: u32, field: u8) -> Vec<u8> {
    [b"c".as_slice(), &id.to_le_bytes(), &[field]].concat()
}

fn stake_key(id: u32, index: usize) -> Vec<u8> {
    let mut key = candidate_key(id, b's');
    key.extend(magnitude(&Amount::from(index)));
    key
}

fn stake_index(bytes: &[u8]) -> Result<usize> {
    Ok(match bytes {
        [] => 0,
        [a] if *a != 0 => usize::from(*a),
        [a, b] if *a != 0 => usize::from(u16::from_be_bytes([*a, *b])),
        _ => anyhow::bail!("invalid candidate stake index"),
    })
}

fn candidate_models(raw: Option<Vec<u8>>) -> Result<BTreeMap<u32, Candidate>> {
    let mut candidates = BTreeMap::new();
    if let Some(raw) = raw {
        let mut rows = full_list(&raw)?;
        while !rows.is_empty() {
            let candidate = Candidate::decode(Header::decode_bytes(&mut rows, true)?)?;
            ensure!(
                candidates.insert(candidate.id, candidate).is_none(),
                "duplicate candidate ID"
            );
        }
    }
    Ok(candidates)
}

fn decode_ids(raw: &[u8], active: bool) -> Result<BTreeMap<PublicKey, u32>> {
    let mut ids = BTreeMap::new();
    let mut rows = full_list(raw)?;
    while !rows.is_empty() {
        let mut row = Header::decode_bytes(&mut rows, true)?;
        let (key, id) = if active {
            (PublicKey::decode(&mut row)?, u32::decode(&mut row)?)
        } else {
            let id = u32::decode(&mut row)?;
            (PublicKey::decode(&mut row)?, id)
        };
        finish(row)?;
        ids.insert(key, id);
    }
    Ok(ids)
}

fn decode_blocked(raw: &[u8]) -> Result<BTreeSet<PublicKey>> {
    let mut keys = BTreeSet::new();
    let mut rows = full_list(raw)?;
    while !rows.is_empty() {
        keys.insert(PublicKey::decode(&mut rows)?);
    }
    Ok(keys)
}

pub(crate) fn delegated(
    state: &HistoricalState<'_>,
    owner: Address,
    retained: &[RetainedCandidateRow],
    check: impl Fn() -> Result<()>,
) -> Result<BTreeMap<u32, (Amount, Amount)>> {
    let mut totals = BTreeMap::<u32, (Amount, Amount)>::new();
    for id in candidate_models(state.value(b"c")?)?.into_keys() {
        check()?;
        let start = candidate_key(id, b's');
        let end = candidate_key(id, b't');
        let mut coins = BTreeMap::<u32, (usize, Stake)>::new();
        let mut read = |key: &[u8], raw: &[u8]| {
            check()?;
            let index = stake_index(&key[start.len()..])?;
            ensure!(index < MAX_STAKES, "candidate stake index out of bounds");
            let stake = Stake::decode(full_list(raw)?)?;
            if stake.owner == owner
                && coins
                    .get(&stake.coin)
                    .is_none_or(|(prior, _)| index > *prior)
            {
                // Go walks slots in numeric order and keeps the last one per coin.
                coins.insert(stake.coin, (index, stake));
            }
            Ok(())
        };
        state.visit_range(start.as_slice()..end.as_slice(), &mut read)?;
        for row in retained_in_range(retained, &start, &end) {
            read(&row.key, &row.value)?;
        }
        for (coin, (_, stake)) in coins {
            let (value, bip) = totals.entry(coin).or_default();
            *value += stake.value;
            *bip += stake.bip;
        }
    }
    Ok(totals)
}

pub(crate) struct View<'a> {
    pub store: &'a BlockStore,
    pub height: u64,
    pub candidates: Option<&'a Candidates>,
    pub waitlist: Option<&'a Waitlist>,
    pub validator_count: Option<usize>,
    pub retained: &'a [RetainedCandidateRow],
}

pub(crate) enum DelegationLimit {
    Low,
    Concentrated,
}

#[derive(Default)]
pub(crate) struct CandidateId {
    pub id: u32,
    pub active: bool,
}

impl View<'_> {
    pub fn id(&self, key: PublicKey) -> Result<CandidateId> {
        if let Some(candidates) = self.candidates {
            return Ok(candidates.id(key));
        }
        for (path, active) in [(ACTIVE_IDS, true), (DELETED_IDS, false)] {
            if let Some(raw) = self.store.value_at(self.height, path)?
                && let Some(&id) = decode_ids(&raw, active)?.get(&key)
            {
                return Ok(CandidateId { id, active });
            }
        }
        Ok(CandidateId::default())
    }

    pub fn is_blocked(&self, key: PublicKey) -> Result<bool> {
        if let Some(candidates) = self.candidates {
            return Ok(candidates.blocked.contains(&key));
        }
        Ok(match self.store.value_at(self.height, BLOCKED_KEYS)? {
            Some(raw) => decode_blocked(&raw)?.contains(&key),
            None => false,
        })
    }

    pub fn can_declare(&self, coins: &coins::View<'_>, coin: u32, stake: &Amount) -> Result<bool> {
        let committed;
        let candidates = if let Some(candidates) = self.candidates {
            candidates
        } else {
            committed = Candidates::load(self.store, self.height, self.retained)?;
            &committed
        };
        if candidates.candidates.len() < DECLARATION_STAKE_CHECK_COUNT {
            return Ok(true);
        }
        let cutoff = candidates.ranked()[DECLARATION_STAKE_CHECK_COUNT - 1];
        let value = candidates.estimate_stake(coins, coin, stake)?;
        Ok(value > candidates.candidates[&cutoff].total)
    }

    pub fn candidate(&self, id: u32) -> Result<Option<Cow<'_, Candidate>>> {
        let Some(mut candidate) = self.candidate_model(id)? else {
            return Ok(None);
        };
        if let Cow::Owned(candidate) = &mut candidate {
            candidate.stakes.resize_with(MAX_STAKES, || None);
            let start = candidate_key(id, b's');
            let end = candidate_key(id, b'v');
            self.store.visit_range_at(
                self.height,
                start.as_slice()..end.as_slice(),
                |key, value| candidate.load_field(key[5], &key[6..], value),
            )?;
            for row in retained_in_range(self.retained, &start, &end) {
                candidate.load_field(row.key[5], &row.key[6..], &row.value)?;
            }
        }
        Ok(Some(candidate))
    }

    pub fn owner(&self, key: PublicKey) -> Result<Option<Address>> {
        let id = self.id(key)?;
        if !id.active {
            return Ok(None);
        }
        Ok(Some(
            self.candidate_model(id.id)?
                .context("candidate model is missing")?
                .owner,
        ))
    }

    pub fn candidate_model(&self, id: u32) -> Result<Option<Cow<'_, Candidate>>> {
        if let Some(candidates) = self.candidates {
            return Ok(candidates.candidates.get(&id).map(Cow::Borrowed));
        }
        let Some(raw) = self.store.value_at(self.height, b"c")? else {
            return Ok(None);
        };
        let mut rows = full_list(&raw)?;
        while !rows.is_empty() {
            let fields = Header::decode_bytes(&mut rows, true)?;
            let candidate = Candidate::decode(fields)?;
            if candidate.id == id {
                return Ok(Some(Cow::Owned(candidate)));
            }
        }
        Ok(None)
    }

    pub fn waiting(&self, owner: Address, candidate: u32, coin: u32) -> Result<Option<Amount>> {
        if candidate == 0 {
            return Ok(None);
        }
        let items = match self.waitlist.and_then(|list| list.0.get(&owner)) {
            Some(items) => Cow::Borrowed(items.as_slice()),
            None => Cow::Owned(Waitlist::load(self.store, self.height, owner)?),
        };
        Ok(items
            .iter()
            .find(|item| item.matches(candidate, coin))
            .map(|item| item.value.clone()))
    }

    pub fn delegation_limit(
        &self,
        coins: &coins::View<'_>,
        id: u32,
        owner: Address,
        coin: u32,
        value: &Amount,
    ) -> Result<Option<DelegationLimit>> {
        let committed;
        let candidates = if let Some(candidates) = self.candidates {
            candidates
        } else {
            committed = Candidates::load(self.store, self.height, self.retained)?;
            &committed
        };
        let candidate = candidates
            .candidates
            .get(&id)
            .context("missing delegation candidate")?;
        let bip_value = candidates.estimate_stake(coins, coin, value)?;
        let mut previous = Amount::ZERO;
        if candidate.stakes.iter().flatten().count() == MAX_STAKES {
            let matching = candidate.stakes.iter().flatten().find(|stake| {
                bip_value > stake.bip || (stake.owner == owner && stake.coin == coin)
            });
            let Some(stake) = matching else {
                return Ok(Some(DelegationLimit::Low));
            };
            previous = stake.bip.clone();
        }
        let count = match self.validator_count {
            Some(count) => count,
            None => load_validators(self.store, self.height)?.len(),
        };
        if count < 4 {
            return Ok(None);
        }
        let delta = BigInt::from(bip_value) - BigInt::from(previous);
        let total: Amount = candidates.candidates.values().map(|c| &c.total).sum();
        let projected = BigInt::from(candidate.total.clone()) + &delta;
        ensure!(
            projected > BigInt::ZERO,
            "delegation leaves a zero candidate total"
        );
        Ok((BigInt::from(total) + delta < projected * 5).then_some(DelegationLimit::Concentrated))
    }
}

impl Candidates {
    pub fn load(
        store: &BlockStore,
        height: u64,
        retained: &[RetainedCandidateRow],
    ) -> Result<Self> {
        let mut candidates = candidate_models(store.value_at(height, b"c")?)?;
        for candidate in candidates.values_mut() {
            candidate.stakes.resize_with(MAX_STAKES, || None);
        }
        let mut load = |key: &[u8], value: &[u8]| {
            let Some((id, field, suffix)) = candidate_row(key) else {
                return Ok(());
            };
            let Some(candidate) = candidates.get_mut(&id) else {
                return Ok(());
            };
            candidate.load_field(field, suffix, value)
        };
        store.visit_range_at(height, b"c\0".as_slice()..b"d".as_slice(), &mut load)?;
        for row in retained {
            load(&row.key, &row.value)?;
        }
        let read_ids = |path, active| -> Result<_> {
            store
                .value_at(height, path)?
                .map(|raw| decode_ids(&raw, active))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        let active = read_ids(ACTIVE_IDS, true)?;
        let deleted = read_ids(DELETED_IDS, false)?;
        let blocked = store
            .value_at(height, BLOCKED_KEYS)?
            .map(|raw| decode_blocked(&raw))
            .transpose()?
            .unwrap_or_default();
        let max_id = match store.value_at(height, MAX_ID)? {
            Some(raw) => u32::from_le_bytes(raw.as_slice().try_into()?),
            None => active.values().copied().max().unwrap_or_default(),
        };
        Ok(Self {
            candidates,
            active,
            deleted,
            blocked,
            max_id,
            ..Default::default()
        })
    }

    fn id(&self, key: PublicKey) -> CandidateId {
        match self.active.get(&key) {
            Some(&id) => CandidateId { id, active: true },
            None => CandidateId {
                id: self.deleted.get(&key).copied().unwrap_or_default(),
                active: false,
            },
        }
    }

    fn estimate_stake(&self, coins: &coins::View<'_>, coin: u32, value: &Amount) -> Result<Amount> {
        if coin == 0 || value.is_zero() {
            return Ok(value.clone());
        }
        let delegated = self
            .candidates
            .values()
            .flat_map(|candidate| candidate.stakes.iter().flatten().chain(&candidate.updates))
            .filter(|stake| stake.coin == coin)
            .fold(value.clone(), |total, stake| total + &stake.value);
        let supply = coins.supply(coin)?;
        let ratio = coins.coin(coin)?.context("missing stake coin")?.ratio;
        let reserve =
            bancor::delegated_reserve(&supply.volume, &supply.reserve, ratio, &delegated)?;
        Ok(reserve * value / delegated)
    }

    pub fn declare(
        &mut self,
        key: PublicKey,
        owner: Address,
        sender: Address,
        commission: u32,
        height: u64,
    ) -> Result<u32> {
        let id = self
            .max_id
            .checked_add(1)
            .context("candidate ID overflow")?;
        self.max_id = id;
        self.active.insert(key, id);
        self.catalog_dirty = true;
        self.candidates.insert(
            id,
            Candidate {
                key,
                reward_address: sender,
                owner,
                control: sender,
                commission,
                jailed_until: 0,
                status: 1,
                id,
                last_edit_commission: height,
                model_dirty: true,
                total: Amount::ZERO,
                total_dirty: true,
                updates_dirty: false,
                stakes: vec![None; MAX_STAKES],
                updates: Vec::new(),
            },
        );
        Ok(id)
    }

    pub fn get_mut(&mut self, key: PublicKey) -> Result<&mut Candidate> {
        self.find_mut(key)
            .context("validator has no candidate record")
    }

    pub fn change_key(&mut self, old: PublicKey, new: PublicKey) -> Result<()> {
        let id = self
            .active
            .remove(&old)
            .context("missing candidate key to replace")?;
        self.active.insert(new, id);
        self.blocked.insert(old);
        let candidate = self
            .candidates
            .get_mut(&id)
            .context("missing candidate model")?;
        candidate.key = new;
        candidate.model_dirty = true;
        self.catalog_dirty = true;
        Ok(())
    }

    pub fn find_mut(&mut self, key: PublicKey) -> Option<&mut Candidate> {
        self.candidates
            .values_mut()
            .find(|candidate| candidate.key == key)
    }

    pub fn delegate(
        &mut self,
        id: u32,
        owner: Address,
        coin: u32,
        value: Amount,
    ) -> Result<PublicKey> {
        let candidate = self
            .candidates
            .get_mut(&id)
            .with_context(|| format!("stake destination {id} is missing"))?;
        candidate.updates.push(Stake {
            owner,
            coin,
            value,
            bip: Amount::ZERO,
            dirty: false,
        });
        candidate.updates_dirty = true;
        Ok(candidate.key)
    }

    pub fn subtract_stake(
        &mut self,
        id: u32,
        owner: Address,
        coin: u32,
        value: &Amount,
    ) -> Result<()> {
        let candidate = self
            .candidates
            .get_mut(&id)
            .context("missing stake candidate")?;
        let stake = candidate
            .stakes
            .iter_mut()
            .flatten()
            .find(|stake| stake.owner == owner && stake.coin == coin)
            .context("missing active stake")?;
        ensure!(&stake.value >= value, "withdrawal exceeds active stake");
        stake.value -= value;
        stake.dirty = true;
        Ok(())
    }

    pub fn reward_owners(&self, validators: &[Validator]) -> BTreeSet<Address> {
        let keys: BTreeSet<_> = validators.iter().map(|v| v.key).collect();
        self.candidates
            .values()
            .filter(|candidate| keys.contains(&candidate.key))
            .flat_map(|candidate| candidate.stakes.iter().flatten())
            .filter(|stake| !stake.bip.is_zero())
            .map(|stake| stake.owner)
            .collect()
    }

    fn ranked(&self) -> Vec<u32> {
        let mut ids: Vec<_> = self.candidates.keys().copied().collect();
        ids.sort_unstable_by(|left, right| {
            self.candidates[right]
                .total
                .cmp(&self.candidates[left].total)
                .then_with(|| right.cmp(left))
        });
        ids
    }

    pub fn correct_v330_stakes(&mut self) -> Result<BigInt> {
        // The 45 entries are the consensus constants in Go's FixStakesAfter10509400.
        let corrections: BTreeMap<&str, BTreeMap<&str, &str>> =
            serde_json::from_str(include_str!("v330-stake-corrections.json"))?;
        let mut excess = BigInt::ZERO;
        for candidate in self.candidates.values_mut() {
            let key = display_key(candidate.key);
            let amounts = corrections.get(key.as_str());
            let mut candidate_excess = BigInt::ZERO;
            let mut found_dao = false;
            let mut found_developers = false;
            for stake in candidate.stakes.iter_mut().flatten() {
                match stake.owner {
                    DAO => found_dao = true,
                    DEVELOPERS => found_developers = true,
                    _ => continue,
                }
                let address = display_address(stake.owner);
                let amount: Amount = amounts
                    .and_then(|row| row.get(address.as_str()))
                    .with_context(|| format!("missing v330 stake correction for {key}/{address}"))?
                    .parse()?;
                candidate_excess += BigInt::from(stake.take_value()) - BigInt::from(amount.clone());
                stake.value = amount.clone();
                stake.bip = amount;
                if found_dao && found_developers {
                    break;
                }
            }
            if !candidate_excess.is_zero() {
                candidate.total = (BigInt::from(candidate.total.clone()) - &candidate_excess)
                    .to_biguint()
                    .context("negative corrected candidate stake")?;
                candidate.total_dirty = true;
                excess += candidate_excess;
            }
        }
        Ok(excess)
    }

    pub fn recalculate(
        &mut self,
        coins: &coins::View<'_>,
        waitlist: &mut Waitlist,
        events: &mut Vec<Any>,
    ) -> Result<()> {
        let mut delegated = BTreeMap::<u32, Amount>::new();
        for candidate in self.candidates.values() {
            for stake in candidate.stakes.iter().flatten().chain(&candidate.updates) {
                if stake.coin != 0 && !stake.value.is_zero() {
                    *delegated.entry(stake.coin).or_default() += &stake.value;
                }
            }
        }
        // A coin's first valuation precedes every kick of a positive stake
        // in that coin. The original delegated totals give Go's cache inputs.
        let mut values = BTreeMap::new();
        for (id, amount) in delegated {
            let coin = coins
                .coin(id)?
                .with_context(|| format!("unknown stake coin {id}"))?;
            let supply = coins.supply(id)?;
            ensure!(
                amount <= supply.volume,
                "delegated amount exceeds supply of coin {id}"
            );
            let reserve =
                bancor::delegated_reserve(&supply.volume, &supply.reserve, coin.ratio, &amount)
                    .with_context(|| format!("value stakes of coin {id}"))?;
            values.insert(id, (reserve, amount));
        }
        for id in self.ranked() {
            self.candidates
                .get_mut(&id)
                .context("missing ranked candidate")?
                .recalculate(&values, waitlist, coins.store, coins.height, events)?;
        }
        Ok(())
    }

    pub fn validators(&self, old: &[Validator]) -> Vec<Validator> {
        let minimum = Amount::from(1_000_000_000_000_000_000_000u128);
        self.ranked()
            .into_iter()
            .map(|id| &self.candidates[&id])
            .filter(|candidate| candidate.status == 2 && candidate.total >= minimum)
            .take(MAX_VALIDATORS)
            .map(|candidate| {
                let prior = old.iter().find(|v| v.key == candidate.key);
                Validator {
                    key: candidate.key,
                    absent: prior.map_or(0, |v| v.absent),
                    stake: candidate.total.clone(),
                    reward: prior.map_or_else(Amount::default, |v| v.reward.clone()),
                    dirty: true,
                    reward_dirty: true,
                }
            })
            .collect()
    }

    pub fn remove_excess(&mut self, validators: &[Validator]) -> Vec<Candidate> {
        if self.candidates.len() <= MAX_CANDIDATES {
            return Vec::new();
        }
        let mut ids: Vec<_> = self.candidates.keys().copied().collect();
        // Pruning gives ties to the lower ID, unlike validator selection.
        ids.sort_unstable_by(|left, right| {
            self.candidates[right]
                .total
                .cmp(&self.candidates[left].total)
                .then_with(|| left.cmp(right))
        });
        let mut removed = Vec::new();
        for id in ids.into_iter().skip(MAX_CANDIDATES) {
            let key = self.candidates[&id].key;
            if validators.iter().any(|validator| validator.key == key) {
                continue;
            }
            self.active.remove(&key);
            self.blocked.insert(key);
            self.deleted.insert(key, id);
            self.removed.insert(id);
            self.catalog_dirty = true;
            removed.push(self.candidates.remove(&id).expect("ranked candidate"));
        }
        removed
    }

    pub fn write(
        &self,
        store: &BlockStore,
        height: u64,
        ops: &mut Vec<Mutation>,
        retained: &mut Vec<RetainedCandidateRow>,
    ) -> Result<()> {
        let mut rows: BTreeMap<_, _> = retained
            .drain(..)
            .filter(|row| {
                candidate_row(&row.key).is_some_and(|(id, _, _)| self.candidates.contains_key(&id))
            })
            .map(|row| (row.key, row.value))
            .collect();
        let ranked = self.ranked();
        if !self.removed.is_empty()
            || self
                .candidates
                .values()
                .any(|candidate| candidate.model_dirty)
        {
            ops.push(set(
                b"c".to_vec(),
                list(|out| {
                    for id in &ranked {
                        out.extend(self.candidates[id].encode());
                    }
                }),
            ));
        }
        if self.catalog_dirty {
            ops.push(set(ACTIVE_IDS.to_vec(), encode_ids(&self.active, true)));
            ops.push(set(
                BLOCKED_KEYS.to_vec(),
                list(|out| {
                    for key in self.blocked.iter().rev() {
                        key.encode(out);
                    }
                }),
            ));
            ops.push(set(MAX_ID.to_vec(), self.max_id.to_le_bytes().to_vec()));
        }
        if !self.removed.is_empty() {
            for &id in &self.removed {
                let start = [b"c".as_slice(), &id.to_le_bytes()].concat();
                let end = [b"c".as_slice(), &id.wrapping_add(1).to_le_bytes()].concat();
                // Go scans these little-endian ID bounds. The range is empty
                // when the first byte wraps, so that ID's old rows remain.
                if start < end {
                    store.visit_range_at(
                        height,
                        start.as_slice()..end.as_slice(),
                        |key, value| {
                            if let Some((id, b's' | b't' | b'u', _)) = candidate_row(key) {
                                ops.push(Mutation::Remove(key.to_vec()));
                                // This Go range can include another ID. Go keeps
                                // that candidate's rows in memory after Commit.
                                if self.candidates.contains_key(&id) {
                                    rows.insert(key.to_vec(), value.to_vec());
                                }
                            }
                            Ok(())
                        },
                    )?;
                }
            }
        }
        if !self.removed.is_empty() || (store.head().is_none() && !self.deleted.is_empty()) {
            ops.push(set(DELETED_IDS.to_vec(), encode_ids(&self.deleted, false)));
        }
        let first_candidate_write = ops.len();
        for id in ranked {
            let candidate = &self.candidates[&id];
            if candidate.total_dirty {
                ops.push(set(candidate_key(id, b't'), magnitude(&candidate.total)));
            }
            for (index, stake) in candidate.stakes.iter().enumerate() {
                if let Some(stake) = stake
                    && stake.dirty
                {
                    let key = stake_key(id, index);
                    if stake.value.is_zero() {
                        ops.push(Mutation::Remove(key));
                    } else {
                        ops.push(set(key, stake.encode()));
                    }
                }
            }
            if candidate.updates_dirty {
                ops.push(set(
                    candidate_key(id, b'u'),
                    list(|out| {
                        for update in &candidate.updates {
                            out.extend(update.encode());
                        }
                    }),
                ));
            }
        }
        for operation in &ops[first_candidate_write..] {
            let key = match operation {
                Mutation::Set { key, .. } | Mutation::Remove(key) => key,
            };
            rows.remove(key);
        }
        retained.extend(
            rows.into_iter()
                .map(|(key, value)| RetainedCandidateRow { key, value }),
        );
        Ok(())
    }
}

fn encode_ids(ids: &BTreeMap<PublicKey, u32>, active: bool) -> Vec<u8> {
    let mut ids: Vec<_> = ids.iter().collect();
    ids.sort_unstable_by_key(|(_, id)| **id);
    list(|out| {
        for (key, id) in ids {
            out.extend(list(|out| {
                if active {
                    key.encode(out);
                    id.encode(out);
                } else {
                    id.encode(out);
                    key.encode(out);
                }
            }));
        }
    })
}

type CoinValues = BTreeMap<u32, (Amount, Amount)>;

impl Candidate {
    pub fn last_edit_commission(&self) -> u64 {
        self.last_edit_commission
    }

    pub fn set_commission(&mut self, commission: u32, height: u64) {
        self.commission = commission;
        self.last_edit_commission = height;
        self.model_dirty = true;
    }

    pub fn set_addresses(&mut self, owner: Address, reward: Address, control: Address) {
        self.owner = owner;
        self.reward_address = reward;
        self.control = control;
        self.model_dirty = true;
    }

    fn decode(mut fields: &[u8]) -> Result<Self> {
        let key = PublicKey::decode(&mut fields)?;
        let reward_address = Address::decode(&mut fields)?;
        let owner = Address::decode(&mut fields)?;
        let control = Address::decode(&mut fields)?;
        let commission = u32::decode(&mut fields)?;
        let status = u8::decode(&mut fields)?;
        let id = u32::decode(&mut fields)?;
        let last_edit_commission = u64::decode(&mut fields)?;
        let jailed_until = u64::decode(&mut fields)?;
        finish(fields)?;
        ensure!(commission <= 100 && id != 0, "invalid candidate");
        Ok(Self {
            key,
            reward_address,
            owner,
            control,
            commission,
            jailed_until,
            status,
            id,
            last_edit_commission,
            model_dirty: false,
            stakes: Vec::new(),
            total: Amount::ZERO,
            total_dirty: false,
            updates_dirty: false,
            updates: Vec::new(),
        })
    }

    fn encode(&self) -> Vec<u8> {
        list(|out| {
            self.key.encode(out);
            self.reward_address.encode(out);
            self.owner.encode(out);
            self.control.encode(out);
            self.commission.encode(out);
            self.status.encode(out);
            self.id.encode(out);
            self.last_edit_commission.encode(out);
            self.jailed_until.encode(out);
        })
    }

    pub fn set_status(&mut self, online: bool, jail: Option<u64>) {
        self.status = if online { 2 } else { 1 };
        if let Some(height) = jail {
            self.jailed_until = height;
        }
        self.model_dirty = true;
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn is_offline(&self) -> bool {
        self.status == 1
    }

    pub fn into_frozen_funds(self) -> impl Iterator<Item = frozen::Fund> {
        let source = Some((self.key, self.id));
        self.stakes
            .into_iter()
            .flatten()
            .chain(self.updates)
            .map(move |stake| frozen::Fund::new(stake.owner, stake.coin, stake.value, source, 0))
    }

    fn load_field(&mut self, field: u8, suffix: &[u8], value: &[u8]) -> Result<()> {
        match (field, suffix) {
            (b't', []) => self.total = Amount::from_bytes_be(value),
            (b'u', []) => {
                let mut updates = full_list(value)?;
                while !updates.is_empty() {
                    self.updates
                        .push(Stake::decode(Header::decode_bytes(&mut updates, true)?)?);
                }
            }
            (b's', suffix) => {
                let index = stake_index(suffix)?;
                let slot = self
                    .stakes
                    .get_mut(index)
                    .context("candidate stake index out of bounds")?;
                *slot = Some(Stake::decode(full_list(value)?)?);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn stake_value(&self, owner: Address, coin: u32) -> Option<&Amount> {
        self.stakes
            .iter()
            .flatten()
            .find(|stake| stake.owner == owner && stake.coin == coin)
            .map(|stake| &stake.value)
    }

    pub fn add_reward(&mut self, owner: Address, value: Amount) {
        self.updates.push(Stake {
            owner,
            coin: 0,
            bip: value.clone(),
            value,
            dirty: false,
        });
    }

    fn recalculate(
        &mut self,
        values: &CoinValues,
        waitlist: &mut Waitlist,
        store: &BlockStore,
        height: u64,
        events: &mut Vec<Any>,
    ) -> Result<()> {
        let mut positions = BTreeMap::new();
        for (index, slot) in self.stakes.iter_mut().enumerate() {
            if let Some(stake) = slot {
                stake.revalue(values);
                positions.entry((stake.owner, stake.coin)).or_insert(index);
            }
        }
        self.updates_dirty = !self.updates.is_empty();
        let mut new: Vec<Stake> = Vec::new();
        let mut pending = BTreeMap::new();
        for update in self.updates.drain(..) {
            let key = (update.owner, update.coin);
            if let Some(index) = positions.get(&key) {
                let stake = self.stakes[*index]
                    .as_mut()
                    .context("missing existing stake")?;
                stake.value += update.value;
                stake.dirty = true;
                stake.revalue(values);
            } else if !update.value.is_zero() {
                match pending.entry(key) {
                    Entry::Occupied(entry) => {
                        let index: usize = *entry.get();
                        new[index].value += update.value;
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(new.len());
                        new.push(update);
                    }
                }
            }
        }
        new.sort_by(|left, right| right.bip.cmp(&left.bip));
        for mut stake in new {
            stake.revalue(values);
            let index = self
                .stakes
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.as_ref().map(|stake| &stake.bip))
                .context("candidate has no stake slots")?
                .0;
            let slot = &mut self.stakes[index];
            let kicked = if slot
                .as_ref()
                .is_some_and(|smallest| smallest.bip > stake.bip)
            {
                Some(stake)
            } else {
                stake.dirty = true;
                slot.replace(stake)
            };
            if let Some(kicked) = kicked {
                events.push(pack(StakeKickEvent {
                    address: display_address(kicked.owner),
                    amount: kicked.value.to_string(),
                    coin: u64::from(kicked.coin),
                    validator_pub_key: display_key(self.key),
                }));
                waitlist.add(store, height, self.id, kicked)?;
            }
        }
        let total: Amount = self.stakes.iter().flatten().map(|stake| &stake.bip).sum();
        self.total_dirty |= self.total != total;
        self.total = total;
        Ok(())
    }
}

impl Stake {
    fn revalue(&mut self, values: &CoinValues) {
        let bip = if self.coin == 0 {
            self.value.clone()
        } else if self.value.is_zero() {
            Amount::ZERO
        } else {
            let (bip, amount) = &values[&self.coin];
            bip * &self.value / amount
        };
        self.dirty |= self.bip != bip;
        self.bip = bip;
    }
}

#[derive(Default)]
pub(crate) struct Waitlist(BTreeMap<Address, Vec<WaitItem>>);

#[derive(Clone)]
struct WaitItem {
    candidate: u32,
    coin: u32,
    value: Amount,
}

impl WaitItem {
    fn matches(&self, candidate: u32, coin: u32) -> bool {
        self.candidate == candidate && self.coin == coin
    }
}

impl Waitlist {
    fn load(store: &BlockStore, height: u64, owner: Address) -> Result<Vec<WaitItem>> {
        let Some(raw) = store.value_at(height, &waitlist_key(owner))? else {
            return Ok(Vec::new());
        };
        let mut record = full_list(&raw)?;
        let mut rows = Header::decode_bytes(&mut record, true)?;
        finish(record)?;
        let mut items = Vec::new();
        while !rows.is_empty() {
            let mut row = Header::decode_bytes(&mut rows, true)?;
            items.push(WaitItem {
                candidate: u32::decode(&mut row)?,
                coin: u32::decode(&mut row)?,
                value: amount(&mut row)?,
            });
            finish(row)?;
        }
        Ok(items)
    }

    fn items(
        &mut self,
        store: &BlockStore,
        height: u64,
        owner: Address,
    ) -> Result<&mut Vec<WaitItem>> {
        Ok(match self.0.entry(owner) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Self::load(store, height, owner)?),
        })
    }

    fn add(&mut self, store: &BlockStore, height: u64, candidate: u32, stake: Stake) -> Result<()> {
        let items = if store.head().is_none() {
            self.0.entry(stake.owner).or_default()
        } else {
            self.items(store, height, stake.owner)?
        };
        Self::add_item(items, candidate, stake.coin, stake.value);
        Ok(())
    }

    pub fn import(&mut self, owner: Address, candidate: u32, coin: u32, value: Amount) {
        Self::add_item(self.0.entry(owner).or_default(), candidate, coin, value);
    }

    fn add_item(items: &mut Vec<WaitItem>, candidate: u32, coin: u32, value: Amount) {
        if let Some(item) = items.iter_mut().find(|item| item.matches(candidate, coin)) {
            item.value += value;
        } else {
            items.push(WaitItem {
                candidate,
                coin,
                value,
            });
        }
    }

    pub fn withdraw(
        &mut self,
        store: &BlockStore,
        height: u64,
        owner: Address,
        candidate: u32,
        coin: u32,
        value: &Amount,
    ) -> Result<()> {
        let items = self.items(store, height, owner)?;
        let previous = &items
            .iter()
            .find(|item| item.matches(candidate, coin))
            .context("missing waitlisted stake")?
            .value;
        let remainder = (previous > value).then(|| previous - value);
        items.retain(|item| !item.matches(candidate, coin));
        if let Some(value) = remainder {
            // Go deletes the item and appends any remainder at the end of the list.
            items.push(WaitItem {
                candidate,
                coin,
                value,
            });
        }
        Ok(())
    }

    pub fn write(self, ops: &mut Vec<Mutation>) {
        for (owner, items) in self.0.into_iter().rev() {
            if items.is_empty() {
                ops.push(Mutation::Remove(waitlist_key(owner)));
                continue;
            }
            ops.push(set(
                waitlist_key(owner),
                list(|out| {
                    out.extend(list(|out| {
                        for item in items {
                            out.extend(list(|out| {
                                item.candidate.encode(out);
                                item.coin.encode(out);
                                put_amount(&item.value, out);
                            }));
                        }
                    }));
                }),
            ));
        }
    }
}

fn waitlist_key(owner: Address) -> Vec<u8> {
    [b"w".as_slice(), &owner].concat()
}
