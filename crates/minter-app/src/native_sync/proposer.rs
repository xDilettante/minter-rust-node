use super::validator_set;
use anyhow::{Context, Result, ensure};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};
use tendermint::{
    PublicKey,
    validator::{Info, Set},
};
use tendermint_proto::v0_34::{abci, types};

pub(super) fn update(prior: Option<&Set>, changes: &[abci::ValidatorUpdate]) -> Result<Set> {
    if let Some(prior) = prior.filter(|_| changes.is_empty()) {
        return Ok(prior.clone());
    }
    let mut validators: BTreeMap<_, _> = prior
        .into_iter()
        .flat_map(Set::validators)
        .map(|validator| (validator.address, validator.clone()))
        .collect();
    let mut seen = BTreeSet::new();
    let mut additions = BTreeSet::new();
    let mut removed_power = 0;
    for change in changes {
        let key: PublicKey = change
            .pub_key
            .clone()
            .context("validator update lacks a key")?
            .try_into()?;
        let mut info = Info::new(key, change.power.try_into()?);
        ensure!(
            seen.insert(info.address),
            "duplicate validator update {}",
            info.address
        );
        if change.power == 0 {
            let removed = validators
                .remove(&info.address)
                .with_context(|| format!("remove unknown validator {}", info.address))?;
            removed_power += removed.power();
        } else {
            if let Some(existing) = validators.get(&info.address) {
                info.proposer_priority = existing.proposer_priority;
            } else {
                additions.insert(info.address);
            }
            validators.insert(info.address, info);
        }
    }
    let mut set = validator_set(validators.into_values().collect())?;
    if prior.is_none_or(|set| set.proposer.is_some()) {
        // Go computes the new validator's priority before removals.
        let power = (set.total_voting_power.value() + removed_power) as i64;
        let new_priority = (-(power + (power >> 3))).into();
        for validator in &mut set.validators {
            if additions.contains(&validator.address) {
                validator.proposer_priority = new_priority;
            }
        }
        normalize(&mut set);
        if prior.is_none() {
            increment(&mut set, 1)?;
        } else {
            set.proposer = prior.and_then(|set| set.proposer.clone());
        }
    }
    Ok(set)
}

/// Advance the same number of rounds as one Go IncrementProposerPriority call.
pub(super) fn increment(set: &mut Set, rounds: u32) -> Result<()> {
    ensure!(
        rounds > 0 && !set.validators.is_empty(),
        "invalid proposer priority increment"
    );
    normalize(set);
    let total: i64 = set.total_voting_power.into();
    for _ in 0..rounds {
        for validator in &mut set.validators {
            let power: i64 = validator.power.into();
            validator.proposer_priority = validator
                .proposer_priority
                .value()
                .saturating_add(power)
                .into();
        }
        let proposer = set
            .validators
            .iter_mut()
            .max_by_key(|validator| (validator.proposer_priority, Reverse(validator.address)))
            .context("missing proposer")?;
        proposer.proposer_priority = proposer
            .proposer_priority
            .value()
            .saturating_sub(total)
            .into();
        set.proposer = Some(proposer.clone());
    }
    Ok(())
}

fn normalize(set: &mut Set) {
    let limit = 2 * i64::from(set.total_voting_power);
    let min = set
        .validators
        .iter()
        .map(|validator| validator.proposer_priority.value())
        .min()
        .unwrap_or(0);
    let max = set
        .validators
        .iter()
        .map(|validator| validator.proposer_priority.value())
        .max()
        .unwrap_or(0);
    // Match Go's signed int64 arithmetic for priorities supplied by a checkpoint.
    let difference = max.wrapping_sub(min).wrapping_abs();
    if difference > limit {
        let divisor = difference.wrapping_add(limit).wrapping_sub(1) / limit;
        for validator in &mut set.validators {
            validator.proposer_priority = (validator.proposer_priority.value() / divisor).into();
        }
    }
    // Go big.Int.Div rounds down when the sum is negative.
    let sum = set
        .validators
        .iter()
        .map(|validator| i128::from(validator.proposer_priority.value()))
        .sum::<i128>();
    let average = sum.div_euclid(set.validators.len() as i128) as i64;
    for validator in &mut set.validators {
        validator.proposer_priority = validator
            .proposer_priority
            .value()
            .saturating_sub(average)
            .into();
    }
}

pub(super) fn to_wire(set: &Set) -> types::ValidatorSet {
    let mut wire: types::ValidatorSet = set.clone().into();
    // Go's ValidatorSet.ToProto omits its cached total from the wire.
    wire.total_voting_power = 0;
    wire
}

pub(super) fn validate_wire(raw: types::ValidatorSet) -> Result<Set> {
    let set: Set = raw.try_into()?;
    let mut validated = validator_set(set.validators)?;
    if let Some(proposer) = set.proposer {
        let member = validated
            .validator(proposer.address)
            .context("selected proposer is not a validator")?;
        ensure!(
            member.pub_key == proposer.pub_key
                && member.power == proposer.power
                && member.proposer_priority == proposer.proposer_priority,
            "selected proposer differs from its validator entry"
        );
        validated.proposer = Some(proposer);
    }
    Ok(validated)
}

pub(super) mod stored {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    pub fn serialize<S: Serializer>(set: &Set, serializer: S) -> Result<S::Ok, S::Error> {
        to_wire(set).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Set, D::Error> {
        // Old cursors used tendermint::validator::Set JSON. Its common field
        // names are identical and missing priorities default to zero.
        validate_wire(types::ValidatorSet::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_round_advancement() {
        for (initial, once, seven) in [
            ([33, -27, 8], [2, -4, 3], [-1, 5, -3]),
            ([-12, 15, 2], [-2, 1, 3], [4, 1, -3]),
        ] {
            let validators = [4u32, 3, 2].into_iter().enumerate().map(|(index, power)| {
                let mut key = [0; 32];
                key[0] = index as u8 + 1;
                let mut info = Info::new(PublicKey::from_raw_ed25519(&key).unwrap(), power.into());
                info.proposer_priority = initial[index].into();
                info
            });
            let set = validator_set(validators.collect()).unwrap();
            for (steps, expected, proposer) in [
                (vec![1], once, if initial[0] > 0 { 4 } else { 3 }),
                (vec![7], seven, 2),
                (vec![1, 6], seven, 2),
                (vec![1; 7], seven, 2),
            ] {
                let mut candidate = set.clone();
                for rounds in steps {
                    increment(&mut candidate, rounds).unwrap();
                }
                assert_eq!(
                    candidate
                        .validators
                        .iter()
                        .map(|v| v.proposer_priority.value())
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(candidate.proposer.unwrap().power(), proposer);
            }
        }
    }
}
