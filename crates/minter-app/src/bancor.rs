use crate::{binary, state::Amount};
use anyhow::{Result, bail, ensure};
use num_traits::{One, Zero};

// Go's formula uses 100 significant binary bits with ties rounded to even.
const PRECISION: usize = 100;
const SCALE: usize = 384;

pub(crate) fn sale_return(
    supply: &Amount,
    reserve: &Amount,
    ratio: u32,
    sold: &Amount,
) -> Result<Amount> {
    ensure!(sold <= supply, "sale exceeds coin supply");
    if sold.is_zero() || reserve.is_zero() {
        return Ok(Amount::ZERO);
    }
    if sold == supply {
        return Ok(reserve.clone());
    }
    ensure!((1..=100).contains(&ratio), "invalid reserve ratio {ratio}");
    if ratio == 100 {
        return Ok(reserve * sold / supply);
    }

    let one = Amount::one() << SCALE;
    let sold = round(sold << SCALE);
    let supply = round(supply << SCALE);
    let reserve = round(reserve << SCALE);
    let base = round(&one - divide(&sold, &supply));
    let discount = power_change(&base, 100.0 / f64::from(ratio))?;
    Ok(round((discount * reserve) >> SCALE) >> SCALE)
}

pub(crate) fn sale_amount(
    supply: &Amount,
    reserve: &Amount,
    ratio: u32,
    wanted: &Amount,
) -> Result<Amount> {
    ensure!(wanted <= reserve, "withdrawal exceeds coin reserve");
    if wanted.is_zero() {
        return Ok(Amount::ZERO);
    }
    ensure!((1..=100).contains(&ratio), "invalid reserve ratio {ratio}");
    if ratio == 100 {
        return Ok(wanted * supply / reserve);
    }

    let reserve = round(reserve << SCALE);
    let wanted = round(wanted << SCALE);
    let supply = round(supply << SCALE);
    // Go rounds the subtraction before dividing by the reserve.
    let base = divide(&round(&reserve - wanted), &reserve);
    let discount = power_change(&base, f64::from(ratio) / 100.0)?;
    Ok(round((discount * supply) >> SCALE) >> SCALE)
}

pub(crate) fn purchase_return(
    supply: &Amount,
    reserve: &Amount,
    ratio: u32,
    deposit: &Amount,
) -> Result<Amount> {
    if deposit.is_zero() {
        return Ok(Amount::ZERO);
    }
    ensure!(!reserve.is_zero(), "empty coin reserve");
    ensure!((1..=100).contains(&ratio), "invalid reserve ratio {ratio}");
    if ratio == 100 {
        return Ok(supply * deposit / reserve);
    }
    growth(deposit, reserve, supply, f64::from(ratio) / 100.0)
}

pub(crate) fn purchase_amount(
    supply: &Amount,
    reserve: &Amount,
    ratio: u32,
    wanted: &Amount,
) -> Result<Amount> {
    if wanted.is_zero() {
        return Ok(Amount::ZERO);
    }
    ensure!(!supply.is_zero(), "empty coin supply");
    ensure!((1..=100).contains(&ratio), "invalid reserve ratio {ratio}");
    if ratio == 100 {
        return Ok(wanted * reserve / supply);
    }

    let supply = round(supply << SCALE);
    let reserve = round(reserve << SCALE);
    let wanted = round(wanted << SCALE);
    let base = divide(&round(&supply + wanted), &supply);
    let growth = power_change(&base, 100.0 / f64::from(ratio))?;
    Ok(round((growth * reserve) >> SCALE) >> SCALE)
}

pub(crate) fn delegated_reserve(
    supply: &Amount,
    reserve: &Amount,
    ratio: u32,
    delegated: &Amount,
) -> Result<Amount> {
    if delegated <= supply {
        let liquid = supply - delegated;
        return Ok(reserve - sale_return(supply, reserve, ratio, &liquid)?);
    }
    // Go can quote a hypothetical delegation above supply before checking the account.
    // Its sale return on the negative liquid amount increases the attributed reserve.
    let excess = delegated - supply;
    ensure!(
        !supply.is_zero() && (1..=100).contains(&ratio),
        "invalid delegation coin"
    );
    let increase = if ratio == 100 {
        (reserve * excess + supply - 1u32) / supply
    } else {
        growth(&excess, supply, reserve, 100.0 / f64::from(ratio))?
    };
    Ok(reserve + increase)
}

fn growth(
    numerator: &Amount,
    denominator: &Amount,
    multiplier: &Amount,
    exponent: f64,
) -> Result<Amount> {
    let one = Amount::one() << SCALE;
    let numerator = round(numerator << SCALE);
    let denominator = round(denominator << SCALE);
    let multiplier = round(multiplier << SCALE);
    let base = round(one + divide(&numerator, &denominator));
    let change = power_change(&base, exponent)?;
    Ok(round((change * multiplier) >> SCALE) >> SCALE)
}

fn round(value: Amount) -> Amount {
    binary::round(value, PRECISION)
}

fn divide(numerator: &Amount, denominator: &Amount) -> Amount {
    binary::divide(numerator, denominator, SCALE, PRECISION)
}

fn power_change(base: &Amount, exponent: f64) -> Result<Amount> {
    // In Go the exponent is first evaluated as float64. Its bits are an
    // exact dyadic fraction. Repeated roots evaluate the fractional bits
    // using integer arithmetic on every platform.
    let bits = exponent.to_bits();
    let fraction_bits = 1075 - ((bits >> 52) & 0x7ff) as usize;
    let significand = (1u64 << 52) | (bits & ((1u64 << 52) - 1));
    let integral = significand >> fraction_bits;
    let fraction = significand - (integral << fraction_bits);
    let grows = base > &(Amount::one() << SCALE);

    for scale in [SCALE, SCALE * 2, SCALE * 4] {
        let one = Amount::one() << scale;
        let base = base << (scale - SCALE);
        let (mut lower, mut upper) = if integral == 0 {
            (one.clone(), one.clone())
        } else {
            let exact = base.pow(integral as u32);
            let discarded = scale * (integral as usize - 1);
            (&exact >> discarded, ceil_shift(exact, discarded))
        };
        let mut root_lower = base.clone();
        let mut root_upper = base;
        for bit in (fraction.trailing_zeros() as usize..fraction_bits).rev() {
            root_lower = (&root_lower << scale).sqrt();
            let radicand = &root_upper << scale;
            root_upper = radicand.sqrt();
            if &root_upper * &root_upper != radicand {
                root_upper += 1u32;
            }
            if fraction & (1u64 << bit) != 0 {
                lower = (lower * &root_lower) >> scale;
                upper = ceil_shift(upper * &root_upper, scale);
            }
        }
        let (low, high) = if grows {
            (round(round(lower) - &one), round(round(upper) - &one))
        } else {
            (round(&one - round(upper)), round(&one - round(lower)))
        };
        if low == high {
            return Ok(low >> (scale - SCALE));
        }
    }
    bail!("Bancor power could not be rounded")
}

fn ceil_shift(value: Amount, bits: usize) -> Amount {
    let mut top = &value >> bits;
    if (&top << bits) != value {
        top += 1u32;
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn reserve_formulas_match_go_rounding() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Sample {
            supply: String,
            reserve: String,
            ratio: u32,
            sold: String,
            r#return: String,
            want_receive: String,
            sale_amount: String,
            purchase_return: String,
            purchase_amount: String,
        }
        let samples: Vec<Sample> =
            serde_json::from_str(include_str!("../tests/fixtures/go-bancor-sale.json")).unwrap();
        for sample in samples {
            let supply = sample.supply.parse().unwrap();
            let reserve = sample.reserve.parse().unwrap();
            let sold = sample.sold.parse().unwrap();
            let wanted = sample.want_receive.parse().unwrap();
            let got = sale_return(&supply, &reserve, sample.ratio, &sold).unwrap();
            assert_eq!(
                got.to_string(),
                sample.r#return,
                "CRR {} sold {}",
                sample.ratio,
                sample.sold
            );
            let got = sale_amount(&supply, &reserve, sample.ratio, &wanted).unwrap();
            assert_eq!(
                got.to_string(),
                sample.sale_amount,
                "CRR {} receive {}",
                sample.ratio,
                sample.want_receive
            );
            let bought = purchase_return(&supply, &reserve, sample.ratio, &wanted).unwrap();
            assert_eq!(
                bought.to_string(),
                sample.purchase_return,
                "CRR {}",
                sample.ratio
            );
            let deposit = purchase_amount(&supply, &reserve, sample.ratio, &sold).unwrap();
            assert_eq!(
                deposit.to_string(),
                sample.purchase_amount,
                "CRR {}",
                sample.ratio
            );
        }
    }
}
