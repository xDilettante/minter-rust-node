use super::*;
use minter_state::{MaintenanceStage, NativeContextWrite, OperationCancelled};
use std::collections::HashSet;

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeRetentionReport {
    pub full_blocks_removed: u64,
    pub compact_records_removed: u64,
    pub archive_from: Option<u64>,
    pub context_from: Option<u64>,
    pub keep_native_blocks: u64,
}

fn advance_floor(
    head: u64,
    from: u64,
    now: Time,
    params: &Params,
    mut time_at: impl FnMut(u64) -> Result<Time>,
) -> Result<u64> {
    ensure!(from > 0 && from <= head, "invalid retained history floor");
    let mut floor = from;
    let mut end = head;
    // Verified header times are monotone. Expiry can only change from true to
    // false as height increases, so a large full archive needs logarithmic reads.
    while floor + 1 < end {
        let middle = floor + (end - floor) / 2;
        if super::super::expired(head, now, middle, time_at(middle)?, params) {
            floor = middle;
        } else {
            end = middle;
        }
    }
    Ok(floor)
}

impl NativeSync {
    pub fn keep_native_blocks(&self) -> Result<u64> {
        self.app.store.keep_native_blocks()
    }

    /// Persist the policy; call maintenance after preparing any served snapshot.
    pub fn set_keep_native_blocks(&mut self, keep: u64) -> Result<()> {
        self.app.store.set_keep_native_blocks(keep)
    }

    pub fn maintain_native_history(&mut self) -> Result<NativeRetentionReport> {
        self.retain_native_history(true)
    }

    pub(in crate::native_sync) fn maintain_history_after_commit(&mut self) {
        let result = self
            .head()
            .and_then(|head| self.retain_native_history(head.height.is_multiple_of(1000)));
        if let Err(error) = result
            && !OperationCancelled::caused(&error)
        {
            // The application block is already durable. Cleanup resumes from its
            // atomic floor on the next commit or startup; do not report a lost block.
            eprintln!("Native history cleanup will retry: {error:#}");
        }
    }

    fn retain_native_history(&mut self, collect_sets: bool) -> Result<NativeRetentionReport> {
        let control = self.app.store.maintenance_control().clone();
        control.check()?;
        let keep = self.keep_native_blocks()?;
        let mut report = NativeRetentionReport {
            keep_native_blocks: keep,
            archive_from: self.app.store.native_block_base()?,
            context_from: self.history_context_floor()?,
            ..Default::default()
        };
        if keep == 0 || report.archive_from.is_none() {
            return Ok(report);
        }
        self.ensure_history_context()?;
        let cursor = self.cursor()?;
        let now = cursor
            .previous
            .as_ref()
            .context("missing history head")?
            .time;
        let head = cursor.height;
        let floor = advance_floor(
            head,
            report.context_from.context("missing history floor")?,
            now,
            &cursor.params.evidence,
            |h| {
                control.check()?;
                Ok(self.history_header(h)?.0.time)
            },
        )?;
        let mut base = report.archive_from.unwrap();
        let mut from = report.context_from.unwrap();
        control.maintenance_progress(MaintenanceStage::CompactRecords, 0)?;
        while from < floor.min(base) {
            control.check()?;
            let record = Record::decode(
                self.app
                    .store
                    .native_context_at(from)?
                    .context("missing compact floor")?
                    .as_slice(),
            )?;
            self.app
                .store
                .prune_native_context(from, &included(&record)?)?;
            report.compact_records_removed += 1;
            from += 1;
            control.maintenance_progress(
                MaintenanceStage::CompactRecords,
                report.compact_records_removed,
            )?;
        }
        let target = head.saturating_sub(keep - 1).max(base);
        control.maintenance_progress(MaintenanceStage::NativeBlocks, 0)?;
        while base < target {
            control.check()?;
            let raw = self
                .app
                .store
                .native_block_at(base)?
                .context("missing retired block")?;
            let block = ReceivedBlock::decode(BlockRecord::decode(raw.as_slice())?.block_response)?;
            let hashes = block
                .evidence()
                .evidence
                .iter()
                .map(wire::hash)
                .collect::<Result<Vec<_>>>()?;
            let next_header = self.history_header(base + 1)?.0;
            let next_set = proposer::to_wire(&self.history_validators(base + 1, &next_header)?)
                .encode_to_vec();
            let compact = if base >= floor {
                let mut record = self.history_record(base)?;
                let validators = std::mem::take(&mut record.validators);
                Some((
                    record.encode_to_vec(),
                    block.block().header.validators_hash.as_bytes().try_into()?,
                    validators,
                ))
            } else {
                None
            };
            self.app.store.retire_native_block(
                base,
                &raw,
                &next_set,
                compact
                    .as_ref()
                    .map(|(record, hash, validators)| NativeContextWrite {
                        record,
                        validator_hash: hash,
                        validators,
                    }),
                &hashes,
            )?;
            report.full_blocks_removed += 1;
            base += 1;
            control
                .maintenance_progress(MaintenanceStage::NativeBlocks, report.full_blocks_removed)?;
            if report.full_blocks_removed.is_multiple_of(1000) {
                eprintln!(
                    "Native history cleanup retired {} full blocks; next {base}",
                    report.full_blocks_removed
                );
            }
        }
        if collect_sets {
            let mut live = HashSet::new();
            let mut processed = 0;
            control.maintenance_progress(MaintenanceStage::MembershipRecords, 0)?;
            self.app.store.visit_native_context(|height, raw| {
                control.check()?;
                let record = Record::decode(raw)?;
                let (header, _) = decode_signed(&record.signed_header)?;
                ensure!(
                    header.height.value() == height,
                    "compact key differs from its header"
                );
                let hash = header.validators_hash.as_bytes().try_into()?;
                ensure!(
                    self.app.store.native_context_validators(&hash)?.is_some(),
                    "missing retained membership"
                );
                live.insert(hash);
                processed += 1;
                control.maintenance_progress(MaintenanceStage::MembershipRecords, processed)?;
                Ok(())
            })?;
            control.check()?;
            self.app.store.retain_context_validator_sets(&live)?;
        }
        report.archive_from = self.app.store.native_block_base()?;
        report.context_from = self.history_context_floor()?;
        Ok(report)
    }
}

fn included(record: &Record) -> Result<Vec<[u8; 32]>> {
    types::EvidenceList::decode(record.evidence.as_slice())?
        .evidence
        .iter()
        .map(wire::hash)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn finds_the_expiry_boundary_without_rescanning_a_large_archive() -> Result<()> {
        let now = Time::unix_epoch()
            .checked_add(Duration::from_secs(1_000_000))
            .unwrap();
        let params = Params {
            max_age_num_blocks: 1000,
            max_age_duration: tendermint::evidence::Duration(Duration::from_secs(1000)),
            max_bytes: 1024,
        };
        let mut reads = 0;
        let floor = advance_floor(1_000_000, 1, now, &params, |height| {
            reads += 1;
            ensure!(reads <= 32, "rescanned the expired archive prefix");
            Ok(Time::unix_epoch()
                .checked_add(Duration::from_secs(height))
                .unwrap())
        })?;
        assert_eq!(floor, 998_999);
        assert!(reads <= 20);
        Ok(())
    }

    #[test]
    fn keeps_the_last_expired_boundary_and_never_drops_available_live_history() -> Result<()> {
        let now = Time::unix_epoch()
            .checked_add(Duration::from_secs(1000))
            .unwrap();
        let mut params = Params {
            max_age_num_blocks: 10,
            max_age_duration: tendermint::evidence::Duration(Duration::from_secs(60)),
            max_bytes: 1024,
        };
        let at = |h| -> Result<Time> { Ok(now.checked_sub(Duration::from_secs(200 - h)).unwrap()) };
        assert_eq!(advance_floor(200, 1, now, &params, at)?, 139);
        assert_eq!(advance_floor(200, 139, now, &params, at)?, 139);
        assert_eq!(advance_floor(200, 150, now, &params, at)?, 150);
        params.max_age_num_blocks = 100;
        assert_eq!(advance_floor(200, 1, now, &params, at)?, 99);
        assert_eq!(advance_floor(200, 200, now, &params, at)?, 200);
        assert!(
            advance_floor(200, 199, now, &params, |_| anyhow::bail!(
                "should not read head"
            ))
            .is_ok()
        );
        assert!(advance_floor(200, 1, now, &params, |_| anyhow::bail!("missing history")).is_err());
        Ok(())
    }
}
