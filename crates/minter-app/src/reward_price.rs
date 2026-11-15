use crate::{
    binary,
    state::{Amount, amount, finish, full_list, list, put_amount},
};
use alloy_rlp::{Decodable, Encodable};
use anyhow::{Result, ensure};
use num_rational::Ratio;
use serde::{Deserialize, Serialize};

pub(crate) const USDT: u32 = 1993;
const PIP: u64 = 1_000_000_000_000_000_000;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Price {
    pub time: u64,
    pub bip: Amount,
    pub usdt: Amount,
    pub off: bool,
    pub last: Amount,
}

impl Price {
    pub fn encode(&self) -> Vec<u8> {
        list(|out| {
            self.time.encode(out);
            put_amount(&self.bip, out);
            put_amount(&self.usdt, out);
            self.off.encode(out);
            put_amount(&self.last, out);
        })
    }

    pub fn decode(raw: &[u8]) -> Result<Self> {
        let mut fields = full_list(raw)?;
        let price = Self {
            time: u64::decode(&mut fields)?,
            bip: amount(&mut fields)?,
            usdt: amount(&mut fields)?,
            off: bool::decode(&mut fields)?,
            last: amount(&mut fields)?,
        };
        finish(fields)?;
        Ok(price)
    }

    pub fn update(
        previous: Option<&Self>,
        time: u64,
        bip: &Amount,
        usdt: &Amount,
        fixed: bool,
    ) -> Result<(Self, Amount)> {
        let safe = safe_reward(bip, usdt)?;
        let (last, off) = if let Some(previous) = previous {
            ensure!(
                previous.bip.bits() > 0 && previous.usdt.bits() > 0,
                "empty previous price reserves"
            );
            let old = &previous.usdt * bip;
            let new = usdt * &previous.bip;
            let recovering = if fixed {
                previous.last < safe
            } else {
                new >= old
            };
            // The reference floors the signed percentage. A change below -9% floors to at most -10.
            if &new * 100u32 < &old * 91u32 {
                (Amount::ZERO, true)
            } else if previous.off && recovering {
                let last = &previous.last + 10 * PIP;
                if last < safe {
                    (last, true)
                } else {
                    (safe.clone(), false)
                }
            } else {
                (safe.clone(), false)
            }
        } else {
            (safe.clone(), false)
        };
        Ok((
            Self {
                time,
                bip: bip.clone(),
                usdt: usdt.clone(),
                off,
                last,
            },
            safe,
        ))
    }
}

fn safe_reward(bip: &Amount, usdt: &Amount) -> Result<Amount> {
    ensure!(bip.bits() > 0 && usdt.bits() > 0, "empty reward price pool");
    // Go SetRat chooses the precision from the reduced numerator and denominator.
    let ratio = Ratio::new(usdt.clone(), bip.clone());
    let numerator = ratio.numer();
    let denominator = ratio.denom();
    let precision = numerator.bits().max(denominator.bits()).max(64) as usize;
    let scale = precision + 64 + denominator.bits().saturating_sub(numerator.bits()) as usize;
    let price = binary::divide(numerator, denominator, scale, precision);
    let radicand = price << (3 * scale);
    let root = radicand.sqrt().sqrt();
    let drop = (root.bits() as usize).saturating_sub(precision);
    let digits = root >> drop;
    // Compare the exact fourth power of the midpoint before rounding to even.
    let midpoint = ((&digits << 1usize) + 1u32) << drop;
    let half = (radicand << 4usize).cmp(&midpoint.pow(4));
    let root = binary::rounded(digits, half) << drop;
    let scaled = binary::round(root * 350u32, precision);
    Ok(binary::round(scaled * PIP, precision) >> scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn matches_go_reward_price_and_recovery() {
        #[derive(Deserialize)]
        struct Sample {
            series: String,
            time: u64,
            bip: String,
            usdt: String,
            reward: String,
            safe: String,
            off: bool,
        }
        let samples: Vec<Sample> =
            serde_json::from_str(include_str!("../tests/fixtures/go-reward-price.json")).unwrap();
        let mut prior = BTreeMap::new();
        for sample in samples {
            let (price, safe) = Price::update(
                prior.get(&sample.series),
                sample.time,
                &sample.bip.parse().unwrap(),
                &sample.usdt.parse().unwrap(),
                sample.series == "fixed",
            )
            .unwrap();
            assert_eq!(
                (price.last.to_string(), safe.to_string(), price.off),
                (sample.reward, sample.safe, sample.off),
                "{} at {}",
                sample.series,
                sample.time,
            );
            prior.insert(sample.series, price);
        }
    }
}
