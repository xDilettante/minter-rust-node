use crate::state::{PublicKey, finish, full_list, list, set};
use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::Result;
use minter_state::{BlockStore, Mutation};
use std::collections::{BTreeMap, btree_map::Entry};

#[derive(Default)]
pub(crate) struct Votes {
    dirty: BTreeMap<u64, Vec<PublicKey>>,
    expired: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) struct Vote {
    pub key: PublicKey,
    pub height: u64,
}

impl Votes {
    pub fn begin(store: &BlockStore, prior: u64, height: u64) -> Result<Self> {
        let Some(voters) = load(store, prior, height)? else {
            return Ok(Self::default());
        };
        Ok(Self {
            dirty: BTreeMap::from([(height, voters)]),
            expired: Some(height),
        })
    }

    pub fn scheduled(&self) -> &[PublicKey] {
        self.expired
            .and_then(|height| self.dirty.get(&height))
            .map_or(&[], Vec::as_slice)
    }

    pub fn contains(
        changes: Option<&Self>,
        store: &BlockStore,
        prior: u64,
        vote: Vote,
    ) -> Result<bool> {
        if let Some(voters) = changes.and_then(|changes| changes.dirty.get(&vote.height)) {
            return Ok(voters.contains(&vote.key));
        }
        Ok(load(store, prior, vote.height)?.is_some_and(|voters| voters.contains(&vote.key)))
    }

    pub fn record(&mut self, store: &BlockStore, prior: u64, vote: Vote) -> Result<()> {
        let voters = match self.dirty.entry(vote.height) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                entry.insert(load(store, prior, vote.height)?.unwrap_or_default())
            }
        };
        voters.push(vote.key);
        Ok(())
    }

    pub fn write(self, operations: &mut Vec<Mutation>) {
        // Go keeps a deleted model visible until Commit, then removes it even
        // when a transaction appended another vote to it in the same block.
        for (height, voters) in self.dirty {
            if self.expired == Some(height) {
                operations.push(Mutation::Remove(key(height)));
            } else {
                let items = list(|out| {
                    for voter in voters {
                        out.extend(list(|out| voter.encode(out)));
                    }
                });
                operations.push(set(key(height), list(|out| out.extend(items))));
            }
        }
    }
}

pub(crate) fn key(height: u64) -> Vec<u8> {
    [b"h".as_slice(), &height.to_le_bytes()].concat()
}

fn load(store: &BlockStore, prior: u64, height: u64) -> Result<Option<Vec<PublicKey>>> {
    let Some(raw) = store.value_at(prior, &key(height))? else {
        return Ok(None);
    };
    Ok(Some(decode_voters(&raw)?))
}

pub(crate) fn decode_voters(raw: &[u8]) -> Result<Vec<PublicKey>> {
    let mut model = full_list(raw)?;
    let mut items = Header::decode_bytes(&mut model, true)?;
    finish(model)?;
    let mut voters = Vec::new();
    while !items.is_empty() {
        let mut item = Header::decode_bytes(&mut items, true)?;
        voters.push(PublicKey::decode(&mut item)?);
        finish(item)?;
    }
    Ok(voters)
}
