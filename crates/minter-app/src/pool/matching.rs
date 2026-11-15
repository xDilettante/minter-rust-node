use super::{Match, Pool, SwapStep, View};
use crate::{orders::Update, state::Amount};
use anyhow::{Context, Result, ensure};
use num_rational::Ratio;
use num_traits::{FromPrimitive, ToPrimitive, Zero};

pub(super) fn fee(amount: &Amount, denominator: u32) -> Amount {
    (amount + (denominator - 1)) / denominator
}

impl Pool {
    pub fn sell(
        self,
        view: &View<'_>,
        from: u32,
        to: u32,
        input: Amount,
        overlay: &[Update],
    ) -> Result<SwapStep> {
        let burned = fee(&input, 1000);
        let net = input - &burned;
        let (reserve_in, reserve_out) = self.reserves(from, to);
        let (mut reserve_in, mut reserve_out) = (reserve_in.clone(), reserve_out.clone());
        let mut left = net.clone();
        let mut output = Amount::ZERO;
        let mut matches = Vec::new();
        if !left.is_zero() {
            let mut book = view.book(&self, from > to, overlay)?;
            while !left.is_zero() {
                let Some(order) = book.next()? else { break };
                let (want_buy, want_sell) = order.amounts_for_taker();
                if let Some((used, received)) =
                    pool_before_order(&reserve_in, &reserve_out, want_buy, want_sell)?
                {
                    if left <= used {
                        break;
                    }
                    left -= &used;
                    reserve_in += used;
                    reserve_out -= &received;
                    output += received;
                }

                let buy = &left - fee(&left, 1001);
                if &buy <= want_buy {
                    // The Go rational is rounded with at least the precision of
                    // its numerator and denominator before truncation. That
                    // precision keeps it on the same side of the next integer.
                    let sell = &buy * want_sell / want_buy;
                    output += &sell - fee(&sell, 1000);
                    matches.push(Match { order, buy, sell });
                    left = Amount::ZERO;
                    break;
                }
                let maker_fee_in = fee(want_buy, 1000);
                let maker_fee_out = fee(want_sell, 1000);
                left -= want_buy + &maker_fee_in;
                output += want_sell - &maker_fee_out;
                reserve_in += maker_fee_in;
                reserve_out += maker_fee_out;
                matches.push(Match {
                    buy: want_buy.clone(),
                    sell: want_sell.clone(),
                    order,
                });
            }
        }
        if !left.is_zero()
            && let Some(remaining) = constant_product_output(&reserve_in, &reserve_out, &left)
        {
            output += remaining;
        }
        self.settle(from, to, net, output, burned, matches)
    }

    pub fn buy(
        self,
        view: &View<'_>,
        from: u32,
        to: u32,
        output: &Amount,
        overlay: &[Update],
    ) -> Result<Option<SwapStep>> {
        let (reserve_in, reserve_out) = self.reserves(from, to);
        let (mut reserve_in, mut reserve_out) = (reserve_in.clone(), reserve_out.clone());
        let mut left = output.clone();
        let mut net = Amount::ZERO;
        let mut matches = Vec::new();
        if !left.is_zero() {
            let mut book = view.book(&self, from > to, overlay)?;
            while !left.is_zero() {
                let Some(order) = book.next()? else { break };
                let (want_buy, want_sell) = order.amounts_for_taker();
                if let Some((used, received)) =
                    pool_before_order(&reserve_in, &reserve_out, want_buy, want_sell)?
                {
                    if left <= received {
                        break;
                    }
                    left -= &received;
                    reserve_in += &used;
                    reserve_out -= received;
                    net += used;
                }

                let sell = &left + fee(&left, 999);
                if &sell <= want_sell {
                    let buy = &sell * want_buy / want_sell;
                    net += &buy + fee(&buy, 1000);
                    matches.push(Match { order, buy, sell });
                    left = Amount::ZERO;
                    break;
                }
                let maker_fee_in = fee(want_buy, 1000);
                let maker_fee_out = fee(want_sell, 1000);
                left -= want_sell - &maker_fee_out;
                net += want_buy + &maker_fee_in;
                reserve_in += maker_fee_in;
                reserve_out += maker_fee_out;
                matches.push(Match {
                    buy: want_buy.clone(),
                    sell: want_sell.clone(),
                    order,
                });
            }
        }
        if !left.is_zero() {
            if left >= reserve_out {
                return Ok(None);
            }
            net += &reserve_in * &left * 1000u32 / (&reserve_out - &left) / 998u32 + 1u32;
        }
        let burned = fee(&net, 999);
        Ok(Some(self.settle(
            from,
            to,
            net,
            output.clone(),
            burned,
            matches,
        )?))
    }
}

fn constant_product_output(
    reserve_in: &Amount,
    reserve_out: &Amount,
    input: &Amount,
) -> Option<Amount> {
    let balance_adjusted = reserve_in * 1000u32 + input * 998u32;
    let remaining = reserve_in * reserve_out * 1000u32 / balance_adjusted + 1u32;
    (&remaining < reserve_out).then(|| reserve_out - remaining)
}

fn pool_before_order(
    reserve_in: &Amount,
    reserve_out: &Amount,
    order_buy: &Amount,
    order_sell: &Amount,
) -> Result<Option<(Amount, Amount)>> {
    if reserve_out * order_buy <= reserve_in * order_sell {
        return Ok(None);
    }
    // Go uses big.Float at 53 bits and rounds after each operation.
    ensure!(
        order_buy.bits() <= 128 && order_sell.bits() <= 128,
        "order exceeds quote bounds"
    );
    let r0 = reserve_in
        .to_u128()
        .context("pool input reserve exceeds quote bounds")? as f64;
    let r1 = reserve_out
        .to_u128()
        .context("pool output reserve exceeds quote bounds")? as f64;
    let price = Ratio::new_raw(order_sell.clone(), order_buy.clone())
        .to_f64()
        .context("invalid order price")?;
    let k = r0 * r1;
    let r0_squared = r0 * r0;
    let b = 999.0 * r0;
    let k_over_price = k * (1.0 / price);
    let discriminant = 998001.0 * r0_squared - 998000.0 * (r0_squared - k_over_price);
    let threshold = (-b + discriminant.sqrt()) / 998.0;
    let Some(threshold) = Amount::from_f64(threshold) else {
        return Ok(None);
    };
    Ok(constant_product_output(reserve_in, reserve_out, &threshold)
        .map(|output| (threshold, output)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_at_the_first_order() {
        // SwapV2 reserves and first order from Go at 40_574_359.
        let reserve_in = Amount::from(367_371_547_849_934_766_624u128);
        let reserve_out = Amount::from(3_912_039_879_765_074_829_282_498u128);
        let order_buy = Amount::from(109_852_379_850_520_116_687u128);
        let order_sell = Amount::from(366_174_599_501_733_722_286_733u128);
        let threshold = Amount::from(289_539_440_827_115_732_992u128);
        assert_eq!(
            pool_before_order(&reserve_in, &reserve_out, &order_buy, &order_sell)
                .unwrap()
                .unwrap()
                .0,
            threshold
        );
        let input = Amount::from(42_557_400_000_000_000u64);
        assert_eq!(
            constant_product_output(&reserve_in, &reserve_out, &input),
            Some(Amount::from(452_223_552_672_390_789_269u128))
        );
    }
}
