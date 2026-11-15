use super::{Candidate, Candidates, MAX_CANDIDATES, MAX_STAKES, Stake};
use crate::{
    genesis::{Integer, Volumes, address, decimal, public_key},
    state::{Amount, PublicKey},
};
use anyhow::{Result, ensure};
use minter_protocol::Address;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Deserialize)]
pub(crate) struct GenesisCandidate {
    #[serde(deserialize_with = "public_key")]
    public_key: PublicKey,
    #[serde(deserialize_with = "address")]
    owner_address: Address,
    #[serde(deserialize_with = "address")]
    reward_address: Address,
    #[serde(deserialize_with = "address")]
    control_address: Address,
    id: Integer,
    status: Integer,
    commission: Integer,
    #[serde(default)]
    last_edit_commission_height: Integer,
    #[serde(default)]
    jailed_until: Integer,
    #[serde(deserialize_with = "decimal")]
    total_bip_stake: Amount,
    #[serde(default)]
    stakes: Vec<GenesisStake>,
    #[serde(default)]
    updates: Vec<GenesisStake>,
}

#[derive(Deserialize)]
struct GenesisStake {
    #[serde(deserialize_with = "address")]
    owner: Address,
    #[serde(default)]
    coin: Integer,
    #[serde(deserialize_with = "decimal")]
    value: Amount,
    #[serde(default, deserialize_with = "decimal")]
    bip_value: Amount,
}

impl GenesisStake {
    fn into_stake(self, volumes: &mut Volumes) -> Result<Stake> {
        let coin = self.coin.u32()?;
        volumes.take(coin, &self.value)?;
        Ok(Stake {
            owner: self.owner,
            coin,
            value: self.value,
            bip: self.bip_value,
            dirty: true,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct GenesisDeletedCandidate {
    id: Integer,
    #[serde(deserialize_with = "public_key")]
    public_key: PublicKey,
}

impl Candidates {
    pub fn from_genesis(
        rows: Vec<GenesisCandidate>,
        blocked: BTreeSet<PublicKey>,
        deleted: Vec<GenesisDeletedCandidate>,
        volumes: &mut Volumes,
    ) -> Result<Self> {
        ensure!(
            rows.len() <= MAX_CANDIDATES,
            "genesis requires candidate pruning"
        );
        let mut candidates = Self {
            blocked,
            catalog_dirty: true,
            ..Self::default()
        };
        for row in rows {
            let id = row.id.u32()?;
            ensure!(
                id != 0
                    && !candidates.candidates.contains_key(&id)
                    && candidates.active.insert(row.public_key, id).is_none(),
                "invalid or duplicate genesis candidate"
            );
            let commission = row.commission.u32()?;
            ensure!(commission <= 100, "invalid genesis candidate commission");
            let mut stakes = row
                .stakes
                .into_iter()
                .map(|stake| stake.into_stake(volumes))
                .collect::<Result<Vec<_>>>()?;
            let mut updates = row
                .updates
                .into_iter()
                .map(|stake| stake.into_stake(volumes))
                .collect::<Result<Vec<_>>>()?;
            if stakes.len() > MAX_STAKES {
                updates.extend(stakes.split_off(MAX_STAKES));
            }
            let mut stakes: Vec<_> = stakes.into_iter().map(Some).collect();
            stakes.resize_with(MAX_STAKES, || None);
            candidates.max_id = candidates.max_id.max(id);
            candidates.candidates.insert(
                id,
                Candidate {
                    key: row.public_key,
                    reward_address: row.reward_address,
                    owner: row.owner_address,
                    control: row.control_address,
                    commission,
                    jailed_until: row.jailed_until.0,
                    status: if row.status.0 == 2 { 2 } else { 1 },
                    id,
                    last_edit_commission: row.last_edit_commission_height.0,
                    model_dirty: true,
                    // Go creates the total-stake row even when it encodes zero.
                    total_dirty: true,
                    total: row.total_bip_stake,
                    updates_dirty: false,
                    stakes,
                    updates,
                },
            );
        }
        let mut deleted_ids = BTreeSet::new();
        for row in deleted {
            let id = row.id.u32()?;
            ensure!(
                id != 0
                    && !candidates.candidates.contains_key(&id)
                    && deleted_ids.insert(id)
                    && candidates.deleted.insert(row.public_key, id).is_none(),
                "invalid or duplicate deleted genesis candidate"
            );
        }
        Ok(candidates)
    }

    pub fn contains_genesis_id(&self, id: u32) -> bool {
        self.candidates.contains_key(&id) || self.deleted.values().any(|&deleted| deleted == id)
    }
}
