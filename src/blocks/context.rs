use super::{BlockStore, EVIDENCE_INDEX_START, Hash, prefixed, write_sync};
use anyhow::{Context, Result, ensure};
use rocksdb::WriteBatch;

pub(super) const CONTEXT_PREFIX: &[u8] = b"\0minter/native-context-v1/";
pub(super) const SET_PREFIX: &[u8] = b"\0minter/context-validators-v1/";
const VERIFIED_PREFIX: &[u8] = b"\0minter/context-verified-v1/";

impl BlockStore {
    pub fn native_context_verified(&self, hash: &Hash) -> Result<bool> {
        Ok(self.db.get(prefixed(VERIFIED_PREFIX, hash))?.as_deref() == Some(&[1]))
    }

    /// The application calls this only after checking the complete file and age window.
    pub fn publish_verified_native_context(&self, hash: &Hash) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.put(prefixed(VERIFIED_PREFIX, hash), [1]);
        write_sync(&self.db, batch)
    }

    pub fn native_context_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .get(prefixed(CONTEXT_PREFIX, &height.to_be_bytes()))?)
    }

    pub fn native_context_validators(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(prefixed(SET_PREFIX, hash))?)
    }

    /// The caller authenticates the header, set and included hashes against its successor.
    /// Publish one contiguous older height without changing IAVL or the served archive.
    pub fn extend_native_context(
        &self,
        height: u64,
        record: &[u8],
        validator_hash: &Hash,
        validators: &[u8],
        committed: &[Hash],
    ) -> Result<()> {
        let floor = self
            .evidence_index_start()?
            .context("no verified context floor")?;
        ensure!(
            height > 0 && height.checked_add(1) == Some(floor),
            "context must immediately precede the verified floor"
        );
        let mut batch = WriteBatch::default();
        self.stage_native_context(&mut batch, height, record, validator_hash, validators)?;
        Self::stage_evidence_index(&mut batch, height, None, committed, false);
        batch.put(EVIDENCE_INDEX_START, height.to_be_bytes());
        write_sync(&self.db, batch)
    }

    pub(super) fn stage_native_context(
        &self,
        batch: &mut WriteBatch,
        height: u64,
        record: &[u8],
        validator_hash: &Hash,
        validators: &[u8],
    ) -> Result<()> {
        ensure!(
            !record.is_empty() && !validators.is_empty(),
            "empty historical context"
        );
        let key = prefixed(SET_PREFIX, validator_hash);
        if let Some(existing) = self.db.get(&key)? {
            ensure!(
                existing == validators,
                "historical validator membership changed"
            );
        }
        batch.put(prefixed(CONTEXT_PREFIX, &height.to_be_bytes()), record);
        batch.put(key, validators);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativeBlockWrite;

    #[test]
    fn publishes_only_contiguous_context_with_its_set_and_included_hashes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        let mut store = BlockStore::open(&path)?;
        store.initialize(99, [], b"genesis")?;
        let head = store.commit_native_block(
            100,
            [],
            |_| Ok(b"cursor".to_vec()),
            b"events",
            NativeBlockWrite {
                expected_root: None,
                record: b"block",
                validators: Some(b"native-set"),
                committed_evidence: &[],
                pending_evidence: None,
            },
        )?;
        assert!(
            store
                .extend_native_context(98, b"gap", &[3; 32], b"set", &[])
                .is_err()
        );
        assert_eq!(store.evidence_index_start()?, Some(100));
        store.extend_native_context(99, b"header99", &[3; 32], b"set", &[[7; 32]])?;
        drop(store);
        let store = BlockStore::open_existing(&path)?;
        assert_eq!(store.evidence_index_start()?, Some(99));
        assert_eq!(
            store.native_context_at(99)?.as_deref(),
            Some(b"header99".as_slice())
        );
        assert_eq!(
            store.native_context_validators(&[3; 32])?.as_deref(),
            Some(b"set".as_slice())
        );
        assert!(store.evidence_committed(&[7; 32])?);
        assert!(!store.native_context_verified(&[9; 32])?);
        store.publish_verified_native_context(&[9; 32])?;
        store.extend_native_context(98, b"header98", &[3; 32], b"set", &[])?;
        assert!(
            store
                .extend_native_context(97, b"header97", &[3; 32], b"different set", &[])
                .is_err()
        );
        assert_eq!(store.evidence_index_start()?, Some(98));
        assert!(store.native_context_at(97)?.is_none());
        assert_eq!(store.head(), Some(head));
        assert_eq!(
            store.reader().unwrap().native_block_range()?,
            Some((100, 100))
        );
        assert_eq!(
            store.native_validators_at(100)?.as_deref(),
            Some(b"native-set".as_slice())
        );
        assert!(store.native_validators_at(99)?.is_none());
        assert_eq!(store.metadata()?.as_deref(), Some(b"cursor".as_slice()));
        drop(store);
        let store = BlockStore::open_existing(&path)?;
        assert!(store.native_context_verified(&[9; 32])?);
        assert!(!store.native_context_verified(&[8; 32])?);
        Ok(())
    }
}
