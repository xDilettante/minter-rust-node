use crate::state::{Amount, AppValues, amount, finish, full_list, list, put_amount, set};
use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::{Context, Result, ensure};
use minter_protocol::Address;
use minter_state::{BlockStore, Mutation};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Symbol(pub [u8; 10]);

impl Symbol {
    pub fn bytes(&self) -> &[u8] {
        let first = self.0.iter().position(|&byte| byte != 0);
        let last = self.0.iter().rposition(|&byte| byte != 0);
        match (first, last) {
            (Some(first), Some(last)) => &self.0[first..=last],
            _ => &[],
        }
    }

    pub fn name(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.bytes())
    }

    pub fn base(network: u8) -> Self {
        Self(if network == 2 {
            *b"MNT\0\0\0\0\0\0\0"
        } else {
            *b"BIP\0\0\0\0\0\0\0"
        })
    }
}

#[derive(Clone)]
pub(crate) struct Coin {
    pub name: Vec<u8>,
    pub ratio: u32,
    pub max_supply: Amount,
    pub version: u16,
    pub symbol: Symbol,
    pub mintable: bool,
    pub burnable: bool,
}

impl Coin {
    fn load(store: &BlockStore, height: u64, id: u32) -> Result<Option<Self>> {
        Self::load_from(id, |key| store.value_at(height, key))
    }

    pub fn load_from(
        id: u32,
        read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
    ) -> Result<Option<Self>> {
        let Some(raw) = read(&model_key(id))? else {
            return Ok(None);
        };
        let mut fields = full_list(&raw)?;
        let coin = Self {
            name: Header::decode_bytes(&mut fields, false)?.to_vec(),
            ratio: u32::decode(&mut fields)?,
            max_supply: amount(&mut fields)?,
            version: u16::decode(&mut fields)?,
            symbol: Symbol(<[u8; 10]>::decode(&mut fields)?),
            mintable: bool::decode(&mut fields)?,
            burnable: bool::decode(&mut fields)?,
        };
        finish(fields)?;
        Ok(Some(coin))
    }

    fn encode(&self) -> Vec<u8> {
        list(|out| {
            self.name.as_slice().encode(out);
            self.ratio.encode(out);
            put_amount(&self.max_supply, out);
            self.version.encode(out);
            self.symbol.0.encode(out);
            self.mintable.encode(out);
            self.burnable.encode(out);
        })
    }

    pub fn full_symbol(&self) -> String {
        if self.version == 0 {
            self.symbol.name().into_owned()
        } else {
            format!("{}-{}", self.symbol.name(), self.version)
        }
    }
}

#[derive(Clone)]
pub(crate) struct Supply {
    pub volume: Amount,
    pub reserve: Amount,
}

#[derive(Default)]
pub(crate) struct Changes {
    created: BTreeMap<u32, (Coin, Option<Address>)>,
    updated: BTreeMap<u32, Coin>,
    symbols: BTreeMap<Symbol, Vec<u32>>,
    supplies: BTreeMap<u32, Supply>,
    owners: BTreeMap<u32, (Symbol, Address)>,
}

impl Changes {
    pub fn create(&mut self, id: u32, coin: Coin, supply: Supply, owner: Address) {
        self.register(id, coin, supply, Some(owner));
    }

    pub fn register(&mut self, id: u32, coin: Coin, supply: Supply, owner: Option<Address>) {
        self.symbols.entry(coin.symbol).or_default().push(id);
        self.created.insert(id, (coin, owner));
        self.supplies.insert(id, supply);
    }

    pub fn set_supply(&mut self, id: u32, supply: Supply) {
        self.supplies.insert(id, supply);
    }

    pub fn set_owner(&mut self, id: u32, symbol: Symbol, owner: Address) {
        self.owners.insert(id, (symbol, owner));
    }

    pub fn recreate(
        &mut self,
        previous_id: u32,
        previous: Coin,
        history: Vec<u32>,
        id: u32,
        coin: Coin,
        supply: Supply,
    ) {
        if let Some((model, _)) = self.created.get_mut(&previous_id) {
            *model = previous;
        } else {
            self.updated.insert(previous_id, previous);
        }
        self.symbols.entry(coin.symbol).or_insert(history);
        self.register(id, coin, supply, None);
    }

    pub fn write(self, ops: &mut Vec<Mutation>) {
        // Go commits IDs in descending order and writes the complete ticker
        // history in import order for each newly registered ID.
        let dirty: BTreeSet<_> = self
            .supplies
            .keys()
            .chain(self.owners.keys())
            .chain(self.updated.keys())
            .copied()
            .collect();
        let mut final_owners = BTreeMap::new();
        for &(symbol, owner) in self.owners.values() {
            final_owners.insert(symbol, owner);
        }
        let mut written_owners = BTreeSet::new();
        for id in dirty.into_iter().rev() {
            let model = self.created.get(&id);
            if let Some((coin, _)) = model {
                ops.push(set(
                    symbol_key(coin.symbol),
                    alloy_rlp::encode(&self.symbols[&coin.symbol]),
                ));
            }
            if let Some(coin) = self
                .updated
                .get(&id)
                .or_else(|| model.map(|(coin, _)| coin))
            {
                ops.push(set(model_key(id), coin.encode()));
            }
            if let Some(supply) = self.supplies.get(&id) {
                ops.push(set(
                    info_key(model_key(id)),
                    list(|out| {
                        put_amount(&supply.volume, out);
                        put_amount(&supply.reserve, out);
                    }),
                ));
            }
            let owner = self.owners.get(&id).copied().or_else(|| {
                model.and_then(|(coin, owner)| owner.map(|owner| (coin.symbol, owner)))
            });
            if let Some((symbol, mut owner)) = owner {
                if let Some(&latest) = final_owners.get(&symbol) {
                    if !written_owners.insert(symbol) {
                        continue;
                    }
                    owner = latest;
                }
                ops.push(set(
                    info_key(symbol_key(symbol)),
                    list(|out| owner.encode(out)),
                ));
            }
        }
    }
}

pub(crate) struct View<'a> {
    pub store: &'a BlockStore,
    pub height: u64,
    pub network: u8,
    pub changes: Option<&'a Changes>,
}

impl View<'_> {
    pub fn base_symbol(&self) -> &'static str {
        if self.network == 2 { "MNT" } else { "BIP" }
    }

    pub fn coin(&self, id: u32) -> Result<Option<Cow<'_, Coin>>> {
        if let Some(coin) = self.changes.and_then(|changes| changes.updated.get(&id)) {
            return Ok(Some(Cow::Borrowed(coin)));
        }
        if let Some((coin, _)) = self.changes.and_then(|changes| changes.created.get(&id)) {
            return Ok(Some(Cow::Borrowed(coin)));
        }
        Ok(Coin::load(self.store, self.height, id)?.map(Cow::Owned))
    }

    pub fn symbol(&self, id: u32) -> Result<Option<String>> {
        if id == 0 {
            return Ok(Some(self.base_symbol().into()));
        }
        Ok(self.coin(id)?.map(|coin| coin.full_symbol()))
    }

    pub fn current_id(&self, symbol: Symbol) -> Result<Option<u32>> {
        if symbol == Symbol::base(self.network) {
            return Ok(Some(0));
        }
        let ids = self.history(symbol)?;
        if ids.is_empty() {
            return Ok(None);
        }
        for id in ids {
            let coin = self.coin(id)?.context("ticker refers to a missing coin")?;
            ensure!(coin.symbol == symbol, "ticker refers to a different symbol");
            if coin.version == 0 {
                return Ok(Some(id));
            }
        }
        anyhow::bail!("ticker has no current coin")
    }

    pub fn history(&self, symbol: Symbol) -> Result<Vec<u32>> {
        if let Some(ids) = self
            .changes
            .and_then(|changes| changes.symbols.get(&symbol))
        {
            return Ok(ids.clone());
        }
        Ok(ticker_ids(symbol, |key| self.store.value_at(self.height, key))?.unwrap_or_default())
    }

    pub fn supply(&self, id: u32) -> Result<Cow<'_, Supply>> {
        if let Some(supply) = self.changes.and_then(|changes| changes.supplies.get(&id)) {
            return Ok(Cow::Borrowed(supply));
        }
        Ok(Cow::Owned(load_supply(id, |key| {
            self.store.value_at(self.height, key)
        })?))
    }

    pub fn next_id(&self) -> Result<u32> {
        let last = match self
            .changes
            .and_then(|changes| changes.created.last_key_value())
        {
            Some((&id, _)) => id,
            None => AppValues::load(self.store, self.height)?.coins,
        };
        last.checked_add(1).context("coin ID overflow")
    }

    pub fn owner(&self, symbol: Symbol) -> Result<Option<Address>> {
        if let Some(owner) = self.changes.and_then(|changes| {
            changes
                .owners
                .values()
                .rev()
                .find_map(|&(ticker, owner)| (ticker == symbol).then_some(owner))
        }) {
            return Ok(Some(owner));
        }
        if let Some(changes) = self.changes
            && let Some(ids) = changes.symbols.get(&symbol)
            && let Some(owner) = ids
                .iter()
                .rev()
                .find_map(|id| changes.created.get(id).and_then(|(_, owner)| *owner))
        {
            return Ok(Some(owner));
        }
        load_owner(symbol, |key| self.store.value_at(self.height, key))
    }
}

pub(crate) fn ticker_ids(
    symbol: Symbol,
    read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<Option<Vec<u32>>> {
    let Some(raw) = read(&symbol_key(symbol))? else {
        return Ok(None);
    };
    let mut bytes = raw.as_slice();
    let ids = Vec::<u32>::decode(&mut bytes)?;
    finish(bytes)?;
    Ok(Some(ids))
}

pub(crate) fn load_supply(
    id: u32,
    read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<Supply> {
    let raw = read(&info_key(model_key(id)))?.context("missing coin supply")?;
    let mut fields = full_list(&raw)?;
    let supply = Supply {
        volume: amount(&mut fields)?,
        reserve: amount(&mut fields)?,
    };
    finish(fields)?;
    Ok(supply)
}

pub(crate) fn load_owner(
    symbol: Symbol,
    read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<Option<Address>> {
    let Some(raw) = read(&info_key(symbol_key(symbol)))? else {
        return Ok(None);
    };
    let mut fields = full_list(&raw)?;
    let owner = Header::decode_bytes(&mut fields, false)?;
    finish(fields)?;
    if owner.is_empty() {
        return Ok(None);
    }
    Ok(Some(owner.try_into()?))
}

fn model_key(id: u32) -> Vec<u8> {
    [b"q".as_slice(), &id.to_le_bytes()].concat()
}

fn symbol_key(symbol: Symbol) -> Vec<u8> {
    [b"qs".as_slice(), &symbol.0].concat()
}

fn info_key(mut key: Vec<u8>) -> Vec<u8> {
    key.push(b'i');
    key
}
