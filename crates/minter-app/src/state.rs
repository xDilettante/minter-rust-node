use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::{Context, Result, ensure};
use minter_protocol::{
    Address,
    state_keys::{account_key, balance_key},
};
use minter_state::{BlockStore, Mutation};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

pub(crate) type Amount = BigUint;
pub(crate) type PublicKey = [u8; 32];

pub(crate) fn display_key(key: PublicKey) -> String {
    format!("Mp{}", hex::encode(key))
}

pub(crate) const ABSENCE_WINDOW: u32 = 24;

pub(crate) fn set(key: Vec<u8>, value: Vec<u8>) -> Mutation {
    Mutation::Set { key, value }
}

pub(crate) fn magnitude(value: &Amount) -> Vec<u8> {
    if value.bits() == 0 {
        Vec::new()
    } else {
        value.to_bytes_be()
    }
}

pub(crate) fn put_amount(value: &Amount, out: &mut Vec<u8>) {
    magnitude(value).as_slice().encode(out);
}

pub(crate) fn list(encode: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut body = Vec::new();
    encode(&mut body);
    let mut encoded = Vec::new();
    Header {
        list: true,
        payload_length: body.len(),
    }
    .encode(&mut encoded);
    encoded.extend(body);
    encoded
}

pub(crate) fn full_list(mut input: &[u8]) -> Result<&[u8]> {
    let list = Header::decode_bytes(&mut input, true)?;
    finish(input)?;
    Ok(list)
}

pub(crate) fn finish(input: &[u8]) -> Result<()> {
    ensure!(input.is_empty(), "unexpected trailing RLP fields");
    Ok(())
}

pub(crate) fn amount(input: &mut &[u8]) -> Result<Amount> {
    let bytes = Header::decode_bytes(input, false)?;
    ensure!(bytes.first() != Some(&0), "noncanonical RLP integer");
    Ok(Amount::from_bytes_be(bytes))
}

#[derive(Default)]
pub(crate) struct Account {
    address: Address,
    pub nonce: u64,
    pub multisig: Multisig,
    pub lock_stake_until: u64,
    exists: bool,
    coins: Vec<u32>,
    balances: BTreeMap<u32, Balance>,
    model_dirty: bool,
    coins_dirty: bool,
}

#[derive(Default)]
struct Balance {
    amount: Amount,
    dirty: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Multisig {
    pub threshold: u32,
    pub weights: Vec<u32>,
    pub addresses: Vec<Address>,
}

impl Multisig {
    fn decode(fields: &mut &[u8]) -> Result<Self> {
        let mut data = Header::decode_bytes(fields, true)?;
        let multisig = Self {
            threshold: u32::decode(&mut data)?,
            weights: Vec::decode(&mut data)?,
            addresses: Vec::decode(&mut data)?,
        };
        finish(data)?;
        ensure!(
            multisig.weights.len() == multisig.addresses.len(),
            "invalid multisig weights"
        );
        Ok(multisig)
    }

    fn encode(&self) -> Vec<u8> {
        list(|out| {
            self.threshold.encode(out);
            self.weights.encode(out);
            self.addresses.encode(out);
        })
    }
}

impl Account {
    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Self::default()
        }
    }

    pub fn load(store: &BlockStore, height: u64, address: Address) -> Result<Self> {
        Self::load_from(address, |key| store.value_at(height, key))
    }

    pub fn load_from(
        address: Address,
        read: impl Fn(&[u8]) -> Result<Option<Vec<u8>>>,
    ) -> Result<Self> {
        let Some(raw) = read(&account_key(address, b""))? else {
            return Ok(Self::new(address));
        };
        let (nonce, multisig, lock_stake_until) = Self::decode_model(&raw)?;
        let mut coins = Vec::new();
        if let Some(raw) = read(&account_key(address, b"c"))? {
            let mut ids = full_list(&raw)?;
            while !ids.is_empty() {
                coins.push(u32::decode(&mut ids)?);
            }
        }
        Ok(Self {
            address,
            exists: true,
            nonce,
            multisig,
            lock_stake_until,
            coins,
            ..Self::default()
        })
    }

    fn decode_model(raw: &[u8]) -> Result<(u64, Multisig, u64)> {
        let mut fields = full_list(raw)?;
        let nonce = u64::decode(&mut fields)?;
        let multisig = Multisig::decode(&mut fields)?;
        let lock_stake_until = u64::decode(&mut fields)?;
        finish(fields)?;
        Ok((nonce, multisig, lock_stake_until))
    }

    pub fn locked_stakes_at(
        store: &BlockStore,
        prior_height: u64,
        height: u64,
        owners: BTreeSet<Address>,
        accounts: &BTreeMap<Address, Self>,
    ) -> Result<BTreeSet<Address>> {
        let mut locked = BTreeSet::new();
        let mut keys = Vec::new();
        for owner in owners {
            if let Some(account) = accounts.get(&owner) {
                if height < account.lock_stake_until {
                    locked.insert(owner);
                }
            } else {
                keys.push(account_key(owner, b""));
            }
        }
        store.visit_keys_at(prior_height, &keys, |key, value| {
            if height < Self::decode_model(value)?.2 {
                locked.insert(key[1..].try_into()?);
            }
            Ok(())
        })?;
        Ok(locked)
    }

    pub fn balance(&mut self, store: &BlockStore, height: u64, coin: u32) -> Result<&Amount> {
        let balance = match self.balances.entry(coin) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let amount = if self.coins.contains(&coin) {
                    stored_balance(self.address, coin, |key| store.value_at(height, key))?
                } else {
                    Amount::default()
                };
                entry.insert(Balance {
                    amount,
                    dirty: false,
                })
            }
        };
        Ok(&balance.amount)
    }

    pub fn stored_balances(
        &self,
        read: impl Fn(&[u8]) -> Result<Option<Vec<u8>>>,
    ) -> Result<Vec<(u32, Amount)>> {
        self.coins
            .iter()
            .map(|&coin| Ok((coin, stored_balance(self.address, coin, &read)?)))
            .collect()
    }

    pub fn set_nonce(&mut self, nonce: u64) {
        self.nonce = nonce;
        self.model_dirty = true;
    }

    pub fn is_multisig(&self) -> bool {
        !self.multisig.weights.is_empty()
    }

    pub fn set_multisig(&mut self, model: Multisig) {
        self.multisig = model;
        self.model_dirty = true;
    }

    pub fn set_stake_lock(&mut self, height: u64) {
        self.lock_stake_until = height;
        self.model_dirty = true;
    }

    pub fn set_balance(&mut self, coin: u32, balance: Amount) {
        let has_coin = self.coins.contains(&coin);
        if balance.bits() == 0 {
            if !has_coin {
                return;
            }
            self.coins.retain(|existing| *existing != coin);
            self.coins_dirty = true;
        } else if !has_coin {
            self.coins.push(coin);
            self.coins_dirty = true;
        }
        self.balances.insert(
            coin,
            Balance {
                amount: balance,
                dirty: true,
            },
        );
    }

    pub fn add_balance(
        &mut self,
        store: &BlockStore,
        height: u64,
        coin: u32,
        value: &Amount,
    ) -> Result<()> {
        let balance = self.balance(store, height, coin)? + value;
        self.set_balance(coin, balance);
        Ok(())
    }

    pub fn sub_balance(
        &mut self,
        store: &BlockStore,
        height: u64,
        coin: u32,
        value: &Amount,
    ) -> Result<()> {
        let balance = self.balance(store, height, coin)?;
        ensure!(balance >= value, "insufficient balance for coin {coin}");
        let balance = balance - value;
        self.set_balance(coin, balance);
        Ok(())
    }

    pub fn write(&self, ops: &mut Vec<Mutation>) {
        if self.model_dirty || (!self.exists && self.coins_dirty) {
            let raw = list(|out| {
                self.nonce.encode(out);
                out.extend(self.multisig.encode());
                self.lock_stake_until.encode(out);
            });
            ops.push(set(account_key(self.address, b""), raw));
        }
        if self.coins_dirty {
            ops.push(set(
                account_key(self.address, b"c"),
                alloy_rlp::encode(&self.coins),
            ));
        }
        for (&coin, balance) in self
            .balances
            .iter()
            .rev()
            .filter(|(_, balance)| balance.dirty)
        {
            let key = balance_key(self.address, coin);
            if balance.amount.bits() == 0 {
                ops.push(Mutation::Remove(key));
            } else {
                ops.push(set(key, magnitude(&balance.amount)));
            }
        }
    }
}

fn stored_balance(
    address: Address,
    coin: u32,
    read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<Amount> {
    Ok(read(&balance_key(address, coin))?
        .map_or_else(Amount::default, |raw| Amount::from_bytes_be(&raw)))
}

#[derive(Default)]
pub(crate) struct AppValues {
    pub slashed: Amount,
    pub coins: u32,
    pub max_gas: u64,
    pub reward: Amount,
    pub safe_reward: Amount,
    pub dirty: bool,
}

impl AppValues {
    pub fn set_reward(&mut self, reward: Amount, safe: Amount) {
        if safe.bits() != 0 || self.safe_reward.bits() != 0 {
            self.reward = reward;
            self.safe_reward = safe;
            self.dirty = true;
        }
    }

    pub fn load(store: &BlockStore, height: u64) -> Result<Self> {
        Self::load_from(|key| store.value_at(height, key))
    }

    pub fn load_from(read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>) -> Result<Self> {
        let Some(raw) = read(b"d")? else {
            return Ok(Self::default());
        };
        let mut data = full_list(&raw)?;
        let values = Self {
            slashed: amount(&mut data)?,
            coins: u32::decode(&mut data)?,
            max_gas: u64::decode(&mut data)?,
            reward: amount(&mut data)?,
            safe_reward: amount(&mut data)?,
            dirty: false,
        };
        finish(data)?;
        Ok(values)
    }

    pub fn write(&self, ops: &mut Vec<Mutation>) {
        if self.dirty {
            ops.push(set(
                b"d".to_vec(),
                list(|out| {
                    put_amount(&self.slashed, out);
                    self.coins.encode(out);
                    self.max_gas.encode(out);
                    put_amount(&self.reward, out);
                    put_amount(&self.safe_reward, out);
                }),
            ));
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Validator {
    pub key: PublicKey,
    pub absent: u32,
    pub stake: Amount,
    pub reward: Amount,
    #[serde(skip)]
    pub dirty: bool,
    #[serde(skip)]
    pub reward_dirty: bool,
}

impl Validator {
    pub fn address(&self) -> Address {
        let mut address = Address::default();
        address.copy_from_slice(&Sha256::digest(self.key)[..20]);
        address
    }
}

fn validator_key(key: &PublicKey, field: u8) -> Vec<u8> {
    [b"v".as_slice(), key, &[field]].concat()
}

pub(crate) fn load_validators(store: &BlockStore, height: u64) -> Result<Vec<Validator>> {
    let Some(raw) = store.value_at(height, b"v")? else {
        return Ok(Vec::new());
    };
    let mut rows = full_list(&raw)?;
    let mut validators = Vec::new();
    while !rows.is_empty() {
        let mut row = Header::decode_bytes(&mut rows, true)?;
        let key = Header::decode_bytes(&mut row, false)?.try_into()?;
        let mut absence = Header::decode_bytes(&mut row, true)?;
        finish(row)?;
        ensure!(
            u32::decode(&mut absence)? == ABSENCE_WINDOW,
            "invalid validator absence window"
        );
        let mut masks = Header::decode_bytes(&mut absence, true)?;
        let absent = u32::decode(&mut masks)?;
        finish(masks)?;
        finish(absence)?;
        ensure!(
            absent < 1 << ABSENCE_WINDOW,
            "invalid validator absence mask"
        );
        let read = |field| -> Result<Amount> {
            let bytes = store
                .value_at(height, &validator_key(&key, field))?
                .context("missing validator amount")?;
            Ok(Amount::from_bytes_be(&bytes))
        };
        validators.push(Validator {
            key,
            absent,
            stake: read(b's')?,
            reward: read(b'r')?,
            dirty: false,
            reward_dirty: false,
        });
    }
    Ok(validators)
}

pub(crate) fn write_validators(
    validators: &[Validator],
    mut removed: Vec<PublicKey>,
    ops: &mut Vec<Mutation>,
) {
    if !removed.is_empty() || validators.iter().any(|validator| validator.dirty) {
        ops.push(set(
            b"v".to_vec(),
            list(|out| {
                for validator in validators {
                    out.extend(list(|out| {
                        validator.key.as_slice().encode(out);
                        out.extend(list(|out| {
                            ABSENCE_WINDOW.encode(out);
                            out.extend(list(|out| validator.absent.encode(out)));
                        }));
                    }));
                }
            }),
        ));
    }
    for validator in validators {
        if validator.dirty {
            ops.push(set(
                validator_key(&validator.key, b's'),
                magnitude(&validator.stake),
            ));
        }
        if validator.dirty || validator.reward_dirty {
            ops.push(set(
                validator_key(&validator.key, b'r'),
                magnitude(&validator.reward),
            ));
        }
    }
    removed.sort_unstable_by(|left, right| right.cmp(left));
    for key in removed {
        ops.push(Mutation::Remove(validator_key(&key, b's')));
        ops.push(Mutation::Remove(validator_key(&key, b'r')));
    }
}
