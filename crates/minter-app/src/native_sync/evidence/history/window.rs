use super::*;

fn select_floor(
    head: u64,
    initial: u64,
    now: Time,
    params: &Params,
    mut time_at: impl FnMut(u64) -> Result<Time>,
) -> Result<Option<u64>> {
    ensure!(initial > 0 && initial <= head, "invalid history origin");
    if head == initial {
        return Ok(None);
    }
    for height in (initial..head).rev() {
        let time = time_at(height)?;
        if height == initial || super::super::expired(head, now, height, time, params) {
            return Ok(Some(height));
        }
    }
    unreachable!("positive genesis is always a boundary")
}

impl NativeSync {
    pub fn history_context_requirement(
        &self,
    ) -> Result<Option<crate::native_sync::HistoryContext>> {
        Ok(self.cursor()?.history_context.clone())
    }
    pub fn history_context_ready(&self) -> Result<bool> {
        let Some(required) = &self.cursor()?.history_context else {
            return Ok(true);
        };
        Ok(self.app.store.native_context_verified(&required.sha256)?
            && self
                .history_context_floor()?
                .zip(self.app.store.native_block_base()?)
                .is_some_and(|(floor, base)| floor <= base))
    }

    pub(in crate::native_sync) fn ensure_history_context(&self) -> Result<()> {
        ensure!(
            self.history_context_ready()?,
            "trusted snapshot history is incomplete; repeat checkpoint startup or import its required context"
        );
        Ok(())
    }

    pub(in crate::native_sync) fn required_history_floor(&self) -> Result<Option<u64>> {
        let cursor = self.cursor()?;
        let initial = self
            .app
            .chain
            .as_ref()
            .context("missing chain")?
            .initial_height;
        let now = cursor
            .previous
            .as_ref()
            .context("missing snapshot header")?
            .time;
        select_floor(cursor.height, initial, now, &cursor.params.evidence, |h| {
            self.app
                .store
                .maintenance_control()
                .export_progress(minter_state::ExportStage::HistoryScan, cursor.height - h)?;
            Ok(self.history_header(h)?.0.time)
        })
    }

    pub(super) fn finish_required_context(
        &self,
        report: &HistoryContextReport,
        bytes: u64,
        hash: [u8; 32],
    ) -> Result<()> {
        let cursor = self.cursor()?;
        let Some(required) = &cursor.history_context else {
            return Ok(());
        };
        if self.history_context_ready()? {
            return Ok(());
        }
        let anchor = self
            .app
            .store
            .checkpoint()
            .context("missing snapshot anchor")?
            .height;
        ensure!(
            report.anchor == anchor && cursor.height == anchor && report.from == required.from,
            "history context range differs from the required snapshot window"
        );
        ensure!(
            bytes == required.bytes && hash == required.sha256,
            "required context size or SHA-256 differs"
        );
        let initial = self
            .app
            .chain
            .as_ref()
            .context("missing chain")?
            .initial_height;
        let oldest = self.history_header(required.from)?.0;
        let now = cursor
            .previous
            .as_ref()
            .context("missing snapshot header")?
            .time;
        ensure!(
            required.from == initial
                || super::super::expired(
                    anchor,
                    now,
                    required.from,
                    oldest.time,
                    &cursor.params.evidence
                ),
            "snapshot context does not cover both age limits"
        );
        self.app
            .store
            .publish_verified_native_context(&required.sha256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn includes_the_boundary_for_both_age_limits_or_genesis() -> Result<()> {
        let now = Time::unix_epoch()
            .checked_add(Duration::from_secs(1000))
            .unwrap();
        let mut params = Params {
            max_age_num_blocks: 10,
            max_age_duration: tendermint::evidence::Duration(Duration::from_secs(60)),
            max_bytes: 1024,
        };
        let time_at =
            |h| -> Result<Time> { Ok(now.checked_sub(Duration::from_secs(200 - h)).unwrap()) };
        assert_eq!(select_floor(200, 1, now, &params, time_at)?, Some(139));
        params.max_age_num_blocks = 100;
        assert_eq!(select_floor(200, 1, now, &params, time_at)?, Some(99));
        assert_eq!(select_floor(200, 180, now, &params, time_at)?, Some(180));
        assert_eq!(select_floor(200, 200, now, &params, time_at)?, None);
        assert!(
            select_floor(200, 199, now, &params, |_| anyhow::bail!(
                "missing genesis header"
            ))
            .is_err()
        );
        assert!(
            select_floor(200, 1, now, &params, |h| {
                ensure!(h >= 190, "missing context");
                time_at(h)
            })
            .is_err()
        );
        Ok(())
    }
}
