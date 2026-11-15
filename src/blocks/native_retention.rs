use super::*;
use context::{CONTEXT_PREFIX, SET_PREFIX};

pub(super) const KEEP_KEY: &[u8] = b"\0minter/native-block-keep-v1";
pub(super) const BASE_KEY: &[u8] = b"\0minter/native-block-base-v1";
const SNAPSHOT_ANCHOR_KEY: &[u8] = b"\0minter/served-snapshot-anchor-v1";
const EXPORTED_ANCHOR_KEY: &[u8] = b"\0minter/exported-snapshot-anchor-v1";

pub struct NativeContextWrite<'a> {
    pub record: &'a [u8],
    pub validator_hash: &'a Hash,
    pub validators: &'a [u8],
}

impl BlockStore {
    pub fn known_native_snapshot(&self, snapshot: &Checkpoint) -> Result<bool> {
        for key in [SNAPSHOT_ANCHOR_KEY, EXPORTED_ANCHOR_KEY] {
            if let Some(raw) = self.db.get(key)? {
                let saved: Checkpoint =
                    serde_json::from_slice(&raw).context("decode snapshot anchor")?;
                if saved == *snapshot {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn remember_served_snapshot(&self, snapshot: &Checkpoint) -> Result<()> {
        self.remember_snapshot(SNAPSHOT_ANCHOR_KEY, snapshot)
    }

    pub fn remember_exported_snapshot(&self, snapshot: &Checkpoint) -> Result<()> {
        self.remember_snapshot(EXPORTED_ANCHOR_KEY, snapshot)
    }

    /// The application verifies the root and BlockID before replacing either bounded slot.
    fn remember_snapshot(&self, key: &[u8], snapshot: &Checkpoint) -> Result<()> {
        validate_checkpoint(snapshot)?;
        ensure!(
            self.head.is_some_and(|head| snapshot.height <= head.height),
            "snapshot anchor exceeds the head"
        );
        let mut batch = WriteBatch::default();
        batch.put(key, serde_json::to_vec(snapshot)?);
        write_sync(&self.db, batch)
    }
    pub fn keep_native_blocks(&self) -> Result<u64> {
        Ok(read_u64(&*self.db, KEEP_KEY)?.unwrap_or_default())
    }

    pub fn set_keep_native_blocks(&mut self, keep: u64) -> Result<()> {
        validate_keep(keep)?;
        self.maintenance_control.check()?;
        let mut batch = WriteBatch::default();
        batch.put(KEEP_KEY, keep.to_be_bytes());
        write_sync(&self.db, batch)
    }

    /// The application decides expiry and supplies the exact retained successor's set.
    /// One durable step preserves a contiguous history while moving the served base.
    pub fn retire_native_block(
        &mut self,
        height: u64,
        expected: &[u8],
        next_validators: &[u8],
        compact: Option<NativeContextWrite<'_>>,
        included: &[Hash],
    ) -> Result<()> {
        ensure!(
            self.native_block_base()? == Some(height),
            "retire only the native archive base"
        );
        ensure!(
            self.head.is_some_and(|head| height + 1 < head.height),
            "retain at least two native blocks"
        );
        ensure!(
            self.native_block_at(height)?.as_deref() == Some(expected),
            "retired native record changed"
        );
        ensure!(
            !next_validators.is_empty() && self.native_block_at(height + 1)?.is_some(),
            "missing retained successor"
        );
        let mut batch = WriteBatch::default();
        if let Some(compact) = compact {
            self.stage_native_context(
                &mut batch,
                height,
                compact.record,
                compact.validator_hash,
                compact.validators,
            )?;
        } else {
            self.stage_drop_history(&mut batch, height, included)?;
        }
        batch.delete(native_block_key(height));
        batch.put(BASE_KEY, (height + 1).to_be_bytes());
        batch.delete(prefixed(NATIVE_VALIDATORS_PREFIX, &height.to_be_bytes()));
        batch.put(
            prefixed(NATIVE_VALIDATORS_PREFIX, &(height + 1).to_be_bytes()),
            next_validators,
        );
        write_sync(&self.db, batch)
    }

    /// Only call for an expired compact record below the first retained full block.
    pub fn prune_native_context(&mut self, height: u64, included: &[Hash]) -> Result<()> {
        ensure!(
            self.evidence_index_start()? == Some(height),
            "prune only the context floor"
        );
        ensure!(
            self.native_block_base()?.is_some_and(|base| height < base),
            "cannot prune full history as compact"
        );
        ensure!(
            self.native_context_at(height)?.is_some(),
            "missing compact history at the floor"
        );
        let mut batch = WriteBatch::default();
        batch.delete(prefixed(CONTEXT_PREFIX, &height.to_be_bytes()));
        self.stage_drop_history(&mut batch, height, included)?;
        write_sync(&self.db, batch)
    }

    fn stage_drop_history(
        &self,
        batch: &mut WriteBatch,
        height: u64,
        included: &[Hash],
    ) -> Result<()> {
        let floor = self
            .evidence_index_start()?
            .context("missing history floor")?;
        ensure!(floor == height, "drop only the oldest retained history");
        batch.put(EVIDENCE_INDEX_START, (height + 1).to_be_bytes());
        for hash in included {
            ensure!(
                self.db
                    .get(prefixed(COMMITTED_EVIDENCE_PREFIX, hash))?
                    .as_deref()
                    == Some(height.to_be_bytes().as_slice()),
                "included marker differs from retired height"
            );
            batch.delete(prefixed(COMMITTED_EVIDENCE_PREFIX, hash));
        }
        Ok(())
    }

    pub fn visit_native_context(
        &self,
        mut visit: impl FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut bounds = ReadOptions::default();
        bounds.set_iterate_range(rocksdb::PrefixRange(CONTEXT_PREFIX));
        let mut records = self.db.raw_iterator_opt(bounds);
        records.seek_to_first();
        while let (Some(key), Some(value)) = (records.key(), records.value()) {
            let height = u64::from_be_bytes(
                key.strip_prefix(CONTEXT_PREFIX)
                    .context("context prefix")?
                    .try_into()?,
            );
            visit(height, value)?;
            records.next();
        }
        records.status().context("iterate compact history")
    }

    /// Call under the writer lock with the hashes of every retained compact header.
    pub fn retain_context_validator_sets(&mut self, live: &HashSet<Hash>) -> Result<()> {
        let control = self.maintenance_control.clone();
        control.maintenance_progress(crate::MaintenanceStage::MembershipSets, 0)?;
        let mut bounds = ReadOptions::default();
        bounds.set_iterate_range(rocksdb::PrefixRange(SET_PREFIX));
        let mut sets = self.db.raw_iterator_opt(bounds);
        sets.seek_to_first();
        let mut batch = WriteBatch::default();
        let mut processed = 0;
        while let Some(key) = sets.key() {
            control.check()?;
            let hash: Hash = key
                .strip_prefix(SET_PREFIX)
                .context("membership prefix")?
                .try_into()?;
            if !live.contains(&hash) {
                batch.delete(key);
                if batch.len() >= 1000 {
                    write_sync(&self.db, std::mem::take(&mut batch))?;
                }
            }
            processed += 1;
            control.maintenance_progress(crate::MaintenanceStage::MembershipSets, processed)?;
            sets.next();
        }
        sets.status()?;
        if !batch.is_empty() {
            control.check()?;
            write_sync(&self.db, batch)?;
        }
        Ok(())
    }
}

pub(super) fn validate_keep(keep: u64) -> Result<()> {
    ensure!(
        keep == 0 || keep >= 2,
        "native retention needs zero or at least two blocks"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_membership_collection_preserves_live_sets_and_can_resume() -> Result<()> {
        use crate::{MaintenanceStage, OperationCancelled};
        fn hash(i: u32) -> Hash {
            let mut hash = [0; 32];
            hash[28..].copy_from_slice(&i.to_be_bytes());
            hash
        }
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        let mut store = BlockStore::open(&path)?;
        store.initialize(99, [], b"state")?;
        let head = store.head();
        let mut batch = WriteBatch::default();
        for i in 0..2005 {
            batch.put(prefixed(SET_PREFIX, &hash(i)), b"membership");
        }
        write_sync(&store.db, batch)?;
        drop(store);
        let stop = OperationControl::default();
        let request = stop.clone();
        let control = stop.with_maintenance_progress(move |phase, done| {
            if phase == MaintenanceStage::MembershipSets && done >= 1001 {
                request.cancel();
            }
        });
        let mut store = BlockStore::open_with_control(&path, &control)?;
        let live = HashSet::from([hash(0), hash(2004)]);
        let error = store
            .retain_context_validator_sets(&live)
            .expect_err("membership cleanup ignored cancellation");
        assert!(OperationCancelled::caused(&error), "{error:#}");
        assert_eq!(store.head(), head);
        assert!(store.native_context_validators(&hash(0))?.is_some());
        assert!(store.native_context_validators(&hash(2004))?.is_some());
        assert!(store.native_context_validators(&hash(1))?.is_none());
        assert!(store.native_context_validators(&hash(1001))?.is_some());
        drop(store);
        let mut store = BlockStore::open(&path)?;
        store.retain_context_validator_sets(&live)?;
        assert_eq!(store.head(), head);
        assert!(store.native_context_validators(&hash(0))?.is_some());
        assert!(store.native_context_validators(&hash(2004))?.is_some());
        assert!(store.native_context_validators(&hash(1001))?.is_none());
        Ok(())
    }

    #[test]
    fn retiring_a_large_prefix_does_not_rescan_its_deleted_keys() -> Result<()> {
        use rocksdb::perf::{PerfContext, PerfMetric, PerfStatsLevel, set_perf_stats};

        let directory = tempfile::tempdir()?;
        let mut store = BlockStore::open(directory.path())?;
        store.initialize(1101, [], b"state")?;
        let mut batch = WriteBatch::default();
        for height in 100u64..=1101 {
            batch.put(native_block_key(height), b"record");
        }
        batch.put(EVIDENCE_INDEX_START, 100u64.to_be_bytes());
        write_sync(&store.db, batch)?;
        store.db.flush()?;
        let db = store.db.clone();
        let pinned = db.snapshot();
        set_perf_stats(PerfStatsLevel::EnableCount);
        let mut perf = PerfContext::default();
        perf.reset();
        for height in 100..1100 {
            store.retire_native_block(height, b"record", b"next", None, &[])?;
        }
        let skipped = perf.metric(PerfMetric::InternalKeySkippedCount)
            + perf.metric(PerfMetric::InternalDeleteSkippedCount);
        set_perf_stats(PerfStatsLevel::Disable);
        eprintln!("Internal keys skipped while retiring 1000 bodies: {skipped}");
        assert!(
            skipped < 10_000,
            "repeatedly scanned the deleted archive prefix: {skipped}"
        );
        assert_eq!(store.native_block_base()?, Some(1100));
        assert!(pinned.get(native_block_key(100))?.is_some());
        Ok(())
    }

    #[test]
    fn remembers_exported_and_served_snapshots_in_separate_bounded_slots() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = BlockStore::open(dir.path())?;
        let head = store.initialize(100, [], b"state")?;
        let served = Checkpoint {
            format: 1,
            network: "minter-mainnet-5".to_owned(),
            height: 100,
            block_hash: [1; 32],
            app_hash: head.app_hash,
            snapshot_sha256: [2; 32],
            bytes: 1,
            nodes: 1,
        };
        let exported = Checkpoint {
            snapshot_sha256: [3; 32],
            ..served.clone()
        };
        store.remember_served_snapshot(&served)?;
        store.remember_exported_snapshot(&exported)?;
        drop(store);
        let store = BlockStore::open_existing(dir.path())?;
        assert!(store.known_native_snapshot(&served)?);
        assert!(store.known_native_snapshot(&exported)?);
        assert!(!store.known_native_snapshot(&Checkpoint {
            app_hash: [9; 32],
            ..served.clone()
        })?);
        let newer = Checkpoint {
            snapshot_sha256: [4; 32],
            ..exported.clone()
        };
        store.remember_exported_snapshot(&newer)?;
        assert!(store.known_native_snapshot(&served)?);
        assert!(!store.known_native_snapshot(&exported)?);
        assert!(store.known_native_snapshot(&newer)?);
        Ok(())
    }

    #[test]
    fn retires_bodies_without_losing_live_history_or_changing_execution() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        let mut store = BlockStore::open(&path)?;
        store.initialize(99, [], b"genesis")?;
        for height in 100..=105 {
            store.commit_native_block(
                height,
                [],
                |_| Ok(b"cursor".to_vec()),
                b"events",
                NativeBlockWrite {
                    expected_root: None,
                    record: &[height as u8],
                    validators: Some(b"set"),
                    committed_evidence: &[[height as u8; 32]],
                    pending_evidence: Some(b"pending"),
                },
            )?;
        }
        let head = store.head();
        assert!(store.set_keep_native_blocks(1).is_err());
        store.set_keep_native_blocks(3)?;
        assert_eq!(store.keep_native_blocks()?, 3);
        assert!(
            store
                .retire_native_block(101, &[101], b"next", None, &[])
                .is_err()
        );
        assert!(
            store
                .retire_native_block(100, b"wrong", b"next", None, &[])
                .is_err()
        );
        store.retire_native_block(100, &[100], b"next", None, &[[100; 32]])?;
        assert_eq!(store.evidence_index_start()?, Some(101));
        assert!(!store.evidence_committed(&[100; 32])?);
        for height in 101..=102 {
            store.retire_native_block(
                height,
                &[height as u8],
                b"next",
                Some(NativeContextWrite {
                    record: &[height as u8],
                    validator_hash: &[height as u8; 32],
                    validators: b"membership",
                }),
                &[[height as u8; 32]],
            )?;
        }
        drop(store);
        let mut store = BlockStore::open_existing(&path)?;
        assert_eq!(store.keep_native_blocks()?, 3);
        assert_eq!(store.head(), head);
        assert_eq!(store.metadata()?.as_deref(), Some(b"cursor".as_slice()));
        assert_eq!(
            store.pending_evidence()?.as_deref(),
            Some(b"pending".as_slice())
        );
        assert_eq!(
            store.reader().unwrap().native_block_range()?,
            Some((103, 105))
        );
        assert_eq!(
            store.native_validators_at(103)?.as_deref(),
            Some(b"next".as_slice())
        );
        assert!(store.native_validators_at(102)?.is_none());
        assert_eq!(store.evidence_index_start()?, Some(101));
        assert_eq!(
            store.native_context_at(101)?.as_deref(),
            Some([101].as_slice())
        );
        assert!(store.evidence_committed(&[101; 32])?);
        assert!(
            store
                .retire_native_block(103, &[103], b"next", None, &[])
                .is_err()
        );
        assert!(store.prune_native_context(102, &[]).is_err());
        store.prune_native_context(101, &[[101; 32]])?;
        store.retain_context_validator_sets(&HashSet::from([[102; 32]]))?;
        drop(store);
        let store = BlockStore::open_existing(&path)?;
        assert_eq!(store.evidence_index_start()?, Some(102));
        assert!(store.native_context_at(101)?.is_none());
        assert!(store.native_context_validators(&[101; 32])?.is_none());
        assert!(store.native_context_validators(&[102; 32])?.is_some());
        assert!(!store.evidence_committed(&[101; 32])?);
        assert!(store.evidence_committed(&[102; 32])?);
        assert_eq!(store.head(), head);
        Ok(())
    }
}
