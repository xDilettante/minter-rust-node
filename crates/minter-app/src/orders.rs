use crate::{
    state::{Amount, amount, finish, full_list, list, put_amount, set},
    transaction::display_address,
};
use alloy_rlp::{Decodable, Encodable, Header};
use anyhow::{Context, Result, ensure};
use minter_api::pb::OrderExpiredEvent;
use minter_protocol::Address;
use minter_state::{BlockStore, Mutation};
use num_rational::Ratio;
use num_traits::ToPrimitive;
use std::collections::{BTreeMap, VecDeque};

pub(crate) const MIN_ORDER_VOLUME: u64 = 10_000_000_000;

#[derive(Clone)]
pub(crate) struct Order {
    pub id: u32,
    pub want_buy: Amount,
    pub want_sell: Amount,
    pub is_buy: bool,
    pub pair: [u32; 2],
    pub owner: Address,
    pub height: u64,
}

impl Order {
    pub fn find(store: &BlockStore, height: u64, id: u32) -> Result<Option<Self>> {
        let Some(raw) = store.value_at(height, &order_key(id))? else {
            return Ok(None);
        };
        let mut fields = full_list(&raw)?;
        let want_buy = amount(&mut fields)?;
        let want_sell = amount(&mut fields)?;
        let owner = Address::decode(&mut fields)?;
        let is_buy = bool::decode(&mut fields)?;
        let height = u64::decode(&mut fields)?;
        let mut coins = Header::decode_bytes(&mut fields, true)?;
        let pair = [u32::decode(&mut coins)?, u32::decode(&mut coins)?];
        finish(coins)?;
        finish(fields)?;
        ensure!(pair[0] < pair[1], "invalid stored order pair");
        ensure!(
            want_buy.bits() > 0 && want_sell.bits() > 0,
            "empty stored order"
        );
        Ok(Some(Self {
            id,
            want_buy,
            want_sell,
            is_buy,
            pair,
            owner,
            height,
        }))
    }

    fn load(store: &BlockStore, height: u64, id: u32) -> Result<Self> {
        Self::find(store, height, id)?.context("indexed swap order does not exist")
    }

    fn encode(&self) -> Vec<u8> {
        list(|out| {
            put_amount(&self.want_buy, out);
            put_amount(&self.want_sell, out);
            self.owner.encode(out);
            self.is_buy.encode(out);
            self.height.encode(out);
            out.extend(list(|out| {
                self.pair[0].encode(out);
                self.pair[1].encode(out);
            }));
        })
    }

    fn index_key(&self) -> Result<Vec<u8>> {
        let price = Ratio::new_raw(self.want_sell.clone(), self.want_buy.clone())
            .to_f64()
            .filter(|price| price.is_normal())
            .context("swap order price exceeds the supported range")?;
        // Go stores the canonical pair's 53-bit price as an exponent, digit and
        // two 19-digit groups, using 38 fractional digits in scientific notation.
        let text = format!("{price:.38e}");
        let (digits, exponent) = text.split_once('e').context("invalid scientific price")?;
        let (digit, fraction) = digits.split_once('.').context("invalid price digits")?;
        let (left, right) = fraction.split_at(19);
        let exponent = exponent.parse::<i16>()? + 127;
        let side = u8::from(!self.is_buy);
        let index_id = if self.is_buy {
            self.id
        } else {
            u32::MAX - self.id
        };
        let mut key = Vec::with_capacity(33);
        key.extend(b"so");
        key.extend(pair_bytes(self.pair));
        key.extend([side, exponent as u8, digit.parse::<u8>()?]);
        key.extend(left.parse::<u64>()?.to_be_bytes());
        key.extend(right.parse::<u64>()?.to_be_bytes());
        key.extend(index_id.to_be_bytes());
        Ok(key)
    }

    pub fn amounts_for_taker(&self) -> (&Amount, &Amount) {
        if self.is_buy {
            (&self.want_sell, &self.want_buy)
        } else {
            (&self.want_buy, &self.want_sell)
        }
    }

    pub fn refund(&self) -> Refund {
        let coin = self.pair[usize::from(!self.is_buy)];
        let (_, value) = self.amounts_for_taker();
        Refund {
            id: self.id,
            owner: self.owner,
            coin,
            value: value.clone(),
        }
    }

    pub fn consume(self, buy: &Amount, sell: &Amount) -> Result<(Update, Option<Refund>)> {
        let (want_buy, want_sell) = self.amounts_for_taker();
        ensure!(
            buy <= want_buy && sell <= want_sell,
            "order fill exceeds its volumes"
        );
        let left_buy = want_buy - buy;
        let left_sell = want_sell - sell;
        ensure!(
            (left_buy.bits() == 0) == (left_sell.bits() == 0),
            "order has one empty side"
        );
        let mut after = self.clone();
        (after.want_buy, after.want_sell) = if self.is_buy {
            (left_sell, left_buy)
        } else {
            (left_buy, left_sell)
        };
        let minimum = Amount::from(MIN_ORDER_VOLUME);
        let (after, refund) = if after.want_buy.bits() == 0 {
            (None, None)
        } else if after.want_buy < minimum || after.want_sell < minimum {
            (None, Some(after.refund()))
        } else {
            (Some(after), None)
        };
        Ok((
            Update {
                before: self,
                after,
            },
            refund,
        ))
    }
}

fn pair_bytes(pair: [u32; 2]) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&pair[0].to_le_bytes());
    bytes[4..].copy_from_slice(&pair[1].to_le_bytes());
    bytes
}

#[derive(Clone)]
pub(crate) struct Update {
    before: Order,
    after: Option<Order>,
}

impl Update {
    pub fn deletes(&self, id: u32) -> bool {
        self.before.id == id && self.after.is_none()
    }
}

#[derive(Default)]
pub(crate) struct Changes {
    by_id: BTreeMap<u32, OrderChange>,
    next_id: Option<u32>,
}

struct OrderChange {
    original: Option<Order>,
    current: Option<Order>,
}

impl Changes {
    pub fn import(&mut self, next_id: u32, orders: Vec<Order>) -> Result<()> {
        for order in orders {
            ensure!(
                order.id > 0
                    && order.id < next_id
                    && order.want_buy.bits() > 0
                    && order.want_sell.bits() > 0,
                "invalid genesis order"
            );
            ensure!(
                self.by_id
                    .insert(
                        order.id,
                        OrderChange {
                            original: None,
                            current: Some(order),
                        }
                    )
                    .is_none(),
                "duplicate genesis order"
            );
        }
        self.next_id = (next_id > 1).then_some(next_id);
        Ok(())
    }

    pub fn apply(&mut self, updates: &[Update]) {
        for update in updates {
            self.update(update);
        }
    }

    fn update(&mut self, update: &Update) {
        let change = self
            .by_id
            .entry(update.before.id)
            .or_insert_with(|| OrderChange {
                original: Some(update.before.clone()),
                current: None,
            });
        change.current.clone_from(&update.after);
    }

    pub fn next_id(&self, store: &BlockStore, height: u64) -> Result<u32> {
        if let Some(id) = self.next_id {
            return Ok(id);
        }
        next_id_at(store, height)
    }

    pub fn place(&mut self, store: &BlockStore, height: u64, order: Order) -> Result<()> {
        ensure!(
            self.next_id(store, height)? == order.id,
            "unexpected next order ID"
        );
        self.next_id = Some(order.id.checked_add(1).context("order ID overflow")?);
        self.by_id.insert(
            order.id,
            OrderChange {
                original: None,
                current: Some(order),
            },
        );
        Ok(())
    }

    pub fn current(&self, store: &BlockStore, height: u64, id: u32) -> Result<Option<Order>> {
        if let Some(change) = self.by_id.get(&id) {
            // Go resolves a cancel by a committed order ID, then checks the
            // delivery state's remaining volumes.
            return Ok(change.original.as_ref().and(change.current.clone()));
        }
        Order::find(store, height, id)
    }

    pub fn cancel(&mut self, store: &BlockStore, height: u64, id: u32) -> Result<Refund> {
        let order = self
            .current(store, height, id)?
            .context("order disappeared before cancel")?;
        let refund = order.refund();
        self.update(&Update {
            before: order,
            after: None,
        });
        Ok(refund)
    }

    pub fn write_next_id(&self, ops: &mut Vec<Mutation>) {
        if let Some(id) = self.next_id {
            ops.push(set(b"sn".to_vec(), alloy_rlp::encode(id)));
        }
    }

    pub fn expire(
        &mut self,
        store: &BlockStore,
        height: u64,
        expire_through: u64,
    ) -> Result<Vec<Refund>> {
        let mut refunds = Vec::new();
        let mut next = 0;
        while let Some(key) = store.first_key_at_or_after(height, &order_key(next))? {
            if key.first() != Some(&b'l') {
                break;
            }
            let id = u32::from_be_bytes(key[1..].try_into()?);
            if id == u32::MAX {
                break;
            }
            next = id + 1;
            let committed = Order::load(store, height, id)?;
            if committed.height > expire_through {
                break;
            }
            ensure!(
                store.value_at(height, &committed.index_key()?)?.is_some(),
                "missing index for expired order {id}"
            );
            let current = match self.by_id.get(&id) {
                Some(change) => change.current.clone(),
                None => Some(committed),
            };
            if let Some(order) = current {
                refunds.push(order.refund());
                self.update(&Update {
                    before: order,
                    after: None,
                });
            }
        }
        Ok(refunds)
    }

    pub fn write(self, ops: &mut Vec<Mutation>) -> Result<()> {
        let mut ordered = Vec::new();
        for (id, change) in self.by_id {
            if let Some(order) = change.original.as_ref().or(change.current.as_ref()) {
                ordered.push((pair_bytes(order.pair), id, change));
            }
        }
        // Go commits pairs by descending key bytes, then orders by ascending ID.
        ordered.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, id, change) in ordered {
            let key = order_key(id).to_vec();
            let old_index = change.original.as_ref().map(Order::index_key).transpose()?;
            if let Some(order) = change.current {
                let new_index = order.index_key()?;
                if let Some(old) = old_index
                    && old != new_index
                {
                    ops.push(Mutation::Remove(old));
                }
                ops.push(set(new_index, Vec::new()));
                ops.push(set(key, order.encode()));
            } else if let Some(old) = old_index {
                ops.push(Mutation::Remove(key));
                ops.push(Mutation::Remove(old));
            }
        }
        Ok(())
    }
}

pub(crate) fn next_id_at(store: &BlockStore, height: u64) -> Result<u32> {
    let Some(raw) = store.value_at(height, b"sn")? else {
        return Ok(1);
    };
    let mut encoded = raw.as_slice();
    let id = u32::decode(&mut encoded)?;
    finish(encoded)?;
    ensure!(id > 0, "invalid next order ID");
    Ok(id)
}

pub(crate) struct Refund {
    pub id: u32,
    pub owner: Address,
    pub coin: u32,
    pub value: Amount,
}

impl Refund {
    pub fn event(&self) -> OrderExpiredEvent {
        OrderExpiredEvent {
            id: u64::from(self.id),
            address: display_address(self.owner),
            coin: u64::from(self.coin),
            amount: self.value.to_string(),
        }
    }
}

pub(crate) struct Book<'a> {
    store: &'a BlockStore,
    height: u64,
    pair: [u32; 2],
    reverse: bool,
    lower: Vec<u8>,
    upper: Vec<u8>,
    cursor: Option<Vec<u8>>,
    changes: BTreeMap<u32, Option<&'a Order>>,
    staged: VecDeque<(Vec<u8>, Order)>,
    stored: Option<(Vec<u8>, u32)>,
}

impl<'a> Book<'a> {
    pub fn new(
        store: &'a BlockStore,
        height: u64,
        changes: Option<&'a Changes>,
        overlay: &'a [Update],
        pair: [u32; 2],
        reverse: bool,
    ) -> Result<Self> {
        let mut changed = BTreeMap::new();
        let relevant = |order: &Order| order.pair == pair && order.is_buy == reverse;
        if let Some(changes) = changes {
            for (&id, change) in &changes.by_id {
                if change
                    .original
                    .as_ref()
                    .or(change.current.as_ref())
                    .is_some_and(relevant)
                {
                    changed.insert(id, change.current.as_ref());
                }
            }
        }
        for update in overlay.iter().filter(|update| relevant(&update.before)) {
            changed.insert(update.before.id, update.after.as_ref());
        }
        let mut staged = changed
            .values()
            .filter_map(|order| *order)
            .map(|order| Ok((order.index_key()?, order.clone())))
            .collect::<Result<Vec<_>>>()?;
        staged.sort_unstable_by(|left, right| {
            if reverse {
                left.0.cmp(&right.0)
            } else {
                right.0.cmp(&left.0)
            }
        });
        let prefix = [b"so".as_slice(), &pair_bytes(pair), &[u8::from(!reverse)]].concat();
        let lower = [&prefix, &[0][..]].concat();
        let upper = [&prefix, &[255; 5][..]].concat();
        let cursor = if reverse {
            lower.clone()
        } else {
            upper.clone()
        };
        Ok(Self {
            store,
            height,
            pair,
            reverse,
            lower,
            upper,
            cursor: Some(cursor),
            changes: changed,
            staged: staged.into(),
            stored: None,
        })
    }

    fn seek(&mut self) -> Result<Option<(Vec<u8>, u32)>> {
        while let Some(bound) = self.cursor.take() {
            let key = if self.reverse {
                self.store.first_key_at_or_after(self.height, &bound)?
            } else {
                self.store.last_key_before(self.height, &bound)?
            };
            let Some(key) = key.filter(|key| self.lower <= *key && *key < self.upper) else {
                return Ok(None);
            };
            ensure!(key.len() == 33, "invalid swap order index key");
            let encoded = u32::from_be_bytes(key[29..].try_into()?);
            let id = if self.reverse {
                encoded
            } else {
                u32::MAX - encoded
            };
            let mut next = key.clone();
            if self.reverse {
                next.push(0);
            }
            self.cursor = Some(next);
            if !self.changes.contains_key(&id) {
                return Ok(Some((key, id)));
            }
        }
        Ok(None)
    }

    pub fn next(&mut self) -> Result<Option<Order>> {
        if self.stored.is_none() {
            self.stored = self.seek()?;
        }
        if let Some((key, _)) = self.staged.front()
            && self.stored.as_ref().is_none_or(|(stored, _)| {
                if self.reverse {
                    key < stored
                } else {
                    stored < key
                }
            })
        {
            return Ok(self.staged.pop_front().map(|(_, order)| order));
        }
        let Some((_, id)) = self.stored.take() else {
            return Ok(None);
        };
        let order = Order::load(self.store, self.height, id)?;
        ensure!(
            order.is_buy == self.reverse && order.pair == self.pair,
            "order differs from its index"
        );
        Ok(Some(order))
    }
}

fn order_key(id: u32) -> [u8; 5] {
    let mut key = [b'l'; 5];
    key[1..].copy_from_slice(&id.to_be_bytes());
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Account, set};
    use serde::Deserialize;
    use tempfile::TempDir;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        before: String,
        after: String,
        seed: Vec<Entry>,
        expire_through: u64,
        refund_count: usize,
        events: Vec<serde_json::Value>,
    }

    #[derive(Deserialize)]
    struct Entry {
        key: String,
        value: String,
    }

    #[test]
    fn refunds_and_removes_the_same_orders_as_go_in_pair_order() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../tests/fixtures/go-order-expiry.json")).unwrap();
        let directory = TempDir::new().unwrap();
        let mut store = BlockStore::open(directory.path()).unwrap();
        let seed = fixture.seed.into_iter().map(|entry| {
            set(
                hex::decode(entry.key).unwrap(),
                hex::decode(entry.value).unwrap(),
            )
        });
        let first = store.initialize(1, seed, b"").unwrap();
        assert_eq!(hex::encode(first.app_hash), fixture.before);

        let mut orders = Changes::default();
        let refunds = orders.expire(&store, 1, fixture.expire_through).unwrap();
        assert_eq!(refunds.len(), fixture.refund_count);
        let events = refunds
            .iter()
            .map(|refund| {
                minter_api::event_struct(minter_api::pack(refund.event()), &Default::default())
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let expected = fixture
            .events
            .into_iter()
            .map(|event| minter_api::protobuf_struct(event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events, expected);
        let mut accounts = BTreeMap::new();
        for refund in refunds {
            let account = match accounts.entry(refund.owner) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(Account::load(&store, 1, refund.owner).unwrap())
                }
            };
            account
                .add_balance(&store, 1, refund.coin, &refund.value)
                .unwrap();
        }
        let mut mutations = Vec::new();
        for account in accounts.into_values().rev() {
            account.write(&mut mutations);
        }
        orders.write(&mut mutations).unwrap();
        let head = store.commit(2, mutations, b"").unwrap();
        assert_eq!(hex::encode(head.app_hash), fixture.after);
        for id in 1..=3 {
            assert!(store.value_at(1, &order_key(id)).unwrap().is_some());
            assert!(store.value_at(2, &order_key(id)).unwrap().is_none());
        }
        assert!(store.value_at(2, &order_key(4)).unwrap().is_some());
    }
}
