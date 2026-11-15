use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    ops::Bound::{Excluded, Unbounded},
    sync::Arc,
};
use tendermint::node;
use tendermint_proto::v0_34::{abci, consensus::NewRoundStep};

use super::NativeNode;

const MAX_PENDING: usize = 10_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECENT: usize = 100_000;
const GOSSIP_BATCH: usize = 32;
type TxHash = [u8; 32];

pub enum Submission {
    Accepted,
    Duplicate,
    Full,
    CatchingUp,
    Disabled,
    Rejected(Box<abci::ResponseCheckTx>),
}

#[derive(Default)]
pub(super) struct Pool {
    pub last_commit_round: i32,
    pending: BTreeMap<u64, Pending>,
    index: HashMap<TxHash, u64>,
    bytes: usize,
    sequence: u64,
    committed: HashSet<TxHash>,
    recent: VecDeque<TxHash>,
}

pub(super) struct Pending {
    pub sequence: u64,
    pub height: u64,
    pub tx: Arc<[u8]>,
    pub source: Option<node::Id>,
    gas: i64,
}

impl minter_p2p::mempool::Mempool for NativeNode {
    fn round_step(&self) -> Result<NewRoundStep> {
        let sync = self.lock()?;
        let pool = sync
            .mempool
            .as_ref()
            .context("native mempool is disabled")?;
        Ok(NewRoundStep {
            height: sync
                .head()?
                .height
                .checked_add(1)
                .context("invalid next height")?
                .try_into()?,
            round: 0,
            step: 1, // RoundStepNewHeight in Tendermint 0.34.
            seconds_since_start_time: 0,
            last_commit_round: pool.last_commit_round,
        })
    }

    fn receive(&self, source: node::Id, tx: Vec<u8>) -> Result<()> {
        self.submit(tx, Some(source))?;
        Ok(())
    }

    fn pending(
        &self,
        peer: node::Id,
        after: u64,
        peer_height: u64,
    ) -> Result<(u64, Vec<Arc<[u8]>>)> {
        let sync = self.lock()?;
        let pool = sync
            .mempool
            .as_ref()
            .context("native mempool is disabled")?;
        let mut cursor = after;
        let mut txs = Vec::new();
        for entry in pool.pending_after(after) {
            if entry.source == Some(peer) {
                cursor = entry.sequence;
                continue;
            }
            if peer_height == 0 || peer_height < entry.height.saturating_sub(1) {
                break;
            }
            txs.push(entry.tx.clone());
            cursor = entry.sequence;
        }
        Ok((cursor, txs))
    }
}

impl Pool {
    pub fn submit(
        &mut self,
        tx: Vec<u8>,
        height: u64,
        source: Option<node::Id>,
        check: impl FnOnce(&[u8], u32) -> Result<abci::ResponseCheckTx>,
    ) -> Result<Submission> {
        let hash = Sha256::digest(&tx).into();
        if self.index.contains_key(&hash) || self.committed.contains(&hash) {
            return Ok(Submission::Duplicate);
        }
        if self.index.len() >= MAX_PENDING || tx.len() > MAX_BYTES - self.bytes {
            return Ok(Submission::Full);
        }
        let response = check(&tx, min_gas_price(self.pending.len()))?;
        if response.code != 0 {
            return Ok(Submission::Rejected(Box::new(response)));
        }
        self.sequence += 1;
        self.bytes += tx.len();
        self.index.insert(hash, self.sequence);
        self.pending.insert(
            self.sequence,
            Pending {
                sequence: self.sequence,
                height,
                tx: tx.into(),
                source,
                gas: response.gas_wanted.max(0),
            },
        );
        Ok(Submission::Accepted)
    }

    pub fn pending_after(&self, sequence: u64) -> impl Iterator<Item = &Pending> {
        self.pending
            .range((Excluded(sequence), Unbounded))
            .take(GOSSIP_BATCH)
            .map(|(_, entry)| entry)
    }

    pub(super) fn proposed(&self, max_bytes: usize, max_gas: i64) -> Vec<Vec<u8>> {
        let mut bytes = 0;
        let mut gas = 0i64;
        self.pending
            .values()
            .take_while(|entry| {
                bytes += 1 + prost::length_delimiter_len(entry.tx.len()) + entry.tx.len();
                gas = gas.saturating_add(entry.gas);
                bytes <= max_bytes && (max_gas < 0 || gas <= max_gas)
            })
            .map(|entry| entry.tx.to_vec())
            .collect()
    }

    pub fn committed(&mut self, txs: &[Vec<u8>], results: &[abci::ResponseDeliverTx]) {
        for (tx, result) in txs.iter().zip(results) {
            let hash = Sha256::digest(tx).into();
            if let Some(id) = self.index.remove(&hash) {
                self.bytes -= self
                    .pending
                    .remove(&id)
                    .expect("indexed transaction")
                    .tx
                    .len();
            }
            if result.code == 0 && self.committed.insert(hash) {
                self.recent.push_back(hash);
                if self.recent.len() > MAX_RECENT {
                    self.committed
                        .remove(&self.recent.pop_front().expect("recent transaction"));
                }
            }
        }
    }
}

fn min_gas_price(size: usize) -> u32 {
    match size {
        0..=100 => 1,
        101..=500 => 2,
        501..=1000 => 5,
        1001..=5000 => 10,
        _ => 50,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_once_relays_in_order_and_evicts_on_commit() {
        let mut pool = Pool::default();
        let first = vec![1; 10];
        let second = vec![2; 16];
        let check = |_: &[u8], price| {
            assert_eq!(price, 1);
            Ok(abci::ResponseCheckTx::default())
        };
        for tx in [&first, &second] {
            assert!(matches!(
                pool.submit(tx.clone(), 10, None, check).unwrap(),
                Submission::Accepted
            ));
        }
        assert_eq!(
            pool.pending_after(0)
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        let reject_duplicate = |_: &[u8], _| panic!("checked a duplicate transaction");
        assert!(matches!(
            pool.submit(first.clone(), 10, None, reject_duplicate)
                .unwrap(),
            Submission::Duplicate
        ));
        pool.committed(
            &[first.clone(), second.clone()],
            &[
                abci::ResponseDeliverTx::default(),
                abci::ResponseDeliverTx {
                    code: 1,
                    ..Default::default()
                },
            ],
        );
        assert_eq!(pool.bytes, 0);
        assert_eq!(pool.pending_after(0).count(), 0);
        assert!(matches!(
            pool.submit(first, 11, None, reject_duplicate).unwrap(),
            Submission::Duplicate
        ));
        assert!(matches!(
            pool.submit(second, 11, None, check).unwrap(),
            Submission::Accepted
        ));
        let newest = pool.pending_after(2).next().unwrap();
        assert_eq!(
            (newest.sequence, newest.height, newest.source),
            (3, 11, None)
        );
        assert_eq!(&*newest.tx, &[2; 16]);
    }

    #[test]
    fn enforces_capacity_and_go_fee_thresholds() {
        let mut pool = Pool {
            bytes: MAX_BYTES,
            ..Pool::default()
        };
        assert!(matches!(
            pool.submit(vec![1], 10, None, |_, _| panic!("checked when full"))
                .unwrap(),
            Submission::Full
        ));
        for (size, expected) in [
            (0, 1),
            (100, 1),
            (101, 2),
            (500, 2),
            (501, 5),
            (1000, 5),
            (1001, 10),
            (5000, 10),
            (5001, 50),
            (MAX_PENDING, 50),
        ] {
            assert_eq!(min_gas_price(size), expected);
        }
    }
}
