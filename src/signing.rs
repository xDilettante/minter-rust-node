use super::write_sync;
use anyhow::{Context, Result};
use rocksdb::{DB, DBRecoveryMode, Options, WriteBatch};
use std::{fs, path::Path};

const STATE: &[u8] = b"minter/signing-state-v1";
const CONSENSUS: &[u8] = b"minter/consensus-lock-v1";

/// Private validator state with RocksDB's exclusive writer lock. Keep this
/// database separate from chain state, snapshots and pruning.
pub struct SigningJournal(DB);

impl SigningJournal {
    pub fn create(path: &Path, state: &[u8]) -> Result<()> {
        fs::create_dir(path).context("create a new signing journal directory")?;
        Self::connect(path, true)?.persist(state)
    }

    pub fn open(path: &Path) -> Result<(Self, Vec<u8>)> {
        let journal = Self::connect(path, false)?;
        let state = journal
            .0
            .get(STATE)?
            .context("signing journal is missing its state")?;
        Ok((journal, state))
    }

    pub fn persist(&self, state: &[u8]) -> Result<()> {
        self.persist_with_round(state, None)
    }

    pub fn consensus_round(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(CONSENSUS)?)
    }

    pub fn persist_with_round(&self, state: &[u8], round: Option<&[u8]>) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.put(STATE, state);
        if let Some(round) = round {
            batch.put(CONSENSUS, round);
        } else {
            batch.delete(CONSENSUS);
        }
        write_sync(&self.0, batch).context("sync private validator state")?;
        // RocksDB only tracks closed WALs in the manifest. Flush before
        // releasing the signature so loss of a live WAL cannot roll it back.
        self.0.flush().context("flush private validator state")
    }

    fn connect(path: &Path, create: bool) -> Result<Self> {
        // RocksDB's POSIX in-process lock table uses the literal path. On
        // Windows the filesystem lock already excludes aliases.
        #[cfg(unix)]
        let canonical = path
            .canonicalize()
            .context("resolve signing journal directory")?;
        #[cfg(unix)]
        let path = canonical.as_path();
        let mut options = Options::default();
        options.create_if_missing(create);
        options.set_error_if_exists(create);
        options.set_paranoid_checks(true);
        options.set_use_fsync(true);
        options.set_wal_recovery_mode(DBRecoveryMode::AbsoluteConsistency);
        options.set_track_and_verify_wals_in_manifest(true);
        Ok(Self(
            DB::open(&options, path).context("open signing journal")?,
        ))
    }
}
