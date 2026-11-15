mod matching;

use crate::{
    orders::{self, Book, Order, Refund, Update},
    state::{Amount, amount, finish, full_list, list, put_amount, set},
};
use alloy_rlp::{Decodable, Encodable};
use anyhow::{Context, Result, ensure};
use minter_protocol::Address;
use minter_state::{BlockStore, Mutation};
use serde::Serialize;
use std::collections::BTreeMap;

pub(crate) const BURN_ADDRESS: Address = [
    0x00, 0xce, 0xdd, 0xe7, 0x86, 0xb3, 0x4d, 0x73, 0x3d, 0x1d, 0xc9, 0x65, 0x59, 0x25, 0x30, 0x81,
    0x57, 0x2d, 0xf2, 0xc6,
];
type PairKey = [u8; 10];
pub(crate) const NEXT_POOL_ID: &[u8] = b"si";

fn pool_key(pair: [u32; 2]) -> PairKey {
    let mut key = [0; 10];
    key[..2].copy_from_slice(b"sd");
    key[2..6].copy_from_slice(&pair[0].to_le_bytes());
    key[6..].copy_from_slice(&pair[1].to_le_bytes());
    key
}

#[derive(Clone)]
pub(crate) struct Pool {
    key: PairKey,
    pair: [u32; 2],
    reserves: [Amount; 2],
    pub id: u32,
}

pub(crate) struct View<'a> {
    pub store: &'a BlockStore,
    pub height: u64,
    pub changes: Option<&'a Changes>,
}

impl View<'_> {
    pub fn load(&self, from: u32, to: u32) -> Result<Option<Pool>> {
        let pair = [from.min(to), from.max(to)];
        let key = pool_key(pair);
        if let Some(pool) = self.changes.and_then(|changes| changes.reserves.get(&key)) {
            return Ok(Some(pool.clone()));
        }
        let Some(raw) = self.store.value_at(self.height, &key)? else {
            return Ok(None);
        };
        let mut fields = full_list(&raw)?;
        let reserves = [amount(&mut fields)?, amount(&mut fields)?];
        let id = u32::decode(&mut fields)?;
        finish(fields)?;
        ensure!(
            reserves.iter().all(|reserve| reserve.bits() > 0),
            "empty swap pool"
        );
        Ok(Some(Pool {
            key,
            pair,
            reserves,
            id,
        }))
    }

    pub fn order(&self, id: u32) -> Result<Option<Order>> {
        match self.changes {
            Some(changes) => changes.orders.current(self.store, self.height, id),
            None => Order::find(self.store, self.height, id),
        }
    }

    pub fn next_order_id(&self) -> Result<u32> {
        match self.changes {
            Some(changes) => changes.orders.next_id(self.store, self.height),
            None => orders::next_id_at(self.store, self.height),
        }
    }

    pub fn next_pool_id(&self) -> Result<u32> {
        if let Some(id) = self.changes.and_then(|changes| changes.next_pool_id) {
            return Ok(id);
        }
        let Some(raw) = self.store.value_at(self.height, NEXT_POOL_ID)? else {
            return Ok(1);
        };
        let mut encoded = raw.as_slice();
        let id = u32::decode(&mut encoded)?;
        finish(encoded)?;
        ensure!(id > 0, "invalid next pool ID");
        Ok(id)
    }

    fn book<'a>(&'a self, pool: &Pool, reverse: bool, overlay: &'a [Update]) -> Result<Book<'a>> {
        Book::new(
            self.store,
            self.height,
            self.changes.map(|state| &state.orders),
            overlay,
            pool.pair,
            reverse,
        )
    }
}

#[derive(Default)]
pub(crate) struct Changes {
    reserves: BTreeMap<PairKey, Pool>,
    next_pool_id: Option<u32>,
    pub orders: orders::Changes,
}

impl Changes {
    pub fn register_pool(&mut self, pair: [u32; 2], reserves: [Amount; 2], id: u32) -> Result<()> {
        ensure!(pair[0] < pair[1] && id > 0, "invalid swap pool");
        ensure!(
            reserves.iter().all(|reserve| reserve.bits() > 0),
            "empty swap pool"
        );
        let key = pool_key(pair);
        ensure!(
            self.reserves
                .insert(
                    key,
                    Pool {
                        key,
                        pair,
                        reserves,
                        id
                    }
                )
                .is_none(),
            "duplicate swap pool pair"
        );
        Ok(())
    }

    pub fn create_pool(&mut self, pair: [u32; 2], reserves: [Amount; 2], id: u32) -> Result<()> {
        let next_id = id.checked_add(1).context("pool ID overflow")?;
        self.register_pool(pair, reserves, id)?;
        self.next_pool_id = Some(next_id);
        Ok(())
    }

    pub fn apply(&mut self, step: &SwapStep) {
        self.replace(step.pool.clone());
        self.orders.apply(&step.order_updates);
    }

    pub fn replace(&mut self, pool: Pool) {
        self.reserves.insert(pool.key, pool);
    }

    pub fn write(self, ops: &mut Vec<Mutation>) -> Result<()> {
        if let Some(id) = self.next_pool_id {
            ops.push(set(NEXT_POOL_ID.to_vec(), alloy_rlp::encode(id)));
        }
        self.orders.write_next_id(ops);
        for (key, pool) in self.reserves.into_iter().rev() {
            ops.push(set(
                key.to_vec(),
                list(|out| {
                    put_amount(&pool.reserves[0], out);
                    put_amount(&pool.reserves[1], out);
                    pool.id.encode(out);
                }),
            ));
        }
        self.orders.write(ops)
    }
}

pub(crate) struct SwapStep {
    pool: Pool,
    pub coin_in: u32,
    pub coin_out: u32,
    pub input: Amount,
    pub output: Amount,
    pub burned: Amount,
    pool_input: Amount,
    pool_output: Amount,
    order_fee_in: Amount,
    order_fee_out: Amount,
    order_updates: Vec<Update>,
    fills: Vec<Fill>,
    pub refunds: Vec<Refund>,
    pub makers: BTreeMap<Address, Amount>,
}

struct Fill {
    id: u32,
    owner: Address,
    buy: Amount,
    sell: Amount,
}

struct Match {
    order: Order,
    buy: Amount,
    sell: Amount,
}

#[derive(Serialize)]
pub(crate) struct SwapEvent {
    pool_id: u32,
    coin_in: u32,
    value_in: String,
    coin_out: u32,
    value_out: String,
    details: SwapDetails,
}

#[derive(Serialize)]
struct SwapDetails {
    amount_in: String,
    amount_out: String,
    commission_amount_in: String,
    commission_amount_out: String,
    amount_in_burned: String,
    orders: Option<Vec<FillEvent>>,
}

#[derive(Serialize)]
struct FillEvent {
    buy: String,
    sell: String,
    seller: String,
    id: u32,
}

impl SwapStep {
    pub fn event(&self) -> SwapEvent {
        SwapEvent {
            pool_id: self.pool.id,
            coin_in: self.coin_in,
            value_in: self.input.to_string(),
            coin_out: self.coin_out,
            value_out: self.output.to_string(),
            details: SwapDetails {
                amount_in: self.pool_input.to_string(),
                amount_out: self.pool_output.to_string(),
                commission_amount_in: self.order_fee_in.to_string(),
                commission_amount_out: self.order_fee_out.to_string(),
                amount_in_burned: self.burned.to_string(),
                orders: (!self.fills.is_empty()).then(|| {
                    self.fills
                        .iter()
                        .map(|fill| FillEvent {
                            buy: fill.buy.to_string(),
                            sell: fill.sell.to_string(),
                            seller: format!("Mx{}", hex::encode(fill.owner)),
                            id: fill.id,
                        })
                        .collect()
                }),
            },
        }
    }

    pub fn pool_for_route(
        &self,
        view: &View<'_>,
        original: &Pool,
        from: u32,
        check: bool,
    ) -> Result<(Pool, Vec<Update>)> {
        let index = usize::from(self.coin_in > self.coin_out);
        // Go projects the gross input and exact-output order fills in the fee
        // direction. The reverse direction uses the net sale and its fills.
        if check && from == self.coin_in && !self.fills.is_empty() {
            let projection = original
                .clone()
                .buy(view, self.coin_in, self.coin_out, &self.output, &[])?
                .context("gas projection has insufficient liquidity")?;
            let bought: Amount = projection.fills.iter().map(|fill| &fill.buy).sum();
            let sold: Amount = projection.fills.iter().map(|fill| &fill.sell).sum();
            let mut pool = original.clone();
            pool.reserves[index] += &self.input;
            pool.reserves[1 - index] += sold;
            ensure!(
                pool.reserves[index] >= bought && pool.reserves[1 - index] >= self.output,
                "gas projection exceeds reserves"
            );
            pool.reserves[index] -= bought;
            pool.reserves[1 - index] -= &self.output;
            return Ok((pool, projection.order_updates));
        }
        let mut pool = self.pool.clone();
        if check && from == self.coin_in {
            pool.reserves[index] += &self.burned;
        }
        Ok((pool, self.order_updates.clone()))
    }

    pub fn uses_pool(&self, pool: &Pool) -> bool {
        self.pool.key == pool.key
    }
}

impl Pool {
    pub fn reserves(&self, from: u32, to: u32) -> (&Amount, &Amount) {
        let index = usize::from(from > to);
        (&self.reserves[index], &self.reserves[1 - index])
    }

    pub fn change_liquidity(
        &mut self,
        pair: [u32; 2],
        volumes: &[Amount; 2],
        add: bool,
    ) -> Result<()> {
        let reverse = usize::from(pair[0] > pair[1]);
        for (i, volume) in volumes.iter().enumerate() {
            let reserve = &mut self.reserves[i ^ reverse];
            if add {
                *reserve += volume;
            } else {
                ensure!(
                    &*reserve >= volume,
                    "liquidity removal exceeds pool reserve"
                );
                *reserve -= volume;
            }
        }
        Ok(())
    }

    fn settle(
        mut self,
        from: u32,
        to: u32,
        net: Amount,
        output: Amount,
        burned: Amount,
        matches: Vec<Match>,
    ) -> Result<SwapStep> {
        let mut order_fee_in = Amount::ZERO;
        let mut order_fee_out = Amount::ZERO;
        let mut bought = Amount::ZERO;
        let mut sold = Amount::ZERO;
        let mut makers = BTreeMap::<Address, Amount>::new();
        let mut updates = Vec::with_capacity(matches.len());
        let mut fills = Vec::with_capacity(matches.len());
        let mut refunds = Vec::new();
        for Match { order, buy, sell } in matches {
            order_fee_in += matching::fee(&buy, 1000);
            order_fee_out += matching::fee(&sell, 1000);
            bought += &buy;
            sold += &sell;
            *makers.entry(order.owner).or_default() += &buy;
            fills.push(Fill {
                id: order.id,
                owner: order.owner,
                buy: buy.clone(),
                sell: sell.clone(),
            });
            let (update, refund) = order.consume(&buy, &sell)?;
            updates.push(update);
            refunds.extend(refund);
        }
        bought += &order_fee_in;
        sold -= &order_fee_out;
        ensure!(
            net >= bought && output >= sold,
            "order matching exceeds swap volumes"
        );
        let pool_input = &net - bought;
        let pool_output = &output - sold;
        let index = usize::from(from > to);
        self.reserves[index] += &pool_input + &order_fee_in;
        self.reserves[1 - index] += &order_fee_out;
        ensure!(
            self.reserves[1 - index] >= pool_output,
            "swap output exceeds reserve"
        );
        self.reserves[1 - index] -= &pool_output;
        Ok(SwapStep {
            pool: self,
            coin_in: from,
            coin_out: to,
            input: net + &burned,
            output,
            burned,
            pool_input,
            pool_output,
            order_fee_in,
            order_fee_out,
            order_updates: updates,
            fills,
            refunds,
            makers,
        })
    }
}

pub(crate) fn commission_in_bip(
    view: &View<'_>,
    price_coin: u32,
    input: &Amount,
) -> Result<Amount> {
    if price_coin == 0 || input.bits() == 0 {
        return Ok(input.clone());
    }
    let pool = view
        .load(price_coin, 0)?
        .context("commission pool does not exist")?;
    Ok(pool.sell(view, price_coin, 0, input.clone(), &[])?.output)
}
