use crate::{
    REWARD_PERIOD, Upgrade,
    stakes::{Candidate, Candidates, Stake},
    state::{Amount, AppValues, Validator, display_key},
    transaction::display_address,
};
use anyhow::{Context, Result, ensure};
use minter_api::{
    Any, pack,
    pb::{RewardEvent, reward_event::Role},
};
use minter_protocol::Address;
use num_bigint::{BigInt, Sign};
use num_traits::Zero;
use std::collections::BTreeSet;

pub(crate) const DAO: Address = [
    0x7f, 0x0f, 0xc2, 0x1d, 0x93, 0x2f, 0x38, 0xca, 0x94, 0x44, 0xf6, 0x17, 0x03, 0x17, 0x45, 0x69,
    0x06, 0x6c, 0xfa, 0x50,
];
pub(crate) const DEVELOPERS: Address = [
    0x68, 0x85, 0x68, 0xd9, 0xd7, 0x0c, 0x57, 0xe7, 0x1d, 0x0b, 0x9d, 0xe6, 0x48, 0x0a, 0xfb, 0x0d,
    0x31, 0x7f, 0x88, 0x5c,
];
const TREASURY_COMMISSION: u32 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
    V3,
    V4,
    V5Bug,
    V5Fix,
}

impl Version {
    pub fn at_height(height: u64, upgrades: &[Upgrade]) -> (Self, bool) {
        for (name, version) in [
            ("v330", Self::V5Fix),
            ("v320", Self::V5Bug),
            ("v310", Self::V4),
        ] {
            if let Some(upgrade) = upgrades.iter().find(|upgrade| upgrade.name == name) {
                // Go's GetVersionHeight adds one, and the comparison is strict.
                let since_recorded = height.saturating_sub(upgrade.height);
                if since_recorded > 1 {
                    return (
                        version,
                        version == Self::V5Fix && since_recorded - 1 < REWARD_PERIOD,
                    );
                }
            }
        }
        (Self::V3, false)
    }
}

fn commission(amount: &Amount, rate: u32) -> Amount {
    amount * rate / 100u32
}

fn add_reward(
    candidate: &mut Candidate,
    events: &mut Vec<Any>,
    role: Role,
    owner: Address,
    amount: Amount,
    coin: u32,
) {
    events.push(pack(RewardEvent {
        role: role as i32,
        address: display_address(owner),
        amount: amount.to_string(),
        validator_pub_key: display_key(candidate.key),
        for_coin: u64::from(coin),
    }));
    candidate.add_reward(owner, amount);
}

fn pay_treasuries(candidate: &mut Candidate, events: &mut Vec<Any>, tax: Amount) {
    add_reward(candidate, events, Role::Dao, DAO, tax.clone(), 0);
    add_reward(candidate, events, Role::Developers, DEVELOPERS, tax, 0);
}

fn net_with_combined_tax(mut amount: Amount, rate: u32) -> Amount {
    amount -= commission(&amount, TREASURY_COMMISSION * 2);
    amount -= commission(&amount, rate);
    amount
}

// The returned tax is the extra amount paid to each of the two treasuries.
fn locked_bonus(
    version: Version,
    app: &AppValues,
    validator: &Validator,
    stake: &Stake,
    rate: u32,
    accumulated: &Amount,
    total_stake: &Amount,
) -> Result<(BigInt, BigInt)> {
    let weight = &stake.bip * REWARD_PERIOD;
    if version == Version::V3 {
        let safe = net_with_combined_tax(&app.safe_reward * &weight / &validator.stake, rate);
        let estimated = net_with_combined_tax(&app.reward * weight / &validator.stake, rate);
        return Ok((
            BigInt::from(safe * 3u32) - BigInt::from(estimated),
            BigInt::ZERO,
        ));
    }

    let (mut safe, mut estimated, divisor) = if accumulated.is_zero() {
        ensure!(
            !total_stake.is_zero(),
            "active validators have no total stake"
        );
        (&app.safe_reward * weight * 3u32, Amount::ZERO, total_stake)
    } else {
        if validator.reward.is_zero() {
            return Ok((BigInt::ZERO, BigInt::ZERO));
        }
        let weight = weight * &validator.reward;
        (
            &app.safe_reward * &weight * 3u32 / &validator.stake,
            &app.reward * weight / &validator.stake,
            accumulated,
        )
    };

    if version == Version::V4 {
        let safe = net_with_combined_tax(safe, rate) / divisor;
        let estimated = net_with_combined_tax(estimated, rate) / divisor;
        return Ok((BigInt::from(safe) - BigInt::from(estimated), BigInt::ZERO));
    }

    // V5Bug takes treasury taxes before dividing by the network-wide weight.
    if version == Version::V5Fix {
        safe /= divisor;
        estimated /= divisor;
    }
    let safe_tax = commission(&safe, TREASURY_COMMISSION);
    let estimated_tax = commission(&estimated, TREASURY_COMMISSION);
    safe -= &safe_tax * 2u32;
    estimated -= &estimated_tax * 2u32;
    // Both V5 formulas also apply the combined tax to the estimated reward.
    estimated -= commission(&estimated, TREASURY_COMMISSION * 2);
    safe -= commission(&safe, rate);
    estimated -= commission(&estimated, rate);
    if version == Version::V5Bug {
        safe /= divisor;
        estimated /= divisor;
    }
    Ok((
        BigInt::from(safe) - BigInt::from(estimated),
        BigInt::from(safe_tax) - BigInt::from(estimated_tax),
    ))
}

pub(crate) fn pay(
    version: Version,
    app: &mut AppValues,
    validators: &mut [Validator],
    candidates: &mut Candidates,
    locked: &BTreeSet<Address>,
    events: &mut Vec<Any>,
) -> Result<BigInt> {
    let accumulated: Amount = validators.iter().map(|v| &v.reward).sum();
    let total_stake: Amount = if accumulated.is_zero() {
        validators.iter().map(|v| &v.stake).sum()
    } else {
        Amount::ZERO
    };
    let mut additional_emission = BigInt::ZERO;
    let early_treasuries = matches!(version, Version::V3 | Version::V4);

    for validator in validators {
        let candidate = candidates.get_mut(validator.key)?;
        let tax = commission(&validator.reward, TREASURY_COMMISSION);
        let after_tax = &validator.reward - &tax * 2u32;
        let validator_reward = commission(&after_tax, candidate.commission);
        let distributable = after_tax - &validator_reward;
        if early_treasuries {
            pay_treasuries(candidate, events, tax.clone());
        }
        add_reward(
            candidate,
            events,
            Role::Validator,
            candidate.reward_address,
            validator_reward,
            0,
        );
        let mut remainder = distributable.clone();
        let mut treasury = BigInt::from(tax);
        let mut delegator_rewards = Vec::new();

        for stake in candidate
            .stakes
            .iter()
            .flatten()
            .filter(|stake| !stake.bip.is_zero())
        {
            ensure!(!validator.stake.is_zero(), "validator has no total stake");
            let reward = &distributable * &stake.bip / &validator.stake;
            ensure!(reward <= remainder, "negative reward remainder");
            remainder -= &reward;
            let mut payout = BigInt::from(reward.clone());
            if locked.contains(&stake.owner) {
                let (bonus, extra_tax) = locked_bonus(
                    version,
                    app,
                    validator,
                    stake,
                    candidate.commission,
                    &accumulated,
                    &total_stake,
                )?;
                treasury += &extra_tax;
                additional_emission += &extra_tax * 2u32;
                payout += &bonus;
                if payout.sign() == Sign::Plus {
                    additional_emission += bonus;
                }
            }
            if payout.sign() == Sign::Plus {
                delegator_rewards.push((stake.owner, payout.into_parts().1, stake.coin));
            }
        }

        for (owner, amount, coin) in delegator_rewards {
            add_reward(candidate, events, Role::Delegator, owner, amount, coin);
        }
        if !early_treasuries {
            pay_treasuries(
                candidate,
                events,
                treasury.to_biguint().context("negative treasury reward")?,
            );
        }
        validator.reward_dirty |= !validator.reward.is_zero();
        validator.reward = Amount::ZERO;
        if !remainder.is_zero() {
            app.slashed += remainder;
            app.dirty = true;
        }
    }
    Ok(additional_emission)
}
