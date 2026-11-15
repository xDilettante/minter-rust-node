use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionPolicy {
    pub keep_iavl_versions: u64,
    pub keep_event_blocks: u64,
    pub keep_native_blocks: u64,
}

impl RetentionPolicy {
    pub const ARCHIVE: Self = Self {
        keep_iavl_versions: 0,
        keep_event_blocks: 0,
        keep_native_blocks: 0,
    };
    pub const FULL: Self = Self {
        keep_iavl_versions: 120,
        keep_event_blocks: 100_000,
        keep_native_blocks: 100_000,
    };

    pub fn profile_name(self) -> &'static str {
        if self == Self::FULL {
            "full"
        } else if self == Self::ARCHIVE {
            "archive"
        } else {
            "custom"
        }
    }
}

impl BlockStore {
    pub fn retention_policy(&self) -> Result<RetentionPolicy> {
        Ok(RetentionPolicy {
            keep_iavl_versions: self.keep_iavl_versions(),
            keep_event_blocks: self.keep_event_blocks(),
            keep_native_blocks: self.keep_native_blocks()?,
        })
    }

    /// Configure an initialized node. Readers see all limits and logical floors
    /// in one durable batch; physical orphan cleanup can resume after publication.
    pub fn configure_retention(&mut self, policy: RetentionPolicy) -> Result<()> {
        native_retention::validate_keep(policy.keep_native_blocks)?;
        let head = self
            .head
            .context("initialize the node before selecting a storage profile")?;
        let version = self.version(head.height);
        self.retention.configure_staged(
            &self.db,
            Some(version),
            policy.keep_iavl_versions,
            &self.maintenance_control,
            |batch| {
                Self::stage_event_retention(batch, Some(head), policy.keep_event_blocks);
                batch.put(
                    native_retention::KEEP_KEY,
                    policy.keep_native_blocks.to_be_bytes(),
                );
            },
        )?;
        self.keep_event_blocks = policy.keep_event_blocks;
        self.retention.sweep(&self.db, &self.maintenance_control)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_writers_do_not_publish_new_storage_limits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let control = OperationControl::default();
        let mut store = BlockStore::open_with_control(directory.path(), &control)?;
        let head = store.initialize(99, [], b"state")?;
        control.cancel();
        for result in [
            store.set_keep_event_blocks(5),
            store.set_keep_native_blocks(2),
            store.set_keep_iavl_versions(1),
            store.configure_retention(RetentionPolicy::FULL),
        ] {
            let error = result.expect_err("cancelled storage configuration was published");
            assert!(crate::OperationCancelled::caused(&error));
        }
        assert_eq!(store.retention_policy()?, RetentionPolicy::ARCHIVE);
        assert_eq!(store.head(), Some(head));
        Ok(())
    }

    #[test]
    fn publishes_all_limits_with_root_and_event_floors_and_preserves_pinned_views() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        let mut store = BlockStore::open(&path)?;
        assert!(store.configure_retention(RetentionPolicy::FULL).is_err());
        store.initialize(99, [], b"genesis")?;
        for height in 100..=105 {
            store.commit_with_events(
                height,
                [Mutation::Set {
                    key: b"balance".to_vec(),
                    value: height.to_be_bytes().to_vec(),
                }],
                b"execution",
                &height.to_be_bytes(),
            )?;
        }
        let head = store.head();
        let reader = store.reader().unwrap();
        let pinned = reader.at(100)?;
        assert!(
            store
                .configure_retention(RetentionPolicy {
                    keep_iavl_versions: 1,
                    keep_event_blocks: 1,
                    keep_native_blocks: 1
                })
                .is_err()
        );
        assert_eq!(store.retention_policy()?, RetentionPolicy::ARCHIVE);
        assert!(reader.at(100).is_ok());
        assert_eq!(reader.event_block_range()?, Some((100, 105)));
        let policy = RetentionPolicy {
            keep_iavl_versions: 2,
            keep_event_blocks: 3,
            keep_native_blocks: 4,
        };
        store.configure_retention(policy)?;
        assert_eq!(store.retention_policy()?, policy);
        assert_eq!(store.head(), head);
        assert_eq!(store.earliest_height(), Some(104));
        assert!(reader.at(103).is_err());
        assert_eq!(reader.event_block_range()?, Some((103, 105)));
        assert_eq!(pinned.value(b"balance")?.unwrap(), 100u64.to_be_bytes());
        assert_eq!(pinned.block_events(100)?.unwrap(), 100u64.to_be_bytes());
        drop(pinned);
        drop(reader);
        drop(store);
        let mut store = BlockStore::open_existing(&path)?;
        assert_eq!(store.retention_policy()?, policy);
        store.configure_retention(RetentionPolicy::ARCHIVE)?;
        for height in 106..=107 {
            store.commit_with_events(height, [], b"execution", b"new-event")?;
        }
        assert_eq!(store.retention_policy()?, RetentionPolicy::ARCHIVE);
        assert_eq!(store.earliest_height(), Some(104));
        assert_eq!(
            store.reader().unwrap().event_block_range()?,
            Some((103, 107))
        );
        Ok(())
    }
}
