use super::{Outcome, display_address, tag};
use crate::state::{Account, Multisig, finish};
use alloy_rlp::Decodable;
use anyhow::Result;
use minter_protocol::{Address, multisig_address};
use std::collections::BTreeSet;
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) struct MultisigTransaction {
    pub create: bool,
    model: Multisig,
}

pub(crate) struct Change {
    pub address: Address,
    pub model: Multisig,
    create: bool,
}

impl MultisigTransaction {
    pub fn decode(create: bool, mut fields: &[u8]) -> Result<Self> {
        let model = Multisig {
            threshold: u32::decode(&mut fields)?,
            weights: Vec::decode(&mut fields)?,
            addresses: Vec::decode(&mut fields)?,
        };
        finish(fields)?;
        Ok(Self { create, model })
    }

    pub fn plan(&self, account: &Account, sender: Address, nonce: u64) -> Result<Change, Outcome> {
        if !self.create && !account.is_multisig() {
            return Err(Outcome::reject(
                603,
                "Multisig does not exists".into(),
                [("address", display_address(sender))],
            ));
        }
        let Multisig {
            threshold,
            weights,
            addresses,
        } = &self.model;
        if weights.len() > 32 {
            return Err(Outcome::reject(
                605,
                "Owners list is limited to 32 items".into(),
                [
                    ("count_owners", weights.len().to_string()),
                    ("max_count_owners", "32".into()),
                ],
            ));
        }
        if addresses.len() != weights.len() {
            return Err(Outcome::reject(
                607,
                "Different count addresses and weights".into(),
                [
                    ("count_addresses", addresses.len().to_string()),
                    ("count_weights", weights.len().to_string()),
                ],
            ));
        }
        for (&address, &weight) in addresses.iter().zip(weights) {
            if weight > 1023 {
                return Err(Outcome::reject(
                    601,
                    "Incorrect multisig weights".into(),
                    [
                        ("address", display_address(address)),
                        ("weight", weight.to_string()),
                        ("max_weight", "1024".into()),
                    ],
                ));
            }
        }
        let mut unique = BTreeSet::new();
        for &address in addresses {
            if !unique.insert(address) {
                return Err(Outcome::reject(
                    606,
                    "Duplicated multisig addresses".into(),
                    [("address", display_address(address))],
                ));
            }
        }
        let total: u32 = weights.iter().sum();
        if !self.create && *threshold > total {
            return Err(Outcome::reject(
                608,
                "Incorrect multisig weights".into(),
                [
                    ("total_weights", total.to_string()),
                    ("threshold", threshold.to_string()),
                ],
            ));
        }
        Ok(Change {
            address: if self.create {
                multisig_address(sender, nonce)
            } else {
                sender
            },
            model: self.model.clone(),
            create: self.create,
        })
    }
}

impl Change {
    pub fn account_to_check(&self) -> Option<Address> {
        self.create.then_some(self.address)
    }

    pub fn rejection(&self, target: &Account) -> Option<Outcome> {
        (target.is_multisig() || target.nonce > 0).then(|| {
            let address = display_address(self.address);
            Outcome::reject(
                602,
                format!("Multisig {address} already exists"),
                [("address", address)],
            )
        })
    }

    pub fn tag_success(&self, tags: &mut Vec<EventAttribute>) {
        if self.create {
            tags.push(tag("tx.created_multisig", &hex::encode(self.address), true));
        }
    }
}
