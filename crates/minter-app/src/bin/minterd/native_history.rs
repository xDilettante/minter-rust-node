use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use minter_app::native_sync::{CheckpointSource, NativeSync};
use minter_p2p::server::BlockSource;
use minter_state::OperationControl;
use serde_json::json;
use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
};

#[derive(Args)]
pub(super) struct Store {
    #[arg(long)]
    db: PathBuf,
    #[arg(long, default_value_t = 1)]
    transaction_chain_id: u8,
    /// Original trusted package, required once for a legacy database without its index.
    #[arg(long, requires = "checkpoint_sha256")]
    checkpoint: Option<PathBuf>,
    #[arg(long, requires = "checkpoint", value_parser = super::native_sync::parse_sha256)]
    checkpoint_sha256: Option<[u8; 32]>,
}

impl Store {
    fn open(self) -> Result<NativeSync> {
        self.open_with_control(&OperationControl::default())
    }

    fn open_with_control(self, control: &OperationControl) -> Result<NativeSync> {
        if let Some(package) = self.checkpoint {
            NativeSync::open_checkpoint_with_control(
                &self.db,
                self.transaction_chain_id,
                CheckpointSource {
                    package: &package,
                    trusted_sha256: self
                        .checkpoint_sha256
                        .context("missing checkpoint SHA-256")?,
                    genesis: None,
                    peers: None,
                },
                None,
                control,
            )
        } else {
            NativeSync::open_with_control(&self.db, self.transaction_chain_id, None, None, control)
        }
    }
}

#[derive(Subcommand)]
pub(super) enum Command {
    /// Export a contiguous range of signed headers, memberships and included proofs.
    Export {
        #[command(flatten)]
        store: Store,
        /// Snapshot height whose LastBlockID anchors the preceding range.
        #[arg(long)]
        anchor: u64,
        /// Oldest included height.
        #[arg(long)]
        from: u64,
        #[arg(long)]
        output: PathBuf,
        /// Stop cooperatively when the supervising process closes stdin.
        #[arg(long)]
        shutdown_on_stdin_close: bool,
    },
    /// Authenticate and add earlier context to a stopped snapshot-restored node.
    Import {
        #[command(flatten)]
        store: Store,
        #[arg(long)]
        input: PathBuf,
    },
    /// Show the verified context floor, served archive and application head.
    Status {
        #[command(flatten)]
        store: Store,
    },
    /// Apply the persisted native-block policy and collect unused compact memberships.
    Maintain {
        #[command(flatten)]
        store: Store,
        /// Reclaim RocksDB files after logical cleanup.
        #[arg(long)]
        compact: bool,
        /// Stop cooperatively when the supervising process closes stdin.
        #[arg(long)]
        shutdown_on_stdin_close: bool,
    },
}

pub(super) async fn run(command: Command) -> Result<()> {
    match command {
        Command::Export {
            store,
            anchor,
            from,
            output,
            shutdown_on_stdin_close,
        } => {
            let control = super::native_sync::export_control();
            let mut signals = super::shutdown::start(shutdown_on_stdin_close, &control).await?;
            let worker_control = control.clone();
            let result = tokio::task::spawn_blocking(move || {
                let node = store.open_with_control(&worker_control)?;
                anyhow::ensure!(!output.try_exists()?, "history destination already exists");
                let parent = output
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                let report = node.export_history_context(anchor, from, file.as_file_mut())?;
                file.as_file().sync_all()?;
                worker_control.check()?;
                file.persist_noclobber(output)
                    .context("publish history context")?;
                println!("{}", serde_json::to_string(&report)?);
                Ok(())
            })
            .await
            .context("native history export worker")
            .and_then(|result| result);
            return super::shutdown::finish(result, &control, &mut signals).await;
        }
        Command::Import { store, input } => {
            let mut node = store.open()?;
            let mut input = BufReader::new(File::open(input)?);
            let report = node.import_history_context(&mut input)?;
            drop(input);
            node.cleanup_history_downloads();
            println!("{}", serde_json::to_string(&report)?);
        }
        Command::Status { store } => {
            let node = store.open()?;
            let blocks = node.blocks()?.range()?;
            let policy = node.retention_policy()?;
            let iavl = node.iavl_range()?;
            let events = node.event_block_range()?;
            let mut status = json!({"head": node.head()?, "contextFrom": node.history_context_floor()?,
                "archiveFrom": blocks.base, "archiveThrough": blocks.height,
                "keepNativeBlocks": policy.keep_native_blocks,
                "storageProfile": policy.profile_name(), "retention": policy,
                "iavlFrom": iavl.0, "iavlThrough": iavl.1,
                "eventsFrom": events.map(|range| range.0), "eventsThrough": events.map(|range| range.1)});
            if let Some(required) = node.history_context_requirement()? {
                status["requiredHistoryContext"] = serde_json::to_value(required)?;
                status["requiredHistoryReady"] = json!(node.history_context_ready()?);
            }
            println!("{status}");
        }
        Command::Maintain {
            store,
            compact,
            shutdown_on_stdin_close,
        } => return maintain(store, compact, shutdown_on_stdin_close).await,
    }
    Ok(())
}

async fn maintain(store: Store, compact: bool, on_stdin_close: bool) -> Result<()> {
    let control = OperationControl::default().with_maintenance_progress(|phase, done| {
        if done.is_multiple_of(1000) {
            eprintln!("Native maintenance: {phase} {done}");
        }
    });
    let mut signals = super::shutdown::start(on_stdin_close, &control).await?;
    let worker_control = control.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut node = store.open_with_control(&worker_control)?;
        let mut report = serde_json::to_value(node.maintain_native_history()?)?;
        if compact {
            worker_control.check()?;
            node.compact_storage()?;
            report["compacted"] = json!(true);
        }
        println!("{report}");
        Ok(())
    })
    .await
    .context("native maintenance worker")
    .and_then(|result| result);
    super::shutdown::finish(result, &control, &mut signals).await
}
