use crate::{
    CHECKPOINT_KEY, Checkpoint, Hash, IAVL_HEIGHT_KEY, IMPORT_KEY, IavlHead, KeyRead, Mutation,
    OperationControl, StateProof, StateStore, begin_import, check_height, finish_import,
    iavl::empty_hash,
    metadata_key, proofs,
    prune::{Retention, first_version, keep_versions},
    read_first_key, read_keys, read_last_key, read_range, read_root, read_u64, read_value,
    snapshot, stage_iavl, validate_checkpoint, verify_root, write_sync,
};
use anyhow::{Context, Result, ensure};
use rocksdb::{
    BottommostLevelCompaction, CompactOptions, DB, ReadOptions, Snapshot, WriteBatch, properties,
};
use std::{
    collections::HashSet,
    fs::File,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

mod context;
mod native_retention;
mod storage_policy;
pub use native_retention::NativeContextWrite;
pub use storage_policy::RetentionPolicy;

const GENESIS_HEIGHT_KEY: &[u8] = b"\0minter/genesis-height-v1";
pub(crate) const EXECUTION_KEY: &[u8] = b"\0minter/execution-v1";
pub(crate) const EVENT_KEEP_KEY: &[u8] = b"\0minter/event-keep-v1";
const EVENT_PREFIX: &[u8; 24] = b"\0minter/block-events-v1/";
const NATIVE_BLOCK_PREFIX: &[u8; 24] = b"\0minter/native-block-v1/";
const NATIVE_VALIDATORS_PREFIX: &[u8] = b"\0minter/native-validators-v1/";
const COMMITTED_EVIDENCE_PREFIX: &[u8] = b"\0minter/committed-evidence-v1/";
const EVIDENCE_INDEX_START: &[u8] = b"\0minter/evidence-index-start-v1";
const PENDING_EVIDENCE: &[u8] = b"\0minter/pending-evidence-v1";

fn checkpoint_import_manifest(trusted: &Checkpoint, native: Option<[u8; 32]>) -> Result<Vec<u8>> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct NativeIntent<'a> {
        #[serde(flatten)]
        snapshot: &'a Checkpoint,
        #[serde(with = "hex::serde")]
        native_manifest_sha256: [u8; 32],
    }
    Ok(match native {
        Some(native_manifest_sha256) => serde_json::to_vec(&NativeIntent {
            snapshot: trusted,
            native_manifest_sha256,
        })?,
        None => serde_json::to_vec(trusted)?,
    })
}

pub struct NativeBlockWrite<'a> {
    /// Block sync authenticates the new root through a signed successor.
    pub expected_root: Option<[u8; 32]>,
    pub record: &'a [u8],
    pub validators: Option<&'a [u8]>,
    pub committed_evidence: &'a [Hash],
    pub pending_evidence: Option<&'a [u8]>,
}

pub struct NativeCheckpointWrite {
    pub metadata: Vec<u8>,
    pub block_record: Vec<u8>,
    pub validators: Vec<u8>,
    pub committed_evidence: Vec<Hash>,
}

/// The chain's IAVL versions and its execution metadata share a durable batch.
pub struct BlockStore {
    db: Arc<DB>,
    directory: PathBuf,
    first_height: Option<u64>,
    checkpoint: Option<Checkpoint>,
    head: Option<IavlHead>,
    retention: Retention,
    keep_event_blocks: u64,
    maintenance_control: OperationControl,
}

/// Create snapshots of the writer's database from another thread.
#[derive(Clone)]
pub struct StateReader {
    db: Arc<DB>,
    origin: u64,
}

/// One immutable view of a retained block. RocksDB pins its data until drop.
pub struct HistoricalState<'a> {
    snapshot: Snapshot<'a>,
    head: IavlHead,
    version: u64,
    latest_version: u64,
    root: Option<Hash>,
}

impl StateReader {
    /// Native blocks are stored independently of the retained IAVL roots.
    pub fn native_block_range(&self) -> Result<Option<(u64, u64)>> {
        self.height_range(NATIVE_BLOCK_PREFIX, Some(native_retention::BASE_KEY))
    }

    pub fn event_block_range(&self) -> Result<Option<(u64, u64)>> {
        self.height_range(EVENT_PREFIX, None)
    }

    fn height_range(&self, prefix: &[u8], floor_key: Option<&[u8]>) -> Result<Option<(u64, u64)>> {
        let snapshot = self.db.snapshot();
        let floor = floor_key
            .map(|key| read_u64(&snapshot, key))
            .transpose()?
            .flatten();
        let mut bounds = ReadOptions::default();
        bounds.set_iterate_range(rocksdb::PrefixRange(prefix));
        let mut records = snapshot.raw_iterator_opt(bounds);
        if let Some(floor) = floor {
            records.seek(prefixed(prefix, &floor.to_be_bytes()));
        } else {
            records.seek_to_first();
        }
        let Some(first) = records
            .key()
            .map(|key| record_height(prefix, key))
            .transpose()?
        else {
            records.status()?;
            ensure!(floor.is_none(), "stored native archive floor has no block");
            return Ok(None);
        };
        ensure!(
            floor.is_none_or(|floor| first == floor),
            "stored native archive floor differs from the first block"
        );
        records.seek_to_last();
        let last = records
            .key()
            .map(|key| record_height(prefix, key))
            .transpose()?
            .context("missing last native block")?;
        records.status()?;
        Ok(Some((first, last)))
    }

    pub fn native_block_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(native_block_key(height))?)
    }

    pub fn at(&self, height: u64) -> Result<HistoricalState<'_>> {
        self.view(Some(height))
    }

    /// Resolve an IAVL version as used by Go's historical Minter v2 API.
    pub fn at_version(&self, version: u64) -> Result<HistoricalState<'_>> {
        let height = version
            .checked_sub(u64::from(self.origin == 0))
            .context("IAVL version must be positive")?;
        self.at(height)
    }

    /// Resolve the head and pin its tree and execution metadata together.
    pub fn latest(&self) -> Result<HistoricalState<'_>> {
        self.view(None)
    }

    fn view(&self, height: Option<u64>) -> Result<HistoricalState<'_>> {
        let snapshot = self.db.snapshot();
        let offset = u64::from(self.origin == 0);
        let origin = self.origin + offset;
        let first = first_version(&snapshot, origin)?;
        let head = read_u64(&snapshot, IAVL_HEIGHT_KEY)?.unwrap_or(origin);
        ensure!(
            (origin..=head).contains(&first),
            "invalid IAVL history floor"
        );
        let height = height.unwrap_or(head - offset);
        check_height(height, first - offset, head - offset)?;
        let version = height + offset;
        let root = read_root(&snapshot, version)?;
        Ok(HistoricalState {
            snapshot,
            head: IavlHead {
                height,
                app_hash: root.unwrap_or_else(empty_hash),
            },
            version,
            latest_version: head,
            root,
        })
    }
}

impl HistoricalState<'_> {
    pub fn head(&self) -> IavlHead {
        self.head
    }

    /// Physical IAVL version; chains beginning at block one have an offset.
    pub fn iavl_version(&self) -> u64 {
        self.version
    }

    pub fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        read_value(&self.snapshot, self.root, self.version, key)
    }

    pub fn proof(&self, key: &[u8]) -> Result<StateProof> {
        proofs::prove(&self.snapshot, self.root, self.version, key)
    }

    /// Export the physical IAVL tree pinned by this RocksDB view.
    pub fn write_go_snapshot(&self, metadata: &[(&str, Vec<u8>)], file: &mut File) -> Result<u64> {
        self.write_go_snapshot_with_control(metadata, file, &OperationControl::default())
    }

    /// Cancellation leaves the caller's output incomplete; publish only on success.
    pub fn write_go_snapshot_with_control(
        &self,
        metadata: &[(&str, Vec<u8>)],
        file: &mut File,
        control: &OperationControl,
    ) -> Result<u64> {
        control.check()?;
        ensure!(
            self.version == self.head.height,
            "Go snapshot requires an unshifted IAVL version"
        );
        snapshot::export(
            &self.snapshot,
            self.root,
            self.version,
            metadata,
            file,
            control,
        )
    }

    /// Return the metadata committed with the latest version in this snapshot.
    pub fn execution_metadata(&self) -> Result<Option<Vec<u8>>> {
        ensure!(
            self.version == self.latest_version,
            "execution metadata requires the latest version"
        );
        Ok(self.snapshot.get(EXECUTION_KEY)?)
    }

    /// Read the events committed at a block height, independently of IAVL versions.
    pub fn block_events(&self, height: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.snapshot.get(event_key(height))?)
    }

    /// Return the configured retention policy at the time the snapshot was pinned.
    pub fn keep_iavl_versions(&self) -> Result<u64> {
        keep_versions(&self.snapshot)
    }

    /// Visit one half-open key range from this pinned version.
    pub fn visit_range(
        &self,
        keys: Range<&[u8]>,
        visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        visit_range(&self.snapshot, self.root, self.version, keys, visit)
    }
}

fn visit_range(
    db: &impl KeyRead,
    root: Option<Hash>,
    version: u64,
    keys: Range<&[u8]>,
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    if keys.start < keys.end
        && let Some(root) = root
    {
        read_range(
            db,
            root,
            version,
            crate::iavl::MAX_HEIGHT + 1,
            &keys,
            &mut Vec::new(),
            &mut visit,
        )?;
    }
    Ok(())
}

/// Read the imported state before publishing its first committed version.
pub struct SnapshotView<'a> {
    db: &'a DB,
    imported: &'a snapshot::Imported,
    height: u64,
}

impl SnapshotView<'_> {
    pub fn metadata(&self, name: &str) -> Option<&[u8]> {
        self.imported.metadata.get(name).map(Vec::as_slice)
    }

    pub fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        read_value(self.db, self.imported.root, self.height, key)
    }
}

impl BlockStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_control(path, &OperationControl::default())
    }

    pub fn open_with_control(path: &Path, control: &OperationControl) -> Result<Self> {
        Self::open_for_execution(path, None, None, control)
    }

    /// Open an existing database with the writer lock for offline maintenance.
    pub fn open_existing(path: &Path) -> Result<Self> {
        let store = Self::open_with_import(path, false, None, None, &OperationControl::default())?;
        ensure!(
            store.head.is_some(),
            "database has no committed Minter state"
        );
        Ok(store)
    }

    /// Also reopen a store with an interrupted import of this trusted checkpoint.
    pub fn open_for_checkpoint(path: &Path, trusted: &Checkpoint) -> Result<Self> {
        validate_checkpoint(trusted)?;
        let manifest = serde_json::to_vec(trusted)?;
        Self::open_for_execution(path, Some(&manifest), None, &OperationControl::default())
    }

    /// Resume a native import with the same full package identity. A legacy
    /// snapshot-only intent can be claimed when authenticated installation starts.
    pub fn open_for_native_checkpoint(
        path: &Path,
        trusted: &Checkpoint,
        native_manifest_sha256: [u8; 32],
    ) -> Result<Self> {
        Self::open_for_native_checkpoint_with_control(
            path,
            trusted,
            native_manifest_sha256,
            &OperationControl::default(),
        )
    }

    pub fn open_for_native_checkpoint_with_control(
        path: &Path,
        trusted: &Checkpoint,
        native_manifest_sha256: [u8; 32],
        control: &OperationControl,
    ) -> Result<Self> {
        validate_checkpoint(trusted)?;
        let legacy = serde_json::to_vec(trusted)?;
        let manifest = checkpoint_import_manifest(trusted, Some(native_manifest_sha256))?;
        Self::open_for_execution(path, Some(&manifest), Some(&legacy), control)
    }

    fn open_for_execution(
        path: &Path,
        manifest: Option<&[u8]>,
        legacy: Option<&[u8]>,
        control: &OperationControl,
    ) -> Result<Self> {
        let store = Self::open_with_import(path, true, manifest, legacy, control)?;
        if let (Some(checkpoint), Some(head)) = (store.checkpoint(), store.head()) {
            ensure!(
                head.height == checkpoint.height || store.metadata()?.is_some(),
                "IAVL trace head has no matching execution metadata"
            );
        }
        if store.head.is_some() {
            store
                .retention
                .sweep(&store.db, &store.maintenance_control)?;
        }
        Ok(store)
    }

    fn open_with_import(
        path: &Path,
        create: bool,
        manifest: Option<&[u8]>,
        legacy: Option<&[u8]>,
        control: &OperationControl,
    ) -> Result<Self> {
        control.check()?;
        let (db, directory) =
            crate::open_database(path, create).context("open RocksDB for block execution")?;
        let keep_event_blocks = read_u64(&db, EVENT_KEEP_KEY)?.unwrap_or_default();
        let Some(bytes) = db.get(GENESIS_HEIGHT_KEY)? else {
            if db.get(CHECKPOINT_KEY)?.is_some() {
                let store = StateStore::from_db(db)?;
                return Ok(Self {
                    db: Arc::new(store.db),
                    directory,
                    first_height: Some(store.checkpoint.height),
                    checkpoint: Some(store.checkpoint),
                    head: Some(store.head),
                    retention: store.retention,
                    keep_event_blocks,
                    maintenance_control: control.clone(),
                });
            }
            if let Some(pending) = db.get(IMPORT_KEY)? {
                ensure!(
                    Some(pending.as_slice()) == manifest
                        || (legacy.is_some() && Some(pending.as_slice()) == legacy),
                    "resume the interrupted import with the same trusted checkpoint package"
                );
            } else {
                ensure!(
                    crate::prune::uninitialized(&db)?,
                    "RocksDB lacks a committed Minter genesis or checkpoint"
                );
            }
            let retention = Retention::pending(&db)?;
            return Ok(Self {
                db: Arc::new(db),
                directory,
                first_height: None,
                checkpoint: None,
                head: None,
                retention,
                keep_event_blocks,
                maintenance_control: control.clone(),
            });
        };
        let genesis_height = u64::from_be_bytes(bytes.as_slice().try_into()?);
        let version = read_u64(&db, IAVL_HEIGHT_KEY)?.context("missing IAVL head")?;
        let height = version
            .checked_sub(u64::from(genesis_height == 0))
            .context("invalid native IAVL version")?;
        ensure!(
            height >= genesis_height
                && version <= i64::MAX as u64
                && db.get(EXECUTION_KEY)?.is_some(),
            "invalid block execution metadata"
        );
        let root = read_root(&db, version)?;
        if let Some(hash) = root {
            verify_root(&db, version, hash)?;
        }
        let head = IavlHead {
            height,
            app_hash: root.unwrap_or_else(empty_hash),
        };
        let retention = Retention::load(&db, genesis_height.max(1), version)?;
        Ok(Self {
            db: Arc::new(db),
            directory,
            first_height: Some(genesis_height),
            checkpoint: None,
            head: Some(head),
            retention,
            keep_event_blocks,
            maintenance_control: control.clone(),
        })
    }

    pub fn head(&self) -> Option<IavlHead> {
        self.head
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Shared request for non-Commit work owned by this writer.
    pub fn maintenance_control(&self) -> &OperationControl {
        &self.maintenance_control
    }

    pub fn earliest_height(&self) -> Option<u64> {
        self.first_height
            .map(|origin| self.retention.first - u64::from(origin == 0))
    }

    pub fn keep_iavl_versions(&self) -> u64 {
        self.retention.keep
    }

    pub fn keep_event_blocks(&self) -> u64 {
        self.keep_event_blocks
    }

    /// Zero retains all future versions. Previously discarded roots stay discarded.
    pub fn set_keep_iavl_versions(&mut self, keep: u64) -> Result<()> {
        let version = self.head.map(|head| self.version(head.height));
        self.retention
            .configure(&self.db, version, keep, &self.maintenance_control)
    }

    /// Zero retains all future event batches. Previously removed batches stay removed.
    pub fn set_keep_event_blocks(&mut self, keep: u64) -> Result<()> {
        self.maintenance_control.check()?;
        if keep == self.keep_event_blocks {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        Self::stage_event_retention(&mut batch, self.head, keep);
        write_sync(&self.db, batch)?;
        self.keep_event_blocks = keep;
        Ok(())
    }

    fn stage_event_retention(batch: &mut WriteBatch, head: Option<IavlHead>, keep: u64) {
        if let Some(head) = head
            && keep != 0
            && head.height >= keep
        {
            batch.delete_range(event_key(0), event_key(head.height - keep + 1));
        }
        batch.put(EVENT_KEEP_KEY, keep.to_be_bytes());
    }

    /// Compact the database after pruning to reclaim files on disk.
    pub fn compact(&self) -> Result<()> {
        self.db.flush().context("flush IAVL before compaction")?;
        let mut options = CompactOptions::default();
        options.set_bottommost_level_compaction(BottommostLevelCompaction::ForceOptimized);
        self.db
            .compact_range_opt::<&[u8], &[u8]>(None, None, &options);
        ensure!(
            self.db
                .property_int_value(properties::BACKGROUND_ERRORS)?
                .unwrap_or(0)
                == 0,
            "RocksDB reported a background error, see its LOG file"
        );
        Ok(())
    }

    pub fn reader(&self) -> Option<StateReader> {
        Some(StateReader {
            db: Arc::clone(&self.db),
            origin: self.first_height?,
        })
    }

    pub fn metadata(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(EXECUTION_KEY)?)
    }

    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    pub fn checkpoint_metadata(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(metadata_key(name))?)
    }

    /// Verify the snapshot and application data before exposing the new head.
    pub fn install_checkpoint<T>(
        &mut self,
        file: File,
        trusted: &Checkpoint,
        prepare: impl FnOnce(SnapshotView<'_>) -> Result<T>,
    ) -> Result<T> {
        self.install_checkpoint_with_control(file, trusted, &OperationControl::default(), prepare)
    }

    pub fn install_checkpoint_with_control<T>(
        &mut self,
        file: File,
        trusted: &Checkpoint,
        control: &OperationControl,
        prepare: impl FnOnce(SnapshotView<'_>) -> Result<T>,
    ) -> Result<T> {
        self.install_checkpoint_batch(file, trusted, None, control, |view| {
            Ok((prepare(view)?, None))
        })
    }

    /// Publish the snapshot, execution metadata and signed block in one batch.
    pub fn install_native_checkpoint<T>(
        &mut self,
        file: File,
        trusted: &Checkpoint,
        prepare: impl FnOnce(SnapshotView<'_>) -> Result<(T, NativeCheckpointWrite)>,
    ) -> Result<T> {
        self.install_checkpoint_batch(file, trusted, None, &OperationControl::default(), |view| {
            let (prepared, native) = prepare(view)?;
            Ok((prepared, Some(native)))
        })
    }

    pub fn install_native_checkpoint_with_control<T>(
        &mut self,
        file: File,
        trusted: &Checkpoint,
        native_manifest_sha256: [u8; 32],
        control: &OperationControl,
        prepare: impl FnOnce(SnapshotView<'_>) -> Result<(T, NativeCheckpointWrite)>,
    ) -> Result<T> {
        self.install_checkpoint_batch(
            file,
            trusted,
            Some(native_manifest_sha256),
            control,
            |view| {
                let (prepared, native) = prepare(view)?;
                Ok((prepared, Some(native)))
            },
        )
    }

    fn install_checkpoint_batch<T>(
        &mut self,
        mut file: File,
        trusted: &Checkpoint,
        native_manifest_sha256: Option<[u8; 32]>,
        control: &OperationControl,
        prepare: impl FnOnce(SnapshotView<'_>) -> Result<(T, Option<NativeCheckpointWrite>)>,
    ) -> Result<T> {
        ensure!(self.head.is_none(), "store already has committed state");
        control.check()?;
        validate_checkpoint(trusted)?;
        snapshot::verify_file_controlled(&mut file, trusted, control)?;
        let manifest = serde_json::to_vec(trusted)?;
        let intent = checkpoint_import_manifest(trusted, native_manifest_sha256)?;
        if native_manifest_sha256.is_some()
            && self
                .db
                .get(IMPORT_KEY)?
                .is_some_and(|pending| pending == manifest)
        {
            // Only the authenticated caller may adopt an older snapshot-only
            // intent. Opening the database alone never changes that identity.
            let mut batch = WriteBatch::default();
            batch.put(IMPORT_KEY, &intent);
            write_sync(&self.db, batch)?;
        }
        begin_import(&self.db, &intent)?;
        let mut imported = snapshot::load_controlled(&self.db, file, trusted, control)?;
        control.check()?;
        if !imported.nodes.is_empty() {
            write_sync(&self.db, std::mem::take(&mut imported.nodes))?;
        }
        control.check()?;
        let (prepared, native) = prepare(SnapshotView {
            db: &self.db,
            imported: &imported,
            height: trusted.height,
        })?;
        control.check()?;
        let mut batch = imported.into_batch(trusted.height);
        self.retention.stage_initial(&mut batch, trusted.height);
        if let Some(native) = native {
            batch.put(EXECUTION_KEY, native.metadata);
            batch.put(native_block_key(trusted.height), native.block_record);
            Self::stage_evidence_index(
                &mut batch,
                trusted.height,
                Some(&native.validators),
                &native.committed_evidence,
                true,
            );
        }
        control.check()?;
        // The publication itself finishes atomically if cancellation arrives now.
        finish_import(&self.db, batch, manifest)?;
        self.retention.first = trusted.height;
        self.first_height = Some(trusted.height);
        self.checkpoint = Some(trusted.clone());
        self.head = Some(IavlHead {
            height: trusted.height,
            app_hash: trusted.app_hash,
        });
        Ok(prepared)
    }

    pub fn value_at(&self, height: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (version, root) = self.version_root(height)?;
        read_value(&*self.db, root, version, key)
    }

    pub fn proof_at(&self, height: u64, key: &[u8]) -> Result<crate::StateProof> {
        let (version, root) = self.version_root(height)?;
        proofs::prove(&*self.db, root, version, key)
    }

    /// Visit committed IAVL entries in ascending key order within a half-open range.
    pub fn visit_range_at(
        &self,
        height: u64,
        keys: Range<&[u8]>,
        visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        let (version, root) = self.version_root(height)?;
        visit_range(&*self.db, root, version, keys, visit)
    }

    /// Visit the requested keys that exist at a committed height. The keys
    /// must be strictly increasing so each shared path is read once.
    pub fn visit_keys_at<K: AsRef<[u8]>>(
        &self,
        height: u64,
        keys: &[K],
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            keys.windows(2)
                .all(|pair| pair[0].as_ref() < pair[1].as_ref()),
            "IAVL lookup keys must be sorted and distinct"
        );
        let (version, root) = self.version_root(height)?;
        if !keys.is_empty()
            && let Some(root) = root
        {
            read_keys(
                &self.db,
                root,
                version,
                crate::iavl::MAX_HEIGHT + 1,
                keys,
                &mut Vec::new(),
                &mut visit,
            )?;
        }
        Ok(())
    }

    /// Return the first key in byte order greater than or equal to the bound.
    pub fn first_key_at_or_after(&self, height: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (version, root) = self.version_root(height)?;
        let Some(root) = root else { return Ok(None) };
        read_first_key(
            &*self.db,
            root,
            version,
            crate::iavl::MAX_HEIGHT + 1,
            Some(key),
            &mut Vec::new(),
        )
    }

    /// Return the last key in byte order strictly below the bound.
    pub fn last_key_before(&self, height: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (version, root) = self.version_root(height)?;
        let Some(root) = root else { return Ok(None) };
        read_last_key(
            &*self.db,
            root,
            version,
            crate::iavl::MAX_HEIGHT + 1,
            Some(key),
            &mut Vec::new(),
        )
    }

    fn version_root(&self, height: u64) -> Result<(u64, Option<crate::Hash>)> {
        let head = self.head.context("genesis is unavailable")?;
        let first = self.earliest_height().context("missing first height")?;
        check_height(height, first, head.height)?;
        let version = self.version(height);
        Ok((version, read_root(&*self.db, version)?))
    }

    /// Minter saves the genesis root at InitialHeight - 1 (at version 1
    /// when that is zero). The genesis nodes themselves carry version 1.
    pub fn initialize(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        metadata: &[u8],
    ) -> Result<IavlHead> {
        self.initialize_with_metadata(height, operations, |_| Ok(metadata.to_vec()))
    }

    /// Build execution metadata with the final root before publishing genesis.
    pub fn initialize_with_metadata(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        metadata: impl FnOnce(IavlHead) -> Result<Vec<u8>>,
    ) -> Result<IavlHead> {
        self.maintenance_control.check()?;
        ensure!(self.head.is_none(), "genesis already exists");
        ensure!(
            self.db.get(IMPORT_KEY)?.is_none(),
            "resume the incomplete checkpoint import before initializing genesis"
        );
        ensure!(height < i64::MAX as u64, "invalid genesis height");
        let version = height.max(1);
        let mut batch = WriteBatch::default();
        let app_hash = crate::stage_iavl_with(&self.db, &mut batch, version, |db, batch| {
            crate::mutations::apply_genesis(db, operations, batch, &self.maintenance_control)
        })?;
        self.retention.stage_initial(&mut batch, version);
        batch.put(GENESIS_HEIGHT_KEY, height.to_be_bytes());
        let head = IavlHead { height, app_hash };
        batch.put(EXECUTION_KEY, metadata(head)?);
        self.maintenance_control.check()?;
        write_sync(&self.db, batch)?;
        self.retention.first = version;
        self.first_height = Some(height);
        self.head = Some(head);
        Ok(head)
    }

    /// Adopt native metadata for a legacy genesis that has no committed blocks.
    pub fn update_genesis_metadata(&self, height: u64, metadata: &[u8]) -> Result<()> {
        ensure!(
            self.first_height == Some(height)
                && self.head.is_some_and(|head| head.height == height)
                && self.native_block_base()?.is_none(),
            "genesis metadata cannot change after the first block"
        );
        let mut batch = WriteBatch::default();
        batch.put(EXECUTION_KEY, metadata);
        self.maintenance_control.check()?;
        write_sync(&self.db, batch)
    }

    pub fn commit(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        metadata: &[u8],
    ) -> Result<IavlHead> {
        self.commit_block(height, operations, |_| Ok(metadata), None, None)
    }

    pub fn commit_with_events(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        metadata: &[u8],
        events: &[u8],
    ) -> Result<IavlHead> {
        self.commit_block(height, operations, |_| Ok(metadata), Some(events), None)
    }

    /// Publish the signed block and its state in the same batch.
    pub fn commit_native_block(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        metadata: impl FnOnce(IavlHead) -> Result<Vec<u8>>,
        events: &[u8],
        block: NativeBlockWrite<'_>,
    ) -> Result<IavlHead> {
        self.commit_block(height, operations, metadata, Some(events), Some(block))
    }

    pub fn native_block_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(native_block_key(height))?)
    }

    /// Replace only the head's native record after extending its commit certificate.
    /// The application validates new signatures; execution state and height stay fixed.
    pub fn update_native_block_record(
        &mut self,
        height: u64,
        expected: &[u8],
        record: &[u8],
    ) -> Result<()> {
        ensure!(
            self.head.is_some_and(|head| head.height == height),
            "native record is not the chain head"
        );
        let key = native_block_key(height);
        ensure!(
            self.db.get(key)?.as_deref() == Some(expected),
            "native record changed before its commit update"
        );
        let mut batch = WriteBatch::default();
        batch.put(key, record);
        write_sync(&self.db, batch)
    }

    /// A change point in the Tendermint validator set, independent of IAVL history.
    pub fn native_validators_at(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let mut bounds = ReadOptions::default();
        bounds.set_iterate_range(rocksdb::PrefixRange(NATIVE_VALIDATORS_PREFIX));
        let mut sets = self.db.raw_iterator_opt(bounds);
        sets.seek_for_prev(prefixed(NATIVE_VALIDATORS_PREFIX, &height.to_be_bytes()));
        sets.status()?;
        Ok(sets.value().map(<[u8]>::to_vec))
    }

    pub fn evidence_index_start(&self) -> Result<Option<u64>> {
        read_u64(&*self.db, EVIDENCE_INDEX_START)
    }

    pub fn native_block_base(&self) -> Result<Option<u64>> {
        self.reader()
            .map(|reader| {
                reader
                    .native_block_range()
                    .map(|range| range.map(|(base, _)| base))
            })
            .transpose()
            .map(Option::flatten)
    }

    /// Persist a historical change point without publishing the index floor.
    /// Interrupted migrations may repeat these idempotent writes.
    pub fn backfill_evidence_height(
        &self,
        height: u64,
        validators: Option<&[u8]>,
        committed: &[Hash],
    ) -> Result<()> {
        ensure!(
            self.head.is_some_and(|head| height <= head.height),
            "evidence backfill exceeds chain head"
        );
        let mut batch = WriteBatch::default();
        Self::stage_evidence_index(&mut batch, height, validators, committed, false);
        write_sync(&self.db, batch)
    }

    pub fn publish_evidence_index_start(&self, height: u64) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.put(EVIDENCE_INDEX_START, height.to_be_bytes());
        write_sync(&self.db, batch)
    }

    pub fn evidence_committed(&self, hash: &Hash) -> Result<bool> {
        Ok(self
            .db
            .get(prefixed(COMMITTED_EVIDENCE_PREFIX, hash))?
            .is_some())
    }

    pub fn pending_evidence(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(PENDING_EVIDENCE)?)
    }

    pub fn store_pending_evidence(&self, evidence: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.put(PENDING_EVIDENCE, evidence);
        write_sync(&self.db, batch)
    }

    fn stage_evidence_index(
        batch: &mut WriteBatch,
        height: u64,
        validators: Option<&[u8]>,
        committed: &[Hash],
        initialize: bool,
    ) {
        if let Some(validators) = validators {
            batch.put(
                prefixed(NATIVE_VALIDATORS_PREFIX, &height.to_be_bytes()),
                validators,
            );
        }
        for hash in committed {
            batch.put(
                prefixed(COMMITTED_EVIDENCE_PREFIX, hash),
                height.to_be_bytes(),
            );
        }
        if initialize {
            batch.put(EVIDENCE_INDEX_START, height.to_be_bytes());
        }
    }

    fn commit_block<M: AsRef<[u8]>>(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        metadata: impl FnOnce(IavlHead) -> Result<M>,
        events: Option<&[u8]>,
        native: Option<NativeBlockWrite<'_>>,
    ) -> Result<IavlHead> {
        let head = self
            .head
            .context("initialize genesis before committing blocks")?;
        ensure!(
            height == head.height + 1 && height < i64::MAX as u64,
            "unexpected block height {height}"
        );
        let version = self.version(height);
        let previous = read_root(&*self.db, self.version(head.height))?;
        let mut batch = WriteBatch::default();
        let mut reused = HashSet::new();
        let app_hash = stage_iavl(
            &self.db,
            &mut batch,
            version,
            version,
            previous,
            operations,
            |hash| {
                if self.retention.keep != 0 {
                    reused.insert(hash);
                }
            },
        )?;
        if let Some(native) = native {
            if let Some(expected) = native.expected_root {
                ensure!(
                    app_hash == expected,
                    "AppHash differs at height {height}: execution {}, signed successor {}",
                    hex::encode(app_hash),
                    hex::encode(expected),
                );
            }
            batch.put(native_block_key(height), native.record);
            Self::stage_evidence_index(
                &mut batch,
                height,
                native.validators,
                native.committed_evidence,
                self.evidence_index_start()?.is_none(),
            );
            if let Some(pending) = native.pending_evidence {
                batch.put(PENDING_EVIDENCE, pending);
            }
        }
        let first = self
            .retention
            .stage_commit(&self.db, &mut batch, version, previous, reused)?;
        let head = IavlHead { height, app_hash };
        batch.put(EXECUTION_KEY, metadata(head)?);
        if let Some(events) = events {
            batch.put(event_key(height), events);
        }
        if self.keep_event_blocks != 0 && height >= self.keep_event_blocks {
            batch.delete(event_key(height - self.keep_event_blocks));
        }
        write_sync(&self.db, batch)?;
        self.retention.first = first;
        self.head = Some(head);
        Ok(head)
    }

    fn version(&self, height: u64) -> u64 {
        height + u64::from(self.first_height == Some(0))
    }
}

fn event_key(height: u64) -> [u8; EVENT_PREFIX.len() + 8] {
    let mut key = [0; EVENT_PREFIX.len() + 8];
    key[..EVENT_PREFIX.len()].copy_from_slice(EVENT_PREFIX);
    key[EVENT_PREFIX.len()..].copy_from_slice(&height.to_be_bytes());
    key
}

fn native_block_key(height: u64) -> [u8; NATIVE_BLOCK_PREFIX.len() + 8] {
    let mut key = [0; NATIVE_BLOCK_PREFIX.len() + 8];
    key[..NATIVE_BLOCK_PREFIX.len()].copy_from_slice(NATIVE_BLOCK_PREFIX);
    key[NATIVE_BLOCK_PREFIX.len()..].copy_from_slice(&height.to_be_bytes());
    key
}

fn prefixed(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + suffix.len());
    key.extend_from_slice(prefix);
    key.extend_from_slice(suffix);
    key
}

fn record_height(prefix: &[u8], key: &[u8]) -> Result<u64> {
    let suffix = key
        .strip_prefix(prefix)
        .context("invalid block record key")?;
    Ok(u64::from_be_bytes(suffix.try_into()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_genesis_does_not_publish_a_head() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        let control = OperationControl::default();
        let mut store = BlockStore::open_with_control(&path, &control)?;
        control.cancel();
        let error = store
            .initialize(
                99,
                [Mutation::Set {
                    key: b"balance".to_vec(),
                    value: vec![7],
                }],
                b"genesis",
            )
            .unwrap_err();
        assert!(crate::OperationCancelled::caused(&error));
        assert_eq!(store.head(), None);
        assert_eq!(store.metadata()?, None);
        drop(store);
        let mut store = BlockStore::open(&path)?;
        assert_eq!(store.head(), None);
        store.initialize(99, [], b"fresh genesis")?;
        assert_eq!(store.head().unwrap().height, 99);
        Ok(())
    }

    #[test]
    fn native_genesis_metadata_must_finish_before_publication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let control = OperationControl::default();
        let mut store = BlockStore::open_with_control(&directory.path().join("state"), &control)?;
        let operations = || {
            [Mutation::Set {
                key: b"balance".to_vec(),
                value: vec![7],
            }]
        };
        let error = store
            .initialize_with_metadata(99, operations(), |_| {
                anyhow::bail!("invalid native parameters")
            })
            .unwrap_err();
        assert!(error.to_string().contains("invalid native parameters"));
        assert_eq!(store.head(), None);
        assert_eq!(store.metadata()?, None);
        let error = store
            .initialize_with_metadata(99, operations(), |head| {
                control.cancel();
                Ok(head.app_hash.to_vec())
            })
            .unwrap_err();
        assert!(crate::OperationCancelled::caused(&error));
        assert_eq!(store.head(), None);
        assert_eq!(store.metadata()?, None);
        let path = directory.path().join("state");
        drop(store);
        let mut store = BlockStore::open(&path)?;
        let head =
            store.initialize_with_metadata(99, operations(), |head| Ok(head.app_hash.to_vec()))?;
        assert_eq!(store.metadata()?.as_deref(), Some(head.app_hash.as_slice()));
        assert!(
            store
                .update_genesis_metadata(99, b"adopted native identity")
                .is_ok()
        );
        store.commit(100, [], b"block metadata")?;
        assert!(
            store
                .update_genesis_metadata(99, b"must be refused")
                .is_err()
        );
        assert_eq!(
            store.metadata()?.as_deref(),
            Some(b"block metadata".as_slice())
        );
        Ok(())
    }

    #[test]
    fn genesis_can_stop_during_mutations_or_nodes_without_affecting_live_commit() -> Result<()> {
        use crate::{GenesisStage, OperationCancelled};
        let directory = tempfile::tempdir()?;
        let operations = || {
            (0u32..5000).map(|index| Mutation::Set {
                key: index.to_be_bytes().to_vec(),
                value: vec![7; 16],
            })
        };
        let mut baseline = BlockStore::open(&directory.path().join("baseline"))?;
        let expected = baseline.initialize(99, operations(), b"genesis")?;
        for phase in [GenesisStage::IavlMutations, GenesisStage::IavlNodes] {
            let path = directory.path().join(phase.to_string());
            let control = OperationControl::default();
            let stop = control.clone();
            let control = control.with_genesis_progress(move |current, done| {
                if current == phase && done == 1000 {
                    stop.cancel();
                }
            });
            let mut store = BlockStore::open_with_control(&path, &control)?;
            let error = store.initialize(99, operations(), b"genesis").unwrap_err();
            assert!(OperationCancelled::caused(&error));
            assert_eq!(store.head(), None);
            assert_eq!(store.metadata()?, None);
            assert_eq!(store.db.get(crate::iavl::root_key(99))?, None);
            assert_eq!(store.db.get(IAVL_HEIGHT_KEY)?, None);
            drop(store);
            let live_control = OperationControl::default();
            let mut store = BlockStore::open_with_control(&path, &live_control)?;
            assert_eq!(store.initialize(99, operations(), b"genesis")?, expected);
            live_control.cancel();
            let block = || {
                [Mutation::Set {
                    key: vec![0; 4],
                    value: vec![8],
                }]
            };
            let committed = store.commit(100, block(), b"next block")?;
            let expected_block = if phase == GenesisStage::IavlMutations {
                baseline.commit(100, block(), b"next block")?
            } else {
                baseline.head().unwrap()
            };
            assert_eq!(committed, expected_block);
            drop(store);
            let store = BlockStore::open(&path)?;
            assert_eq!(store.head(), Some(expected_block));
            assert_eq!(store.metadata()?.as_deref(), Some(b"next block".as_slice()));
        }
        Ok(())
    }

    #[test]
    fn snapshot_export_cancels_without_changing_the_pinned_state() -> Result<()> {
        use crate::{ExportStage, OperationCancelled};
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state");
        let mut store = BlockStore::open(&path)?;
        let head = store.initialize(
            1,
            (0u32..6000).map(|index| Mutation::Set {
                key: index.to_be_bytes().to_vec(),
                value: vec![7; 16],
            }),
            b"genesis",
        )?;
        let reader = store.reader().unwrap();
        let state = reader.latest()?;
        let control = OperationControl::default();
        control.cancel();
        let mut file = tempfile::tempfile()?;
        let error = state
            .write_go_snapshot_with_control(&[], &mut file, &control)
            .unwrap_err();
        assert!(OperationCancelled::caused(&error));
        assert_eq!(file.metadata()?.len(), 0);
        let control = OperationControl::default();
        let stop = control.clone();
        let control = control
            .with_maintenance_progress(|_, _| panic!("export must not call maintenance observers"))
            .with_export_progress(move |phase, done| {
                if phase == ExportStage::IavlNodes && done == 1000 {
                    stop.cancel();
                }
            });
        let error = state
            .write_go_snapshot_with_control(&[], &mut file, &control)
            .unwrap_err();
        assert!(OperationCancelled::caused(&error));
        assert_eq!(state.head(), head);
        assert_eq!(state.value(&99u32.to_be_bytes())?, Some(vec![7; 16]));
        let mut first = tempfile::tempfile()?;
        let mut second = tempfile::tempfile()?;
        assert_eq!(state.write_go_snapshot(&[], &mut first)?, 11999);
        assert_eq!(
            state.write_go_snapshot_with_control(&[], &mut second, &OperationControl::default())?,
            11999
        );
        use std::io::{Read, Seek, SeekFrom};
        let read = |mut file: File| -> Result<Vec<u8>> {
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        assert_eq!(read(first)?, read(second)?);
        assert_eq!(store.head(), Some(head));
        Ok(())
    }

    #[test]
    fn interrupted_history_upgrade_can_repeat_without_publishing_a_partial_index() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chain");
        let mut store = BlockStore::open(&path).unwrap();
        store.initialize(99, [], b"genesis").unwrap();
        store.set_keep_iavl_versions(2).unwrap();
        for height in 100..=104 {
            store
                .commit_native_block(
                    height,
                    [],
                    |_| Ok(b"cursor".to_vec()),
                    b"events",
                    NativeBlockWrite {
                        expected_root: None,
                        record: b"block",
                        validators: None,
                        committed_evidence: &[],
                        pending_evidence: None,
                    },
                )
                .unwrap();
        }
        // Match the schema of databases written before the index existed.
        let mut legacy = WriteBatch::default();
        legacy.delete(EVIDENCE_INDEX_START);
        write_sync(&store.db, legacy).unwrap();
        assert_eq!(store.native_block_base().unwrap(), Some(100));
        store
            .backfill_evidence_height(100, Some(b"first"), &[[7; 32]])
            .unwrap();
        drop(store);

        let store = BlockStore::open_existing(&path).unwrap();
        assert_eq!(store.evidence_index_start().unwrap(), None);
        assert!(store.value_at(100, b"any").is_err());
        store
            .backfill_evidence_height(100, Some(b"first"), &[[7; 32]])
            .unwrap();
        store
            .backfill_evidence_height(102, Some(b"second"), &[])
            .unwrap();
        assert_eq!(store.evidence_index_start().unwrap(), None);
        store.publish_evidence_index_start(100).unwrap();
        drop(store);

        let store = BlockStore::open_existing(&path).unwrap();
        assert_eq!(store.evidence_index_start().unwrap(), Some(100));
        assert_eq!(store.native_validators_at(99).unwrap(), None);
        assert_eq!(
            store.native_validators_at(101).unwrap().as_deref(),
            Some(b"first".as_slice())
        );
        assert_eq!(
            store.native_validators_at(104).unwrap().as_deref(),
            Some(b"second".as_slice())
        );
        assert!(store.evidence_committed(&[7; 32]).unwrap());
        assert!(store.backfill_evidence_height(105, None, &[]).is_err());
    }

    #[test]
    fn native_block_and_cursor_publish_only_at_the_expected_root() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        let mut store = BlockStore::open(&path).unwrap();
        let genesis = store
            .initialize(
                99,
                [Mutation::Set {
                    key: b"balance".to_vec(),
                    value: vec![1],
                }],
                b"genesis",
            )
            .unwrap();
        let block = NativeBlockWrite {
            expected_root: Some(genesis.app_hash),
            record: b"block",
            validators: Some(b"validators"),
            committed_evidence: &[[7; 32]],
            pending_evidence: Some(b"pending"),
        };
        let error = store
            .commit_native_block(
                100,
                [Mutation::Set {
                    key: b"balance".to_vec(),
                    value: vec![2],
                }],
                |_| panic!("inconsistent AppHash must not publish a cursor"),
                b"events",
                block,
            )
            .unwrap_err();
        assert!(error.to_string().contains("AppHash differs"));
        drop(store);

        let mut store = BlockStore::open_existing(&path).unwrap();
        assert_eq!(store.head(), Some(genesis));
        assert_eq!(
            store.metadata().unwrap().as_deref(),
            Some(b"genesis".as_slice())
        );
        assert_eq!(store.native_block_at(100).unwrap(), None);
        assert_eq!(store.evidence_index_start().unwrap(), None);
        assert!(!store.evidence_committed(&[7; 32]).unwrap());
        assert_eq!(store.pending_evidence().unwrap(), None);
        store
            .commit_native_block(
                100,
                [],
                |head| Ok(head.app_hash.to_vec()),
                b"events",
                NativeBlockWrite {
                    expected_root: None,
                    record: b"block",
                    validators: Some(b"validators"),
                    committed_evidence: &[[7; 32]],
                    pending_evidence: Some(b"pending"),
                },
            )
            .unwrap();
        drop(store);

        let store = BlockStore::open_existing(&path).unwrap();
        assert_eq!(store.head().unwrap().height, 100);
        assert_eq!(store.head().unwrap().app_hash, genesis.app_hash);
        assert_eq!(store.evidence_index_start().unwrap(), Some(100));
        assert_eq!(store.native_validators_at(99).unwrap(), None);
        assert_eq!(
            store.native_validators_at(101).unwrap().as_deref(),
            Some(b"validators".as_slice())
        );
        assert!(store.evidence_committed(&[7; 32]).unwrap());
        assert_eq!(
            store.pending_evidence().unwrap().as_deref(),
            Some(b"pending".as_slice())
        );
        assert_eq!(
            store.metadata().unwrap().as_deref(),
            Some(genesis.app_hash.as_slice())
        );
        assert_eq!(
            store.native_block_at(100).unwrap().as_deref(),
            Some(b"block".as_slice())
        );
    }
}
