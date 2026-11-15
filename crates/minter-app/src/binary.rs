use crate::state::Amount;
use num_traits::One;
use std::cmp::Ordering;

pub(crate) fn round(value: Amount, precision: usize) -> Amount {
    let drop = (value.bits() as usize).saturating_sub(precision);
    if drop == 0 {
        return value;
    }
    let digits = &value >> drop;
    let rest = value - (&digits << drop);
    rounded(digits, (rest << 1usize).cmp(&(Amount::one() << drop))) << drop
}

pub(crate) fn divide(
    numerator: &Amount,
    denominator: &Amount,
    scale: usize,
    precision: usize,
) -> Amount {
    let scaled = numerator << scale;
    let floor = &scaled / denominator;
    let drop = (floor.bits() as usize).saturating_sub(precision);
    let unit = denominator << drop;
    let half = ((scaled % &unit) << 1usize).cmp(&unit);
    rounded(floor >> drop, half) << drop
}

pub(crate) fn rounded(mut digits: Amount, half: Ordering) -> Amount {
    match half {
        Ordering::Greater => digits += 1u32,
        Ordering::Equal if digits.bit(0) => digits += 1u32,
        _ => {}
    }
    digits
}
