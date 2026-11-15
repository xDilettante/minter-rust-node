use anyhow::Result;
use std::{
    fmt,
    io::{self, Read, Seek, SeekFrom},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

type SnapshotProgress = dyn Fn(u64, u64) + Send + Sync;
type MaintenanceProgress = dyn Fn(MaintenanceStage, u64) + Send + Sync;
type ExportProgress = dyn Fn(ExportStage, u64) + Send + Sync;
type GenesisProgress = dyn Fn(GenesisStage, u64) + Send + Sync;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GenesisStage {
    FileBytes,
    FileHashBytes,
    AppStateHashBytes,
    EnvelopeBytes,
    AppStateBytes,
    CanonicalHashBytes,
    Accounts,
    Coins,
    Candidates,
    FrozenFunds,
    Waitlist,
    Pools,
    IavlMutations,
    IavlNodes,
}

impl fmt::Display for GenesisStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FileBytes => "file-bytes",
            Self::FileHashBytes => "file-hash-bytes",
            Self::AppStateHashBytes => "app-state-hash-bytes",
            Self::EnvelopeBytes => "envelope-bytes",
            Self::AppStateBytes => "app-state-bytes",
            Self::CanonicalHashBytes => "canonical-hash-bytes",
            Self::Accounts => "accounts",
            Self::Coins => "coins",
            Self::Candidates => "candidates",
            Self::FrozenFunds => "frozen-funds",
            Self::Waitlist => "waitlist",
            Self::Pools => "pools",
            Self::IavlMutations => "iavl-mutations",
            Self::IavlNodes => "iavl-nodes",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintenanceStage {
    IavlIndexNodes,
    IavlOrphans,
    CompactRecords,
    NativeBlocks,
    NativeIndexBlocks,
    MembershipRecords,
    MembershipSets,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExportStage {
    IavlNodes,
    HistoryRecords,
    HistoryScan,
    SnapshotHashBytes,
    HistoryHashBytes,
    AnchorHashBytes,
}

impl fmt::Display for MaintenanceStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IavlIndexNodes => "iavl-index-nodes",
            Self::IavlOrphans => "iavl-orphans",
            Self::CompactRecords => "compact-records",
            Self::NativeBlocks => "native-blocks",
            Self::NativeIndexBlocks => "native-index-blocks",
            Self::MembershipRecords => "membership-records",
            Self::MembershipSets => "membership-sets",
        })
    }
}

impl fmt::Display for ExportStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IavlNodes => "snapshot-export-nodes",
            Self::HistoryRecords => "history-export-records",
            Self::HistoryScan => "history-export-scan",
            Self::SnapshotHashBytes => "snapshot-hash-bytes",
            Self::HistoryHashBytes => "history-hash-bytes",
            Self::AnchorHashBytes => "anchor-hash-bytes",
        })
    }
}

/// Cooperative cancellation for preparation/maintenance, independent of live commits.
#[derive(Clone, Default)]
pub struct OperationControl {
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<SnapshotProgress>>,
    maintenance: Option<Arc<MaintenanceProgress>>,
    export: Option<Arc<ExportProgress>>,
    genesis: Option<Arc<GenesisProgress>>,
}

#[derive(Debug)]
pub struct OperationCancelled;

impl fmt::Display for OperationCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation cancelled")
    }
}

impl std::error::Error for OperationCancelled {}

impl OperationCancelled {
    pub fn caused(error: &anyhow::Error) -> bool {
        error.chain().any(|mut cause| {
            if cause.is::<Self>() {
                return true;
            }
            // io::Error::source can skip the wrapped error itself. Inspect its
            // typed payload as well, including nested I/O wrappers.
            while let Some(inner) = cause
                .downcast_ref::<io::Error>()
                .and_then(io::Error::get_ref)
            {
                if inner.is::<Self>() {
                    return true;
                }
                cause = inner;
            }
            false
        })
    }
}

impl OperationControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Share the same request with existing blocking P2P loops.
    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(OperationCancelled.into());
        }
        Ok(())
    }

    /// Report processed IAVL nodes, not publication or acceptance of the snapshot.
    pub fn with_snapshot_progress(
        mut self,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> Self {
        self.progress = Some(Arc::new(progress));
        self
    }

    pub fn reader<R>(&self, reader: R) -> OperationReader<R> {
        OperationReader {
            reader,
            control: self.clone(),
        }
    }

    /// Counts processed elements in one phase, not completion or publication.
    pub fn with_maintenance_progress(
        mut self,
        progress: impl Fn(MaintenanceStage, u64) + Send + Sync + 'static,
    ) -> Self {
        self.maintenance = Some(Arc::new(progress));
        self
    }

    pub fn maintenance_progress(&self, phase: MaintenanceStage, done: u64) -> Result<()> {
        self.check()?;
        if let Some(progress) = &self.maintenance {
            progress(phase, done);
        }
        self.check()
    }

    /// Export progress is independent of existing maintenance callbacks.
    pub fn with_export_progress(
        mut self,
        progress: impl Fn(ExportStage, u64) + Send + Sync + 'static,
    ) -> Self {
        self.export = Some(Arc::new(progress));
        self
    }

    pub fn export_progress(&self, phase: ExportStage, done: u64) -> Result<()> {
        self.check()?;
        if let Some(progress) = &self.export {
            progress(phase, done);
        }
        self.check()
    }

    pub fn with_genesis_progress(
        mut self,
        progress: impl Fn(GenesisStage, u64) + Send + Sync + 'static,
    ) -> Self {
        self.genesis = Some(Arc::new(progress));
        self
    }

    pub fn genesis_progress(&self, phase: GenesisStage, done: u64) -> Result<()> {
        self.check()?;
        if let Some(progress) = &self.genesis {
            progress(phase, done);
        }
        self.check()
    }

    pub(crate) fn snapshot_progress(&self, done: u64, total: u64) -> Result<()> {
        self.check()?;
        if let Some(progress) = &self.progress {
            progress(done, total);
        }
        self.check()
    }

    fn check_io(&self) -> io::Result<()> {
        if self.is_cancelled() {
            return Err(io::Error::other(OperationCancelled));
        }
        Ok(())
    }
}

pub struct OperationReader<R> {
    reader: R,
    control: OperationControl,
}

impl<R: Read> Read for OperationReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.control.check_io()?;
        self.reader.read(buffer)
    }
}

impl<R: Seek> Seek for OperationReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.control.check_io()?;
        self.reader.seek(position)
    }
}
