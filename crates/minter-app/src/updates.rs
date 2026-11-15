use crate::{
    binary,
    state::{Amount, PublicKey, finish, full_list, list, set},
};
use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::Result;
use minter_state::{BlockStore, Mutation};
use std::{
    borrow::Cow,
    collections::{BTreeMap, btree_map::Entry},
};

#[derive(Default)]
pub(crate) struct Votes<const PREFIX: u8> {
    dirty: BTreeMap<u64, Vec<Proposal>>,
    expired: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Proposal {
    pub payload: Vec<u8>,
    pub voters: Vec<PublicKey>,
}

pub(crate) struct Vote {
    pub height: u64,
    pub key: PublicKey,
    pub payload: Vec<u8>,
}

pub(crate) type NetworkVotes = Votes<b'u'>;
pub(crate) type CommissionVotes = Votes<b'p'>;

pub(crate) fn valid_name(name: &[u8]) -> bool {
    (1..=20).contains(&name.len())
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

impl<const PREFIX: u8> Votes<PREFIX> {
    pub fn key(height: u64) -> Vec<u8> {
        [&[PREFIX], height.to_le_bytes().as_slice()].concat()
    }

    pub fn contains(
        changes: Option<&Self>,
        store: &BlockStore,
        prior: u64,
        height: u64,
        voter: PublicKey,
    ) -> Result<bool> {
        Ok(Self::at(changes, store, prior, height)?
            .iter()
            .any(|proposal| proposal.voters.contains(&voter)))
    }

    pub fn record(&mut self, store: &BlockStore, prior: u64, vote: &Vote) -> Result<()> {
        let proposals = match self.dirty.entry(vote.height) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Self::load(store, prior, vote.height)?),
        };
        add(proposals, vote.key, &vote.payload);
        Ok(())
    }

    pub fn import(
        &mut self,
        height: u64,
        payload: &[u8],
        voters: impl IntoIterator<Item = PublicKey>,
    ) {
        for voter in voters {
            add(self.dirty.entry(height).or_default(), voter, payload);
        }
    }

    pub fn resolve<'a>(
        &mut self,
        store: &BlockStore,
        prior: u64,
        height: u64,
        mut power: impl FnMut(&PublicKey) -> Option<&'a Amount>,
        total: &Amount,
    ) -> Result<Option<Vec<u8>>> {
        let proposals = Self::at(Some(self), store, prior, height)?;
        let mut accepted = None;
        let mut largest = Amount::ZERO;
        for proposal in proposals.iter() {
            let weight: Amount = proposal.voters.iter().filter_map(&mut power).sum();
            if weight > largest && has_quorum(&weight, total) {
                accepted = Some(proposal.payload.clone());
                largest = weight;
            }
        }
        if !proposals.is_empty() {
            self.expired = Some(height);
        }
        Ok(accepted)
    }

    fn at<'a>(
        changes: Option<&'a Self>,
        store: &BlockStore,
        prior: u64,
        height: u64,
    ) -> Result<Cow<'a, [Proposal]>> {
        if let Some(proposals) = changes.and_then(|changes| changes.dirty.get(&height)) {
            return Ok(Cow::Borrowed(proposals));
        }
        Ok(Cow::Owned(Self::load(store, prior, height)?))
    }

    fn load(store: &BlockStore, prior: u64, height: u64) -> Result<Vec<Proposal>> {
        let Some(raw) = store.value_at(prior, &Self::key(height))? else {
            return Ok(Vec::new());
        };
        decode_proposals(&raw)
    }

    pub fn write(self, operations: &mut Vec<Mutation>) {
        // Go writes dirty heights in ascending order, then removes the
        // expired record. A vote may create and remove a key in one block.
        for (height, proposals) in self.dirty {
            operations.push(set(
                Self::key(height),
                list(|out| {
                    for proposal in proposals {
                        out.extend(list(|out| {
                            proposal.voters.encode(out);
                            proposal.payload.as_slice().encode(out);
                        }));
                    }
                }),
            ));
        }
        if let Some(height) = self.expired {
            operations.push(Mutation::Remove(Self::key(height)));
        }
    }
}

fn add(proposals: &mut Vec<Proposal>, voter: PublicKey, payload: &[u8]) {
    if let Some(proposal) = proposals
        .iter_mut()
        .find(|proposal| proposal.payload == payload)
    {
        proposal.voters.push(voter);
    } else {
        proposals.push(Proposal {
            payload: payload.into(),
            voters: vec![voter],
        });
    }
}

pub(crate) fn decode_proposals(raw: &[u8]) -> Result<Vec<Proposal>> {
    let mut rows = full_list(raw)?;
    let mut proposals = Vec::new();
    while !rows.is_empty() {
        let mut fields = Header::decode_bytes(&mut rows, true)?;
        let voters = Vec::<PublicKey>::decode(&mut fields)?;
        let payload = Header::decode_bytes(&mut fields, false)?.to_vec();
        finish(fields)?;
        proposals.push(Proposal { payload, voters });
    }
    Ok(proposals)
}

pub(crate) fn has_quorum(weight: &Amount, total: &Amount) -> bool {
    if total.bits() == 0 {
        return false;
    }
    // Go compares a nearest-even big.Float quotient to the float64 value
    // of two thirds. At the threshold the quotient's exponent is zero.
    let precision = weight.bits().max(total.bits()).max(64) as usize;
    let threshold = Amount::from(6_004_799_503_160_661u64) << (precision - 53);
    binary::divide(weight, total, precision, precision) > threshold
}
