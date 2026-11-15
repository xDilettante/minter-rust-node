use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use minter_app::native_sync::{CheckpointSource, NativeNode, NativeSync};
use minter_p2p::ReceivedBlock;
use minter_p2p::server::BlockServer;
use minter_state::OperationControl;
use serde_json::json;
use std::{
    collections::VecDeque, fs::File, io::BufReader, net::SocketAddr, path::PathBuf, time::Duration,
};

#[path = "native_sync/peers.rs"]
mod peers;
#[path = "native_sync/serve.rs"]
mod serve;
use super::shutdown;
use peers::Peers;

#[derive(Clone, Copy, ValueEnum)]
enum StorageProfile {
    Full,
    Archive,
}

#[derive(Args)]
pub(super) struct Options {
    #[arg(long)]
    db: PathBuf,
    /// Tendermint genesis for the first run, or the trusted genesis of a custom checkpoint.
    #[arg(long)]
    genesis: Option<PathBuf>,
    /// Restore a Go snapshot and its Tendermint state from this directory.
    #[arg(long, requires = "checkpoint_sha256")]
    checkpoint: Option<PathBuf>,
    /// SHA-256 of the checkpoint manifest obtained from a trusted source.
    #[arg(long, requires = "checkpoint", value_parser = parse_sha256)]
    checkpoint_sha256: Option<[u8; 32]>,
    /// Download the trusted checkpoint's snapshot through Tendermint State Sync.
    #[arg(long, requires = "checkpoint", value_name = "ID@IP:PORT")]
    snapshot_peer: Vec<minter_p2p::Endpoint>,
    #[arg(long, default_value_t = 1)]
    transaction_chain_id: u8,
    /// Fetch from an authenticated Tendermint peer. Repeat to add fallbacks.
    #[arg(long, value_name = "ID@IP:PORT", conflicts_with = "blocks")]
    peer: Vec<minter_p2p::Endpoint>,
    /// Read length-delimited BlockResponses from these files in order.
    #[arg(long, value_name = "FILE")]
    blocks: Vec<PathBuf>,
    /// Stop after this height. Peer mode defaults to the available height minus two.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    through: Option<u64>,
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_secs: u64,
    /// Retry when peers are unavailable and follow new blocks.
    #[arg(long, requires = "peer")]
    follow: bool,
    /// Apply and persist all storage limits: full = 120 IAVL versions and
    /// 100000 blocks/events; archive = keep all future data.
    #[arg(long, value_enum, conflicts_with_all = ["keep_iavl_versions", "keep_event_blocks", "keep_native_blocks"])]
    storage_profile: Option<StorageProfile>,
    /// Keep this many IAVL versions, including the head. Zero keeps every version.
    #[arg(long)]
    keep_iavl_versions: Option<u64>,
    /// Keep this many event batches. Zero retains all future batches.
    #[arg(long)]
    keep_event_blocks: Option<u64>,
    /// Persist the number of full native blocks to retain. Zero keeps every body;
    /// at least two are required otherwise. Needed older headers are retained separately.
    #[arg(long, value_parser = parse_native_retention)]
    keep_native_blocks: Option<u64>,
    /// Print a verification record for every committed block.
    #[arg(long)]
    trace: bool,
    /// Serve committed blocks over Tendermint 0.34 P2P.
    #[arg(long)]
    p2p_listen: Option<SocketAddr>,
    /// Public socket address to include in Tendermint NodeInfo.
    #[arg(long, requires = "p2p_listen", value_parser = parse_peer_address)]
    p2p_advertise: Option<SocketAddr>,
    /// Tendermint node_key.json to load or create. Defaults to a file in --db.
    #[arg(long, requires = "p2p_listen")]
    node_key: Option<PathBuf>,
    /// Serve a verified snapshot from the specified directory.
    #[arg(long, requires = "p2p_listen")]
    serve_snapshot: Option<PathBuf>,
    /// Accept and relay validated transactions over Tendermint 0.34.
    #[arg(long, requires = "p2p_listen")]
    mempool: bool,
    /// Follow live Tendermint rounds and execute blocks from a signed precommit quorum.
    #[arg(long, requires = "p2p_listen", conflicts_with_all = ["blocks", "peer", "through", "mempool_peer"])]
    consensus: bool,
    /// Dial a consensus peer by ID and address. Repeat to maintain multiple connections.
    #[arg(long, requires = "consensus", value_name = "ID@IP:PORT")]
    consensus_peer: Vec<minter_p2p::Endpoint>,
    /// Validator key for the local consensus reactor.
    #[arg(long, requires_all = ["consensus", "validator_journal"])]
    validator_key: Option<PathBuf>,
    /// RocksDB signing journal previously imported with validator-signer import.
    #[arg(long, requires_all = ["consensus", "validator_key"])]
    validator_journal: Option<PathBuf>,
    /// Dial a transaction gossip peer by ID and address. Repeat to add peers.
    #[arg(long, requires = "mempool", value_name = "ID@IP:PORT")]
    mempool_peer: Vec<minter_p2p::Endpoint>,
    /// Enable the native Minter v2 HTTP API.
    #[arg(long, requires = "p2p_listen")]
    api_listen: Option<SocketAddr>,
    /// Enable the native Minter v2 gRPC API.
    #[arg(long, requires = "p2p_listen")]
    grpc_listen: Option<SocketAddr>,
    /// Shut down gracefully when the parent process closes standard input.
    #[arg(long, requires = "p2p_listen")]
    shutdown_on_stdin_close: bool,
}

#[derive(Args)]
pub(super) struct ExportOptions {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    output: PathBuf,
    /// Package the snapshot with three captured blocks at H, H+1 and H+2 for native State Sync.
    #[arg(long, value_name = "FILE")]
    anchor_blocks: Option<PathBuf>,
    /// Include the complete recent history needed to verify events preceding the snapshot.
    #[arg(long, requires = "anchor_blocks")]
    with_history_context: bool,
    #[arg(long, default_value_t = 1)]
    transaction_chain_id: u8,
    /// Stop cooperatively when the supervising process closes stdin.
    #[arg(long)]
    shutdown_on_stdin_close: bool,
}

pub(super) fn export_control() -> OperationControl {
    OperationControl::default().with_export_progress(|phase, done| {
        if done.is_multiple_of(1000) {
            eprintln!("Export progress: {phase} {done}");
        }
    })
}

pub(super) async fn export(options: ExportOptions) -> Result<()> {
    let control = export_control();
    let mut signals = shutdown::start(options.shutdown_on_stdin_close, &control).await?;
    let worker_control = control.clone();
    let result = tokio::task::spawn_blocking(move || export_files(options, &worker_control))
        .await
        .context("native snapshot export worker")
        .and_then(|result| result);
    shutdown::finish(result, &control, &mut signals).await
}

fn export_files(options: ExportOptions, control: &OperationControl) -> Result<()> {
    let sync = NativeSync::open_with_control(
        &options.db,
        options.transaction_chain_id,
        None,
        None,
        control,
    )?;
    let anchor = options
        .anchor_blocks
        .map(|path| {
            let mut input = BufReader::new(
                control.reader(
                    File::open(&path)
                        .with_context(|| format!("open snapshot anchor {}", path.display()))?,
                ),
            );
            let mut block =
                || ReceivedBlock::read(&mut input)?.context("missing snapshot anchor block");
            let blocks = [block()?, block()?, block()?];
            ensure!(
                ReceivedBlock::read(&mut input)?.is_none(),
                "extra snapshot anchor block"
            );
            Ok::<_, anyhow::Error>(blocks)
        })
        .transpose()?;
    let exported = if options.with_history_context {
        sync.export_snapshot_with_history(
            &options.output,
            anchor.context("history needs snapshot anchor blocks")?,
        )?
    } else {
        sync.export_go_snapshot(&options.output, anchor)?
    };
    let mut report = serde_json::to_value(&exported.snapshot)?;
    if let Some(hash) = exported.checkpoint_sha256 {
        report["checkpointSha256"] = json!(hex::encode(hash));
    }
    if let Some(context) = exported.history_context {
        report["historyContext"] = serde_json::to_value(context)?;
    }
    println!("{report}");
    Ok(())
}

pub(super) async fn run(options: Options) -> Result<()> {
    let control = OperationControl::default()
        .with_genesis_progress(|phase, done| {
            if done == 0 || done.is_multiple_of(1000) || done.is_power_of_two() {
                eprintln!("Genesis preparation: {phase} {done}");
            }
        })
        .with_snapshot_progress(|done, total| {
            eprintln!("Importing snapshot: {done}/{total} IAVL nodes");
        })
        .with_maintenance_progress(|phase, done| {
            if done.is_multiple_of(1000) {
                eprintln!("Native maintenance: {phase} {done}");
            }
        });
    let mut signals = shutdown::start(options.shutdown_on_stdin_close, &control).await?;
    let result = async {
        control.check()?;
        if let Some(address) = options.p2p_listen {
            return serve::run(options, address, &control, &mut signals).await;
        }
        let preparing = control.clone();
        let (options, node) = tokio::task::spawn_blocking(move || {
            let mut sync = open(&options, &preparing)?;
            preparing.check()?;
            sync.maintain_native_history()?;
            preparing.check()?;
            Ok::<_, anyhow::Error>((options, NativeNode::new(sync)))
        })
        .await
        .context("prepare native sync worker")??;
        let stopping = control.clone();
        tokio::task::spawn_blocking(move || {
            advance(options, &node, None, || stopping.is_cancelled())
        })
        .await
        .context("native sync worker")?
    }
    .await;
    shutdown::finish(result, &control, &mut signals).await
}

fn open(options: &Options, control: &OperationControl) -> Result<NativeSync> {
    control.check()?;
    ensure!(
        !options.peer.is_empty() || !options.blocks.is_empty() || options.p2p_listen.is_some(),
        "provide --peer, --blocks or --p2p-listen"
    );
    ensure!(
        options.blocks.is_empty() || options.through.is_some(),
        "provide --through with --blocks"
    );
    ensure!(
        options
            .through
            .is_none_or(|height| height <= i64::MAX as u64 - 2),
        "target exceeds the Tendermint height range"
    );
    let mut sync = if let Some(checkpoint) = &options.checkpoint {
        let digest = options
            .checkpoint_sha256
            .context("missing checkpoint SHA-256")?;
        NativeSync::open_checkpoint_with_control(
            &options.db,
            options.transaction_chain_id,
            CheckpointSource {
                package: checkpoint,
                trusted_sha256: digest,
                genesis: options.genesis.as_deref(),
                peers: (!options.snapshot_peer.is_empty()).then_some((
                    options.snapshot_peer.as_slice(),
                    Duration::from_secs(options.timeout_secs),
                )),
            },
            None,
            control,
        )
    } else {
        NativeSync::open_with_control(
            &options.db,
            options.transaction_chain_id,
            options.genesis.as_deref(),
            None,
            control,
        )
    }?;
    control.check()?;
    use minter_state::RetentionPolicy;
    if let Some(profile) = options.storage_profile {
        sync.configure_retention(match profile {
            StorageProfile::Full => RetentionPolicy::FULL,
            StorageProfile::Archive => RetentionPolicy::ARCHIVE,
        })?;
    } else if options.keep_iavl_versions.is_some()
        || options.keep_event_blocks.is_some()
        || options.keep_native_blocks.is_some()
    {
        let mut policy = sync.retention_policy()?;
        if let Some(keep) = options.keep_iavl_versions {
            policy.keep_iavl_versions = keep;
        }
        if let Some(keep) = options.keep_event_blocks {
            policy.keep_event_blocks = keep;
        }
        if let Some(keep) = options.keep_native_blocks {
            policy.keep_native_blocks = keep;
        }
        sync.configure_retention(policy)?;
    }
    control.check()?;
    Ok(sync)
}

fn parse_native_retention(raw: &str) -> std::result::Result<u64, String> {
    match raw.parse::<u64>() {
        Ok(1) => Err("retain zero or at least two native blocks".to_owned()),
        Ok(value) => Ok(value),
        Err(_) => Err("expected a nonnegative block count".to_owned()),
    }
}

fn advance(
    options: Options,
    node: &NativeNode,
    server: Option<&BlockServer>,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    let sync = node.lock()?;
    let head = sync.head()?;
    let chain = sync.chain_id().clone();
    drop(sync);
    if options.through.is_some_and(|height| height <= head.height) {
        println!(
            "{}",
            json!({ "through": head.height, "appHash": hex::encode(head.app_hash), "blocks": 0 })
        );
        return Ok(());
    }
    let start = head.height + 1;
    let trace = options.trace;
    if !options.blocks.is_empty() {
        let through = options.through.context("missing archive target")?;
        print_start(&chain, head, Some(through));
        let mut archive = Archive {
            paths: options.blocks.into(),
            reader: None,
            last_height: None,
        };
        let mut first = archive.block(start)?;
        let mut next = archive.block(start + 1)?;
        for height in start..=through {
            if stopping() {
                break;
            }
            let witness = archive.block(height + 2)?;
            let result = node
                .lock()?
                .apply(&first, &next, &witness)
                .with_context(|| format!("apply native block {height}"))?;
            report(&result, trace || height == through)?;
            first = next;
            next = witness;
        }
    } else if !options.peer.is_empty() {
        let combined = options
            .mempool_peer
            .into_iter()
            .map(|peer| peer.id)
            .collect();
        let mut peers = Peers::new(
            options.peer,
            combined,
            server,
            chain.clone(),
            Duration::from_secs(options.timeout_secs),
        );
        let through = if options.through.is_none() && !options.follow {
            Some(peers.available(start, node)?)
        } else {
            options.through
        };
        print_start(&chain, head, through);
        peers.run(node, start, through, options.follow, trace, stopping)?;
    }
    let final_head = node.lock()?.head()?;
    println!(
        "{}",
        json!({
            "through": final_head.height,
            "appHash": hex::encode(final_head.app_hash),
            "blocks": final_head.height - head.height,
        })
    );
    Ok(())
}

fn parse_peer_address(input: &str) -> Result<SocketAddr> {
    let address: SocketAddr = input.parse()?;
    ensure!(
        !address.ip().is_unspecified() && address.port() != 0,
        "advertised P2P address must specify an IP and port"
    );
    Ok(address)
}

fn print_start(chain: &tendermint::chain::Id, head: minter_state::IavlHead, through: Option<u64>) {
    println!(
        "{}",
        json!({
            "chainId": chain.as_str(),
            "startingHeight": head.height,
            "startingAppHash": hex::encode(head.app_hash),
            "target": through,
        })
    );
}

fn report(result: &minter_app::native_sync::AppliedBlock, trace: bool) -> Result<()> {
    if trace || result.height.is_multiple_of(1000) {
        println!("{}", serde_json::to_string(result)?);
    }
    Ok(())
}

pub(super) fn parse_sha256(raw: &str) -> Result<[u8; 32]> {
    let mut hash = [0; 32];
    hex::decode_to_slice(raw, &mut hash).context("expected a 64-digit hexadecimal SHA-256")?;
    Ok(hash)
}

struct Archive {
    paths: VecDeque<PathBuf>,
    reader: Option<BufReader<File>>,
    last_height: Option<u64>,
}

impl Archive {
    fn block(&mut self, target: u64) -> Result<ReceivedBlock> {
        loop {
            if self.reader.is_none() {
                let Some(path) = self.paths.pop_front() else {
                    bail!("archive ended before height {target}");
                };
                self.reader =
                    Some(BufReader::new(File::open(&path).with_context(|| {
                        format!("open block shard {}", path.display())
                    })?));
            }
            let reader = self.reader.as_mut().context("missing archive reader")?;
            let Some(block) = ReceivedBlock::read(reader)? else {
                self.reader = None;
                continue;
            };
            let height = block.block().header.height.value();
            ensure!(
                self.last_height
                    .is_none_or(|last| last.checked_add(1) == Some(height)),
                "archive has a gap or duplicate at height {height}"
            );
            self.last_height = Some(height);
            if height >= target {
                ensure!(height == target, "archive is missing height {target}");
                return Ok(block);
            }
        }
    }
}
