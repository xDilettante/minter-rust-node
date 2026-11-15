use crate::state::{Amount, PublicKey, amount, finish, full_list, list, put_amount, set};
use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::{Context, Result, ensure};
use minter_protocol::Address;
use minter_state::{BlockStore, Mutation};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    ops::RangeInclusive,
};

pub(crate) const TESTNET_QUARTER_HOUR: u64 = 518_400 / 2_920;

pub(crate) fn unbond_period(network: u8) -> u64 {
    if network == 2 {
        3 * TESTNET_QUARTER_HOUR
    } else {
        518_400
    }
}

pub(crate) struct Fund {
    pub owner: Address,
    pub coin: u32,
    pub value: Amount,
    pub source: Option<PublicKey>,
    source_id: u32,
    destinations: Vec<u32>,
    linked_source: bool,
}

impl Fund {
    pub fn new(
        owner: Address,
        coin: u32,
        value: Amount,
        candidate: Option<(PublicKey, u32)>,
        destination: u32,
    ) -> Self {
        let (source, source_id) = candidate.map_or((None, 0), |(key, id)| (Some(key), id));
        Self {
            owner,
            coin,
            value,
            source,
            source_id,
            destinations: if destination == 0 {
                Vec::new()
            } else {
                vec![destination]
            },
            linked_source: false,
        }
    }

    pub fn destination(&self) -> u32 {
        self.destinations.first().copied().unwrap_or_default()
    }

    pub fn has_linked_source(&self) -> bool {
        self.linked_source
    }

    fn decode(mut fields: &[u8]) -> Result<Self> {
        let owner = Address::decode(&mut fields)?;
        let source = Header::decode_bytes(&mut fields, false)?;
        let source = if source.is_empty() {
            None
        } else {
            Some(
                source
                    .try_into()
                    .context("invalid frozen fund candidate key")?,
            )
        };
        let source_id = u32::decode(&mut fields)?;
        let coin = u32::decode(&mut fields)?;
        let value = amount(&mut fields)?;
        let mut destinations = Vec::new();
        while !fields.is_empty() {
            destinations.push(u32::decode(&mut fields)?);
        }
        let fund = Self {
            owner,
            coin,
            value,
            source,
            source_id,
            destinations,
            linked_source: false,
        };
        ensure!(
            fund.destination() == 0 || fund.source.is_some(),
            "frozen stake move lacks a source candidate"
        );
        Ok(fund)
    }

    fn encode(&self) -> Vec<u8> {
        list(|out| {
            self.owner.encode(out);
            self.source
                .as_ref()
                .map_or(&[][..], |key| key.as_slice())
                .encode(out);
            self.source_id.encode(out);
            self.coin.encode(out);
            put_amount(&self.value, out);
            for id in &self.destinations {
                id.encode(out);
            }
        })
    }
}

// Go keeps pointers from these newly frozen funds to candidate public keys.
// Keep their logical keys separately: a rotation alone does not rewrite IAVL.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct References(Vec<Reference>);

#[derive(Clone, Serialize, Deserialize)]
struct Reference {
    height: u64,
    first: usize,
    end: usize,
    key: PublicKey,
}

impl References {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn validate(
        &self,
        head: u64,
        mut read: impl FnMut(u64) -> Result<Option<Vec<u8>>>,
    ) -> Result<()> {
        let mut ordered: Vec<_> = self.0.iter().collect();
        ordered.sort_unstable_by_key(|reference| (reference.height, reference.first));
        let mut prior = None;
        let mut funds = Vec::new();
        for reference in ordered {
            ensure!(reference.height > head, "expired frozen key reference");
            if let Some((height, end)) = prior
                && height == reference.height
            {
                ensure!(reference.first >= end, "overlapping frozen key references");
            } else {
                funds = decode_funds(
                    &read(reference.height)?.context("missing linked frozen record")?,
                )?;
            }
            ensure!(
                reference.first < reference.end,
                "empty frozen reference range"
            );
            let selected = funds
                .get(reference.first..reference.end)
                .context("frozen reference exceeds fund record")?;
            let id = selected[0].source_id;
            ensure!(
                id != 0
                    && selected
                        .iter()
                        .all(|fund| fund.source_id == id && fund.source.is_some()),
                "frozen key reference has inconsistent candidate funds"
            );
            prior = Some((reference.height, reference.end));
        }
        Ok(())
    }

    fn apply(&self, height: u64, funds: &mut [Fund]) -> Result<()> {
        for reference in self.0.iter().filter(|reference| reference.height == height) {
            ensure!(
                reference.first < reference.end,
                "empty frozen reference range"
            );
            for fund in funds
                .get_mut(reference.first..reference.end)
                .context("frozen reference exceeds fund record")?
            {
                fund.source = Some(reference.key);
                fund.linked_source = true;
            }
        }
        Ok(())
    }

    fn track(&mut self, height: u64, index: usize, key: PublicKey) {
        if let Some(last) = self.0.last_mut()
            && last.height == height
            && last.end == index
            && last.key == key
        {
            last.end += 1;
        } else {
            self.0.push(Reference {
                height,
                first: index,
                end: index + 1,
                key,
            });
        }
    }
}

#[derive(Default)]
pub(crate) struct Changes {
    rows: BTreeMap<u64, Option<Vec<Fund>>>,
    references: References,
}

impl Changes {
    pub fn with_references(references: References) -> Self {
        Self {
            references,
            ..Default::default()
        }
    }

    pub fn rotate_key(&mut self, old: PublicKey, new: PublicKey) {
        for reference in &mut self.references.0 {
            if reference.key == old {
                reference.key = new;
            }
        }
    }

    pub fn release(
        &mut self,
        store: &BlockStore,
        prior_height: u64,
        height: u64,
    ) -> Result<Option<Vec<Fund>>> {
        let mut funds = if let Some(funds) = self.rows.get_mut(&height) {
            funds.take()
        } else {
            load(store, prior_height, height)?
        };
        if let Some(funds) = &mut funds {
            self.references.apply(height, funds)?;
        }
        self.references
            .0
            .retain(|reference| reference.height != height);
        if funds.is_some() {
            self.rows.insert(height, None);
        }
        Ok(funds)
    }

    /// Go rewrites every existing frozen-funds record in this interval after
    /// evidence, including records without a stake from the punished candidate.
    pub fn stage_range(
        &mut self,
        store: &BlockStore,
        prior_height: u64,
        heights: RangeInclusive<u64>,
    ) -> Result<()> {
        let first = key(*heights.start());
        let end = key(heights
            .end()
            .checked_add(1)
            .context("frozen height overflow")?);
        store.visit_range_at(
            prior_height,
            first.as_slice()..end.as_slice(),
            |key, value| {
                let height = u64::from_be_bytes(key[1..].try_into()?);
                if let Entry::Vacant(entry) = self.rows.entry(height) {
                    entry.insert(Some(decode_funds(value)?));
                }
                Ok(())
            },
        )?;
        for (&height, funds) in self.rows.range_mut(heights) {
            if let Some(funds) = funds {
                self.references.apply(height, funds)?;
            }
        }
        Ok(())
    }

    pub fn candidate_funds(
        &mut self,
        heights: RangeInclusive<u64>,
        candidate: u32,
    ) -> impl Iterator<Item = &mut Fund> {
        self.rows
            .range_mut(heights)
            .filter_map(|(_, funds)| funds.as_deref_mut())
            .flatten()
            .filter(move |fund| fund.source_id == candidate)
    }

    pub fn push(
        &mut self,
        store: &BlockStore,
        prior_height: u64,
        height: u64,
        fund: Fund,
    ) -> Result<()> {
        let funds = match self.rows.entry(height) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(if store.head().is_some() {
                load(store, prior_height, height)?
            } else {
                None
            }),
        };
        let funds = funds.get_or_insert_with(Vec::new);
        self.references.apply(height, funds)?;
        funds.push(fund);
        Ok(())
    }

    pub fn push_reference(
        &mut self,
        store: &BlockStore,
        prior_height: u64,
        height: u64,
        fund: Fund,
    ) -> Result<()> {
        let key = fund.source.context("linked frozen fund lacks a key")?;
        self.push(store, prior_height, height, fund)?;
        let index = self.rows[&height]
            .as_ref()
            .context("missing linked frozen row")?
            .len()
            - 1;
        self.references.track(height, index, key);
        Ok(())
    }

    pub fn write(self, ops: &mut Vec<Mutation>) -> Result<References> {
        for (height, funds) in self.rows {
            let key = key(height);
            if let Some(mut funds) = funds {
                self.references.apply(height, &mut funds)?;
                ops.push(set(
                    key,
                    list(|out| {
                        out.extend(list(|out| {
                            for fund in funds {
                                out.extend(fund.encode());
                            }
                        }));
                    }),
                ));
            } else {
                ops.push(Mutation::Remove(key));
            }
        }
        Ok(self.references)
    }
}

pub(crate) fn key(height: u64) -> Vec<u8> {
    [b"f".as_slice(), &height.to_be_bytes()].concat()
}

fn load(store: &BlockStore, prior_height: u64, height: u64) -> Result<Option<Vec<Fund>>> {
    let Some(raw) = store.value_at(prior_height, &key(height))? else {
        return Ok(None);
    };
    Ok(Some(decode_funds(&raw)?))
}

fn decode_funds(raw: &[u8]) -> Result<Vec<Fund>> {
    let mut model = full_list(raw)?;
    let mut items = Header::decode_bytes(&mut model, true)?;
    finish(model)?;
    let mut funds = Vec::new();
    while !items.is_empty() {
        funds.push(Fund::decode(Header::decode_bytes(&mut items, true)?)?);
    }
    Ok(funds)
}
