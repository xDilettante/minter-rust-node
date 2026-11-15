mod blocks;
mod mutations;
mod operation;
mod proofs;
mod prune;
mod signing;
mod snapshot;

use anyhow::{Context, Result, ensure};
use iavl::{Content, Hash, MAX_HEIGHT, Node, node_key, root_key};
use minter_iavl as iavl;
use rocksdb::{
    BlockBasedOptions, Cache, DB, DBCompactionStyle, DBCompressionType, Options, Snapshot,
    WriteBatch, WriteOptions,
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    ops::Range,
    path::{Path, PathBuf},
};

pub use blocks::{
    BlockStore, HistoricalState, NativeBlockWrite, NativeCheckpointWrite, NativeContextWrite,
    RetentionPolicy, SnapshotView, StateReader,
};
pub use mutations::Mutation;
pub use operation::{
    ExportStage, GenesisStage, MaintenanceStage, OperationCancelled, OperationControl,
    OperationReader,
};
pub use proofs::StateProof;
pub use signing::SigningJournal;

const CHECKPOINT_KEY: &[u8] = b"\0minter/checkpoint-v1";
const IMPORT_KEY: &[u8] = b"\0minter/import-v1";
const IAVL_HEIGHT_KEY: &[u8] = b"\0minter/iavl-height-v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub format: u32,
    pub network: String,
    pub height: u64,
    #[serde(with = "hex::serde")]
    pub block_hash: [u8; 32],
    #[serde(with = "hex::serde")]
    pub app_hash: [u8; 32],
    #[serde(with = "hex::serde")]
    pub snapshot_sha256: [u8; 32],
    pub bytes: u64,
    pub nodes: u64,
}

pub struct StateStore {
    db: DB,
    checkpoint: Checkpoint,
    head: IavlHead,
    retention: prune::Retention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IavlHead {
    pub height: u64,
    #[serde(with = "hex::serde")]
    pub app_hash: Hash,
}

#[derive(Debug)]
pub struct UnavailableHeight {
    pub height: u64,
    pub first: u64,
    pub latest: u64,
}

impl std::fmt::Display for UnavailableHeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "block height {} is outside the stored range {}..={}",
            self.height, self.first, self.latest
        )
    }
}

impl std::error::Error for UnavailableHeight {}

pub struct IavlWriter(StateStore);

impl StateStore {
    /// Import a checkpoint authenticated by the caller. An interrupted import
    /// can be retried at the same path with the same trusted manifest.
    pub fn import(path: &Path, snapshot_path: &Path, trusted: &Checkpoint) -> Result<()> {
        validate_checkpoint(trusted)?;
        let file = snapshot::open_verified(snapshot_path, trusted)?;
        let create = match fs::create_dir(path) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::read_dir(path)?.next().transpose()?.is_none()
            }
            Err(error) => return Err(error).context("create state directory"),
        };
        let (db, _) = open_database(path, create).context("open RocksDB for checkpoint import")?;
        let manifest = serde_json::to_vec(trusted)?;
        if let Some(committed) = db.get(CHECKPOINT_KEY)? {
            ensure!(
                committed == manifest,
                "store already contains another checkpoint"
            );
            return Self::from_db(db).map(|_| ());
        }

        let retention = prune::Retention::pending(&db)?;
        begin_import(&db, &manifest)?;
        let snapshot = snapshot::load(&db, file, trusted)?;
        let mut batch = snapshot.into_batch(trusted.height);
        retention.stage_initial(&mut batch, trusted.height);
        finish_import(&db, batch, manifest)
    }

    /// Read a checkpoint database while its writer is stopped.
    pub fn open(path: &Path) -> Result<Self> {
        let db = DB::open_for_read_only(&options(false), path, false)
            .context("open checkpoint RocksDB")?;
        Self::from_db(db)
    }

    fn from_db(db: DB) -> Result<Self> {
        let manifest = db
            .get(CHECKPOINT_KEY)?
            .context("store has no committed checkpoint")?;
        let checkpoint: Checkpoint =
            serde_json::from_slice(&manifest).context("decode committed checkpoint")?;
        validate_checkpoint(&checkpoint)?;
        let mut head = IavlHead {
            height: checkpoint.height,
            app_hash: checkpoint.app_hash,
        };
        if let Some(height) = read_u64(&db, IAVL_HEIGHT_KEY)? {
            head.height = height;
            ensure!(
                head.height > checkpoint.height && head.height <= i64::MAX as u64,
                "invalid IAVL head height {}",
                head.height,
            );
            let root = read_root(&db, head.height)?;
            if let Some(hash) = root {
                verify_root(&db, head.height, hash)?;
            }
            head.app_hash = root.unwrap_or_else(iavl::empty_hash);
        }
        let retention = prune::Retention::load(&db, checkpoint.height, head.height)?;
        validate_committed(&db, &checkpoint, retention.first)?;
        Ok(Self {
            db,
            checkpoint,
            head,
            retention,
        })
    }

    pub fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    pub fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.value_at(self.checkpoint.height, key)
    }

    pub fn head(&self) -> IavlHead {
        self.head
    }

    pub fn earliest_height(&self) -> u64 {
        self.retention.first
    }

    pub fn root_at(&self, height: u64) -> Result<Hash> {
        Ok(self.version_root(height)?.unwrap_or_else(iavl::empty_hash))
    }

    pub fn value_at(&self, height: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        read_value(&self.db, self.version_root(height)?, height, key)
    }

    pub fn proof_at(&self, height: u64, key: &[u8]) -> Result<StateProof> {
        proofs::prove(&self.db, self.version_root(height)?, height, key)
    }

    fn version_root(&self, height: u64) -> Result<Option<Hash>> {
        check_height(height, self.earliest_height(), self.head.height)?;
        read_root(&self.db, height)
    }

    /// Read AppDB metadata from the imported Go checkpoint.
    pub fn app_metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(metadata_key(key))?)
    }
}

impl IavlWriter {
    pub fn open(path: &Path) -> Result<Self> {
        let (db, _) = open_database(path, false).context("open RocksDB for IAVL commits")?;
        ensure!(
            db.get(blocks::EXECUTION_KEY)?.is_none(),
            "IAVL trace replay cannot modify a store with execution metadata"
        );
        let store = StateStore::from_db(db)?;
        ensure!(
            store.retention.keep == 0,
            "IAVL trace replay requires archive retention"
        );
        Ok(Self(store))
    }

    pub fn checkpoint(&self) -> &Checkpoint {
        self.0.checkpoint()
    }

    pub fn head(&self) -> IavlHead {
        self.0.head()
    }

    /// Apply ordered IAVL mutations for the next height. Publish the new
    /// version only after the calculated root matches the expected AppHash.
    pub fn apply_block(
        &mut self,
        height: u64,
        operations: impl IntoIterator<Item = Mutation>,
        expected: Hash,
    ) -> Result<IavlHead> {
        let store = &mut self.0;
        ensure!(
            height == store.head.height + 1 && height <= i64::MAX as u64,
            "cannot apply IAVL height {height} after {}",
            store.head.height
        );
        let mut batch = WriteBatch::default();
        let actual = stage_iavl(
            &store.db,
            &mut batch,
            height,
            height,
            read_root(&store.db, store.head.height)?,
            operations,
            |_| {},
        )?;
        ensure!(
            actual == expected,
            "IAVL AppHash mismatch at {height}, expected {}, calculated {}",
            hex::encode(expected),
            hex::encode(actual),
        );
        write_sync(&store.db, batch)?;
        store.head = IavlHead {
            height,
            app_hash: actual,
        };
        Ok(store.head)
    }
}

fn validate_checkpoint(checkpoint: &Checkpoint) -> Result<()> {
    ensure!(
        checkpoint.format == 1,
        "unsupported Go snapshot format {}",
        checkpoint.format
    );
    ensure!(
        !checkpoint.network.is_empty(),
        "checkpoint network is empty"
    );
    ensure!(
        (1..=i64::MAX as u64).contains(&checkpoint.height),
        "invalid checkpoint height"
    );
    Ok(())
}

fn begin_import(db: &DB, manifest: &[u8]) -> Result<()> {
    if let Some(pending) = db.get(IMPORT_KEY)? {
        ensure!(
            pending == manifest,
            "store has an incomplete import for another checkpoint"
        );
        return Ok(());
    }
    ensure!(
        prune::uninitialized(db)?,
        "existing RocksDB does not contain a Minter checkpoint"
    );
    let mut intent = WriteBatch::default();
    intent.put(IMPORT_KEY, manifest);
    write_sync(db, intent)
}

fn finish_import(db: &DB, mut batch: WriteBatch, manifest: Vec<u8>) -> Result<()> {
    batch.put(CHECKPOINT_KEY, manifest);
    batch.delete(IMPORT_KEY);
    write_sync(db, batch)
}

fn validate_committed(db: &DB, checkpoint: &Checkpoint, first: u64) -> Result<()> {
    if first == checkpoint.height {
        let root = read_root(db, checkpoint.height)?;
        ensure!(
            root.is_none() == (checkpoint.nodes == 0)
                && root.unwrap_or_else(iavl::empty_hash) == checkpoint.app_hash,
            "committed root differs from checkpoint"
        );
        if let Some(hash) = root {
            verify_root(db, checkpoint.height, hash)?;
        }
    }
    ensure!(
        db.get(metadata_key("height"))?.as_deref() == Some(&checkpoint.height.to_be_bytes()),
        "stored AppDB height differs from the checkpoint"
    );
    ensure!(
        db.get(metadata_key("hash"))?.as_deref() == Some(&checkpoint.app_hash),
        "stored AppDB hash differs from the checkpoint"
    );
    Ok(())
}

trait KeyRead {
    fn get_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, rocksdb::Error>;
}

impl KeyRead for DB {
    fn get_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, rocksdb::Error> {
        self.get(key)
    }
}

impl KeyRead for Snapshot<'_> {
    fn get_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, rocksdb::Error> {
        self.get(key)
    }
}

fn read_u64(db: &impl KeyRead, key: &[u8]) -> Result<Option<u64>> {
    db.get_bytes(key)?
        .map(|bytes| {
            let bytes = bytes.as_slice().try_into().with_context(|| {
                format!("invalid RocksDB metadata {}", String::from_utf8_lossy(key))
            })?;
            Ok(u64::from_be_bytes(bytes))
        })
        .transpose()
}

fn check_height(height: u64, first: u64, head: u64) -> Result<()> {
    if !(first..=head).contains(&height) {
        return Err(UnavailableHeight {
            height,
            first,
            latest: head,
        }
        .into());
    }
    Ok(())
}

fn read_root(db: &impl KeyRead, height: u64) -> Result<Option<Hash>> {
    let bytes = db
        .get_bytes(&root_key(height))?
        .with_context(|| format!("missing IAVL root at height {height}"))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        bytes
            .as_slice()
            .try_into()
            .context("invalid IAVL root length")?,
    ))
}

fn verify_root(db: &DB, height: u64, hash: Hash) -> Result<()> {
    let raw = load_node(db, hash)?;
    let node = Node::decode(&raw)?;
    ensure!(
        node.version <= height && node.hash(&mut Vec::new()) == hash,
        "invalid IAVL root node at height {height}"
    );
    Ok(())
}

fn stage_iavl(
    db: &DB,
    batch: &mut WriteBatch,
    version: u64,
    node_version: u64,
    previous: Option<Hash>,
    operations: impl IntoIterator<Item = Mutation>,
    reused: impl FnMut(Hash),
) -> Result<Hash> {
    stage_iavl_with(db, batch, version, |db, batch| {
        mutations::apply(db, node_version, previous, operations, batch, reused)
    })
}

fn stage_iavl_with(
    db: &DB,
    batch: &mut WriteBatch,
    version: u64,
    apply: impl FnOnce(&DB, &mut WriteBatch) -> Result<Option<Hash>>,
) -> Result<Hash> {
    let key = root_key(version);
    ensure!(
        db.get(key)?.is_none(),
        "IAVL version {version} already exists, reopen the store"
    );
    let root = apply(db, batch)?;
    batch.put(key, root.as_ref().map_or(&[][..], |hash| hash));
    batch.put(IAVL_HEIGHT_KEY, version.to_be_bytes());
    Ok(root.unwrap_or_else(iavl::empty_hash))
}

fn read_value(
    db: &impl KeyRead,
    root: Option<Hash>,
    version: u64,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let Some(mut hash) = root else {
        return Ok(None);
    };
    let mut max_height = MAX_HEIGHT + 1;
    let mut scratch = Vec::new();
    loop {
        let raw = load_node(db, hash)?;
        let node = read_verified_node(&raw, hash, version, max_height, &mut scratch)?;
        match node.content {
            Content::Leaf(value) => return Ok((key == node.key).then(|| value.to_vec())),
            Content::Branch { left, right } => {
                max_height = node.height;
                hash = if key < node.key { left } else { right };
            }
        }
    }
}

fn read_first_key(
    db: &impl KeyRead,
    hash: Hash,
    version: u64,
    max_height: u8,
    lower_bound: Option<&[u8]>,
    scratch: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    let raw = load_node(db, hash)?;
    let node = read_verified_node(&raw, hash, version, max_height, scratch)?;
    match node.content {
        Content::Leaf(_) => Ok(lower_bound
            .is_none_or(|key| key <= node.key)
            .then(|| node.key.to_vec())),
        Content::Branch { left, right } => {
            if lower_bound.is_none_or(|key| key < node.key) {
                if let Some(key) =
                    read_first_key(db, left, version, node.height, lower_bound, scratch)?
                {
                    return Ok(Some(key));
                }
                return read_first_key(db, right, version, node.height, None, scratch);
            }
            read_first_key(db, right, version, node.height, lower_bound, scratch)
        }
    }
}

fn read_last_key(
    db: &impl KeyRead,
    hash: Hash,
    version: u64,
    max_height: u8,
    upper_bound: Option<&[u8]>,
    scratch: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    let raw = load_node(db, hash)?;
    let node = read_verified_node(&raw, hash, version, max_height, scratch)?;
    match node.content {
        Content::Leaf(_) => Ok(upper_bound
            .is_none_or(|key| node.key < key)
            .then(|| node.key.to_vec())),
        Content::Branch { left, right } => {
            if upper_bound.is_none_or(|key| node.key < key) {
                if let Some(key) =
                    read_last_key(db, right, version, node.height, upper_bound, scratch)?
                {
                    return Ok(Some(key));
                }
                return read_last_key(db, left, version, node.height, None, scratch);
            }
            read_last_key(db, left, version, node.height, upper_bound, scratch)
        }
    }
}

fn read_range(
    db: &impl KeyRead,
    hash: Hash,
    version: u64,
    max_height: u8,
    keys: &Range<&[u8]>,
    scratch: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let raw = load_node(db, hash)?;
    let node = read_verified_node(&raw, hash, version, max_height, scratch)?;
    match node.content {
        Content::Leaf(value) if keys.contains(&node.key) => visit(node.key, value)?,
        Content::Leaf(_) => {}
        Content::Branch { left, right } => {
            if keys.start < node.key {
                read_range(db, left, version, node.height, keys, scratch, visit)?;
            }
            if node.key < keys.end {
                read_range(db, right, version, node.height, keys, scratch, visit)?;
            }
        }
    }
    Ok(())
}

fn read_keys<K: AsRef<[u8]>>(
    db: &DB,
    hash: Hash,
    version: u64,
    max_height: u8,
    keys: &[K],
    scratch: &mut Vec<u8>,
    visit: &mut impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let raw = load_node(db, hash)?;
    let node = read_verified_node(&raw, hash, version, max_height, scratch)?;
    match node.content {
        Content::Leaf(value) => {
            if keys
                .binary_search_by(|key| key.as_ref().cmp(node.key))
                .is_ok()
            {
                visit(node.key, value)?;
            }
        }
        Content::Branch { left, right } => {
            let split = keys.partition_point(|key| key.as_ref() < node.key);
            let (left_keys, right_keys) = keys.split_at(split);
            if !left_keys.is_empty() {
                read_keys(db, left, version, node.height, left_keys, scratch, visit)?;
            }
            if !right_keys.is_empty() {
                read_keys(db, right, version, node.height, right_keys, scratch, visit)?;
            }
        }
    }
    Ok(())
}

fn read_verified_node<'a>(
    raw: &'a [u8],
    hash: Hash,
    version: u64,
    max_height: u8,
    scratch: &mut Vec<u8>,
) -> Result<Node<'a>> {
    let node = Node::decode(raw)?;
    ensure!(
        node.height < max_height && node.version <= version,
        "invalid IAVL child metadata"
    );
    ensure!(
        node.hash(scratch) == hash,
        "IAVL node hash differs from its storage key"
    );
    Ok(node)
}

// POSIX in-process file locks are indexed by their literal path in RocksDB.
// Resolve aliases before opening any writer, not after acquiring its lock.
pub(crate) fn open_database(path: &Path, create: bool) -> Result<(DB, PathBuf)> {
    if create {
        fs::create_dir_all(path).context("create RocksDB directory")?;
    }
    let directory = fs::canonicalize(path).context("resolve RocksDB directory")?;
    #[cfg(unix)]
    let opened_path = directory.as_path();
    // Windows already excludes aliases with its filesystem lock. RocksDB adds
    // forward slashes, so preserve normal Win32 spelling instead of a verbatim path.
    #[cfg(not(unix))]
    let opened_path = path;
    let db = DB::open(&options(create), opened_path).context("open RocksDB")?;
    Ok((db, directory))
}

fn options(create: bool) -> Options {
    let mut options = Options::default();
    options.create_if_missing(create);
    options.set_compression_type(DBCompressionType::Lz4);
    options.set_compaction_style(DBCompactionStyle::Level);
    options.set_max_background_jobs(4);
    options.set_write_buffer_size(32 * 1024 * 1024);
    options.set_max_write_buffer_number(2);
    let mut table = BlockBasedOptions::default();
    table.set_block_cache(&Cache::new_lru_cache(256 * 1024 * 1024));
    options.set_block_based_table_factory(&table);
    options
}

pub(crate) fn metadata_key(name: &str) -> String {
    format!("app/{name}")
}

pub(crate) fn write_sync(db: &DB, batch: WriteBatch) -> Result<()> {
    let mut options = WriteOptions::default();
    options.disable_wal(false);
    options.set_sync(true);
    db.write_opt(batch, &options).context("write RocksDB batch")
}

fn load_node(db: &impl KeyRead, hash: Hash) -> Result<Vec<u8>> {
    db.get_bytes(&node_key(hash))?
        .with_context(|| format!("missing IAVL node {}", hex::encode(hash)))
}
