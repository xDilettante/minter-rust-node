use crate::{
    KeyRead, MaintenanceStage, OperationControl, blocks::EVENT_KEEP_KEY, load_node, read_root,
    read_u64, read_verified_node, write_sync,
};
use anyhow::{Context, Result, ensure};
use minter_iavl::{Content, Hash, MAX_HEIGHT, Node, node_key, root_key};
use rocksdb::{DB, Direction, IteratorMode, ReadOptions, WriteBatch};
use std::collections::HashSet;

const KEEP_KEY: &[u8] = b"\0minter/iavl-keep-v1";
const FIRST_KEY: &[u8] = b"\0minter/iavl-first-v1";
const INDEX_KEY: &[u8] = b"\0minter/iavl-indexed-v1";
const BATCH_NODES: usize = 10_000;
const BATCH_VERSIONS: u64 = 1_000;

#[derive(Clone, Copy)]
pub(crate) struct Retention {
    pub(crate) first: u64,
    pub(crate) keep: u64,
}

impl Retention {
    pub(crate) fn pending(db: &DB) -> Result<Self> {
        Ok(Self {
            first: 0,
            keep: keep_versions(db)?,
        })
    }

    pub(crate) fn load(db: &DB, origin: u64, head: u64) -> Result<Self> {
        let first = first_version(db, origin)?;
        let keep = keep_versions(db)?;
        let indexed = read_u64(db, INDEX_KEY)?;
        ensure!(
            (origin..=head).contains(&first),
            "invalid IAVL history floor"
        );
        ensure!(
            indexed.is_none_or(|version| (first..=head).contains(&version)),
            "invalid IAVL orphan index progress"
        );
        ensure!(
            keep == 0 || (indexed == Some(head) && first >= oldest_to_keep(head, keep)),
            "incomplete IAVL retention metadata"
        );
        Ok(Self { first, keep })
    }

    pub(crate) fn stage_initial(self, batch: &mut WriteBatch, version: u64) {
        if self.keep != 0 {
            batch.put(INDEX_KEY, version.to_be_bytes());
        }
    }

    pub(crate) fn configure(
        &mut self,
        db: &DB,
        head: Option<u64>,
        keep: u64,
        control: &OperationControl,
    ) -> Result<()> {
        self.configure_staged(db, head, keep, control, |_| {})?;
        if head.is_some() {
            self.sweep(db, control)?;
        }
        Ok(())
    }

    /// Publish related storage settings together with the IAVL floor. Rebuild
    /// writes only a resumable orphan index while the previous policy is active.
    pub(crate) fn configure_staged(
        &mut self,
        db: &DB,
        head: Option<u64>,
        keep: u64,
        control: &OperationControl,
        stage: impl FnOnce(&mut WriteBatch),
    ) -> Result<()> {
        control.check()?;
        let mut batch = WriteBatch::default();
        let mut first = self.first;
        if self.keep != keep {
            first = match head {
                Some(head) if keep != 0 => {
                    rebuild(db, self.first, head, control)?;
                    self.stage_floor(&mut batch, oldest_to_keep(head, keep))
                }
                _ => self.first,
            };
            batch.put(KEEP_KEY, keep.to_be_bytes());
        }
        stage(&mut batch);
        if !batch.is_empty() {
            control.check()?;
            write_sync(db, batch)?;
            *self = Self { first, keep };
        }
        Ok(())
    }

    pub(crate) fn stage_commit(
        self,
        db: &DB,
        batch: &mut WriteBatch,
        version: u64,
        previous: Option<Hash>,
        reused: HashSet<Hash>,
    ) -> Result<u64> {
        if self.keep == 0 {
            return Ok(self.first);
        }
        let last_live = version - 1;
        let first = self.stage_floor(batch, oldest_to_keep(version, self.keep));
        visit_retired(db, last_live, previous, reused, |hash| {
            if last_live < first {
                batch.delete(node_key(hash));
            } else {
                batch.put(orphan_key(last_live, hash), []);
            }
            Ok(())
        })?;
        if first > self.first && self.keep > 1 {
            // Older versions were swept during open or the policy change.
            // Seek directly to the newly expired version.
            expire(db, batch, &orphan_prefix(self.first), first, usize::MAX)?;
        }
        batch.put(INDEX_KEY, version.to_be_bytes());
        Ok(first)
    }

    fn stage_floor(self, batch: &mut WriteBatch, first: u64) -> u64 {
        if first <= self.first {
            return self.first;
        }
        if first == self.first + 1 {
            batch.delete(root_key(self.first));
        } else {
            batch.delete_range(root_key(self.first), root_key(first));
        }
        batch.put(FIRST_KEY, first.to_be_bytes());
        first
    }

    // A profile change may retire many versions. Its floor is published
    // before the bounded cleanup, which can resume at the next open.
    pub(crate) fn sweep(self, db: &DB, control: &OperationControl) -> Result<()> {
        control.maintenance_progress(MaintenanceStage::IavlOrphans, 0)?;
        let mut start: Box<[u8]> = Box::from(b"o".as_slice());
        let mut removed = 0;
        loop {
            control.check()?;
            let mut batch = WriteBatch::default();
            let Some(next) = expire(db, &mut batch, &start, self.first, BATCH_NODES)? else {
                return Ok(());
            };
            removed += (batch.len() / 2) as u64;
            write_sync(db, batch)?;
            control.maintenance_progress(MaintenanceStage::IavlOrphans, removed)?;
            start = next;
        }
    }
}

/// A fresh state directory can hold a policy chosen before genesis or import.
pub(crate) fn uninitialized(db: &DB) -> Result<bool> {
    for entry in db.iterator(IteratorMode::Start) {
        let (key, _) = entry?;
        if ![KEEP_KEY, EVENT_KEEP_KEY].contains(&key.as_ref()) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn first_version(db: &impl KeyRead, origin: u64) -> Result<u64> {
    Ok(read_u64(db, FIRST_KEY)?.unwrap_or(origin))
}

pub(crate) fn keep_versions(db: &impl KeyRead) -> Result<u64> {
    Ok(read_u64(db, KEEP_KEY)?.unwrap_or(0))
}

fn oldest_to_keep(head: u64, keep: u64) -> u64 {
    head.saturating_sub(keep - 1)
}

fn orphan_prefix(version: u64) -> [u8; 9] {
    let mut key = [b'o'; 9];
    key[1..].copy_from_slice(&version.to_be_bytes());
    key
}

fn orphan_key(version: u64, hash: Hash) -> [u8; 41] {
    let mut key = [0; 41];
    key[..9].copy_from_slice(&orphan_prefix(version));
    key[9..].copy_from_slice(&hash);
    key
}

fn expire(
    db: &DB,
    batch: &mut WriteBatch,
    start: &[u8],
    first: u64,
    limit: usize,
) -> Result<Option<Box<[u8]>>> {
    let mut options = ReadOptions::default();
    options.set_iterate_upper_bound(orphan_prefix(first));
    let mode = IteratorMode::From(start, Direction::Forward);
    let mut last = None;
    for entry in db.iterator_opt(mode, options).take(limit) {
        let (key, _) = entry?;
        let hash = key
            .get(9..)
            .and_then(|bytes| bytes.try_into().ok())
            .context("invalid IAVL orphan key")?;
        batch.delete(node_key(hash));
        batch.delete(&key);
        last = Some(key);
    }
    Ok(last)
}

fn rebuild(db: &DB, first: u64, head: u64, control: &OperationControl) -> Result<()> {
    control.maintenance_progress(MaintenanceStage::IavlIndexNodes, 0)?;
    let indexed = read_u64(db, INDEX_KEY)?;
    if indexed == Some(head) {
        return Ok(());
    }
    let indexed = indexed.unwrap_or(first);
    let mut previous = read_root(db, indexed)?;
    let mut batch = WriteBatch::default();
    let mut last_write = indexed;
    let mut visited = 0;
    for version in indexed + 1..=head {
        control.check()?;
        let next = read_root(db, version)?;
        if next != previous {
            let mut reused = HashSet::new();
            if let Some(next) = next {
                Tree::new(db, version).walk(
                    next,
                    MAX_HEIGHT + 1,
                    &mut |_| {
                        index_progress(control, &mut visited)?;
                        Ok(false)
                    },
                    &mut |hash, node| {
                        if node.version < version {
                            reused.insert(hash);
                            Ok(false)
                        } else {
                            Ok(true)
                        }
                    },
                )?;
            }
            let last_live = version - 1;
            visit_retired(db, last_live, previous, reused, |hash| {
                index_progress(control, &mut visited)?;
                batch.put(orphan_key(last_live, hash), []);
                Ok(())
            })?;
        }
        if batch.len() >= BATCH_NODES || version - last_write >= BATCH_VERSIONS {
            control.check()?;
            batch.put(INDEX_KEY, version.to_be_bytes());
            write_sync(db, std::mem::take(&mut batch))?;
            last_write = version;
        }
        previous = next;
    }
    batch.put(INDEX_KEY, head.to_be_bytes());
    control.check()?;
    write_sync(db, batch)
}

fn index_progress(control: &OperationControl, visited: &mut u64) -> Result<()> {
    control.check()?;
    *visited += 1;
    if visited.is_multiple_of(1000) {
        control.maintenance_progress(MaintenanceStage::IavlIndexNodes, *visited)?;
    }
    Ok(())
}

// New nodes encode the new version. Any older subtree in the new tree was
// reused from the previous root. A retired hash cannot reappear in a later
// version, even when the same key and value are inserted again.
fn visit_retired(
    db: &DB,
    last_live: u64,
    previous: Option<Hash>,
    mut reused: HashSet<Hash>,
    mut retire: impl FnMut(Hash) -> Result<()>,
) -> Result<()> {
    if let Some(previous) = previous {
        Tree::new(db, last_live).walk(
            previous,
            MAX_HEIGHT + 1,
            &mut |hash| Ok(reused.remove(&hash)),
            &mut |hash, _| {
                retire(hash)?;
                Ok(true)
            },
        )?;
    }
    ensure!(
        reused.is_empty(),
        "reused IAVL subtree is absent from the previous root"
    );
    Ok(())
}

struct Tree<'a> {
    db: &'a DB,
    version: u64,
    scratch: Vec<u8>,
}

impl<'a> Tree<'a> {
    fn new(db: &'a DB, version: u64) -> Self {
        Self {
            db,
            version,
            scratch: Vec::new(),
        }
    }

    fn walk(
        &mut self,
        hash: Hash,
        max_height: u8,
        skip: &mut impl FnMut(Hash) -> Result<bool>,
        descend: &mut impl FnMut(Hash, &Node<'_>) -> Result<bool>,
    ) -> Result<()> {
        if skip(hash)? {
            return Ok(());
        }
        let raw = load_node(self.db, hash)?;
        let node = read_verified_node(&raw, hash, self.version, max_height, &mut self.scratch)?;
        if descend(hash, &node)?
            && let Content::Branch { left, right } = node.content
        {
            self.walk(left, node.height, skip, descend)?;
            self.walk(right, node.height, skip, descend)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BlockStore, MaintenanceStage, Mutation, OperationCancelled, OperationControl,
        RetentionPolicy, options,
    };

    #[test]
    fn cancellation_preserves_policy_publication_and_resumes_orphan_cleanup() -> Result<()> {
        for phase in [
            MaintenanceStage::IavlIndexNodes,
            MaintenanceStage::IavlOrphans,
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("state");
            let mut store = BlockStore::open(&path)?;
            store.initialize(
                99,
                (0u32..6000).map(|i| Mutation::Set {
                    key: i.to_be_bytes().to_vec(),
                    value: b"original".to_vec(),
                }),
                b"genesis",
            )?;
            let expected = store.commit(
                100,
                (0u32..6000).map(|i| Mutation::Remove(i.to_be_bytes().to_vec())),
                b"next",
            )?;
            drop(store);
            let stop = OperationControl::default();
            let request = stop.clone();
            let control = stop.with_maintenance_progress(move |at, done| {
                if at == phase && done > 0 {
                    request.cancel();
                }
            });
            let mut store = BlockStore::open_with_control(&path, &control)?;
            let policy = RetentionPolicy {
                keep_iavl_versions: 1,
                keep_event_blocks: 1,
                keep_native_blocks: 2,
            };
            let error = store
                .configure_retention(policy)
                .expect_err("cleanup ignored the cancellation callback");
            assert!(OperationCancelled::caused(&error), "{error:#}");
            assert_eq!(store.head(), Some(expected));
            if phase == MaintenanceStage::IavlIndexNodes {
                assert_eq!(store.retention_policy()?, RetentionPolicy::ARCHIVE);
                assert_eq!(store.earliest_height(), Some(99));
                assert_eq!(
                    store.value_at(99, &0u32.to_be_bytes())?,
                    Some(b"original".to_vec())
                );
            } else {
                assert_eq!(store.retention_policy()?, policy);
                assert_eq!(store.earliest_height(), Some(100));
                assert!(store.value_at(99, &0u32.to_be_bytes()).is_err());
            }
            drop(store);
            if phase == MaintenanceStage::IavlOrphans {
                let db = DB::open(&options(false), &path)?;
                let remaining = db
                    .iterator(IteratorMode::From(b"o", Direction::Forward))
                    .take_while(|entry| entry.as_ref().is_ok_and(|(key, _)| key.starts_with(b"o")))
                    .count();
                assert!(
                    remaining > 0 && remaining < 11999,
                    "no durable intermediate cleanup: {remaining}"
                );
            }
            let mut recovered = BlockStore::open(&path)?;
            recovered.configure_retention(policy)?;
            assert_eq!(recovered.head(), Some(expected));
            assert_eq!(recovered.retention_policy()?, policy);
            assert_eq!(recovered.value_at(100, &0u32.to_be_bytes())?, None);
            drop(recovered);
            let db = DB::open(&options(false), &path)?;
            assert!(
                db.iterator(IteratorMode::From(b"o", Direction::Forward))
                    .next()
                    .transpose()?
                    .is_none_or(|(key, _)| !key.starts_with(b"o"))
            );
        }
        Ok(())
    }

    #[test]
    fn cancelled_open_does_not_create_a_database() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("not-created");
        let control = OperationControl::default();
        control.cancel();
        let error = BlockStore::open_with_control(&path, &control)
            .err()
            .expect("cancelled open succeeded");
        assert!(OperationCancelled::caused(&error));
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn sweeps_pending_orphans_after_reopening_a_published_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        let mut store = BlockStore::open(&path).unwrap();
        let old = store
            .initialize(
                99,
                [Mutation::Set {
                    key: b"key".to_vec(),
                    value: b"value".to_vec(),
                }],
                b"genesis",
            )
            .unwrap()
            .app_hash;
        let head = store
            .commit(100, [Mutation::Remove(b"key".to_vec())], b"next")
            .unwrap();
        drop(store);

        let db = DB::open(&options(false), &path).unwrap();
        rebuild(&db, 99, 100, &OperationControl::default()).unwrap();
        let mut batch = WriteBatch::default();
        assert_eq!(
            Retention { first: 99, keep: 0 }.stage_floor(&mut batch, 100),
            100
        );
        batch.put(KEEP_KEY, 1u64.to_be_bytes());
        write_sync(&db, batch).unwrap();
        assert!(db.get(root_key(99)).unwrap().is_none());
        assert!(db.get(node_key(old)).unwrap().is_some());
        assert!(db.get(orphan_key(99, old)).unwrap().is_some());
        drop(db);

        let store = BlockStore::open(&path).unwrap();
        assert_eq!(store.earliest_height(), Some(100));
        assert_eq!(store.head(), Some(head));
        assert_eq!(store.value_at(100, b"key").unwrap(), None);
        drop(store);
        let db = DB::open(&options(false), &path).unwrap();
        assert!(db.get(node_key(old)).unwrap().is_none());
        assert!(db.get(orphan_key(99, old)).unwrap().is_none());
    }
}
