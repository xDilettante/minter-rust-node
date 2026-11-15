use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use flate2::read::MultiGzDecoder;
use minter_state::{BlockStore, Checkpoint, IavlHead, IavlWriter, Mutation, StateStore};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Lines},
    path::{Path, PathBuf},
    time::Instant,
};

#[path = "minterctl/node.rs"]
mod node;
#[path = "minterctl/peer.rs"]
mod peer;

type Hash = [u8; 32];

#[derive(Parser)]
#[command(about = "Inspect and verify Minter node data")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run and inspect a node using the same configuration as the desktop app.
    Node {
        #[command(subcommand)]
        action: node::Action,
    },
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
    },
    Iavl {
        #[command(subcommand)]
        action: IavlAction,
    },
    Light {
        #[command(subcommand)]
        action: LightAction,
    },
    /// Connect to a Tendermint peer over native Rust P2P.
    Peer {
        #[command(subcommand)]
        action: peer::Action,
    },
}

#[derive(Subcommand)]
enum LightAction {
    /// Authenticate the configured checkpoint on two Tendermint RPC nodes.
    Init {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        anchor: PathBuf,
        #[arg(long)]
        primary: String,
        #[arg(long)]
        witness: String,
    },
    /// Verify signed headers up to the requested height or the common RPC head.
    Sync {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        primary: String,
        #[arg(long)]
        witness: String,
        #[arg(long)]
        height: Option<u64>,
    },
    /// Show the latest verified header and whether its trust period expired.
    Status {
        #[arg(long)]
        db: PathBuf,
    },
    /// Verify a single coin balance against the signed successor of its state height.
    Balance {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        address: String,
        #[arg(long, default_value_t = 0)]
        coin: u32,
        #[arg(long)]
        height: Option<u64>,
    },
    /// Prove transaction inclusion against the DataHash of a verified header.
    Tx {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        rpc: String,
        #[arg(long)]
        hash: String,
        #[arg(long)]
        height: Option<u64>,
        /// Verify execution against LastResultsHash in the cached successor.
        #[arg(long)]
        result: bool,
    },
}

#[derive(Subcommand)]
enum CheckpointAction {
    /// Import a zlib Go checkpoint using a trusted manifest.
    Import {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Read the committed checkpoint manifest from RocksDB.
    Status {
        #[arg(long)]
        db: PathBuf,
    },
    /// Read an IAVL value by its hexadecimal key.
    Get {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        key: String,
    },
}

#[derive(Subcommand)]
enum IavlAction {
    /// Replay ordered Go mutations against an authenticated header index.
    Replay {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        trace: PathBuf,
        #[arg(long)]
        headers: PathBuf,
        #[arg(long)]
        through: u64,
    },
    /// Read the last committed IAVL height and root.
    Status {
        #[arg(long)]
        db: PathBuf,
    },
    /// Show the persisted IAVL policy and retained height range with the daemon stopped.
    History {
        #[arg(long)]
        db: PathBuf,
    },
    /// Change the IAVL retention policy with the daemon stopped.
    Retain {
        #[arg(long)]
        db: PathBuf,
        /// Number of versions to retain, including the head. Zero keeps all future versions.
        #[arg(long)]
        versions: u64,
        /// Run full RocksDB compaction after updating the policy.
        #[arg(long)]
        compact: bool,
    },
    /// Check every retained root against an authenticated header index.
    Verify {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        headers: PathBuf,
    },
    /// Read a hexadecimal key at a retained height or at the IAVL head.
    Get {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        height: Option<u64>,
    },
    /// Create a Go-compatible ICS23 proof from a retained checkpoint version.
    Prove {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        height: Option<u64>,
    },
    /// Check a proof from "iavl prove" against an independently trusted root.
    VerifyProof {
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        height: u64,
        #[arg(long)]
        app_hash: String,
        #[arg(long)]
        key: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Node { action } => node::run(action),
        Command::Checkpoint { action } => checkpoint(action),
        Command::Iavl { action } => iavl(action),
        Command::Light { action } => light(action),
        Command::Peer { action } => peer::run(action),
    }
}

fn light(action: LightAction) -> Result<()> {
    use minter_light::LightClient;
    let result = match action {
        LightAction::Init {
            db,
            anchor,
            primary,
            witness,
        } => {
            let file = File::open(anchor).context("open trusted Tendermint checkpoint")?;
            let anchor = serde_json::from_reader(BufReader::new(file))
                .context("decode trusted checkpoint")?;
            serde_json::to_string(&LightClient::initialize(&db, anchor, &primary, &witness)?)?
        }
        LightAction::Sync {
            db,
            primary,
            witness,
            height,
        } => serde_json::to_string(&LightClient::open(&db)?.sync(&primary, &witness, height)?)?,
        LightAction::Status { db } => serde_json::to_string(&LightClient::open(&db)?.status()?)?,
        LightAction::Balance {
            db,
            rpc,
            address,
            coin,
            height,
        } => {
            let mut decoded = [0; 20];
            hex::decode_to_slice(address.strip_prefix("Mx").unwrap_or(&address), &mut decoded)
                .context("invalid Minter account address")?;
            serde_json::to_string(&LightClient::open(&db)?.balance(&rpc, decoded, coin, height)?)?
        }
        LightAction::Tx {
            db,
            rpc,
            hash,
            height,
            result,
        } => {
            let mut decoded = [0; 32];
            hex::decode_to_slice(hash, &mut decoded).context("invalid transaction hash")?;
            serde_json::to_string(
                &LightClient::open(&db)?.transaction(&rpc, decoded, height, result)?,
            )?
        }
    };
    println!("{result}");
    Ok(())
}

fn checkpoint(action: CheckpointAction) -> Result<()> {
    match action {
        CheckpointAction::Import {
            db,
            snapshot,
            manifest,
        } => {
            let file = File::open(&manifest)
                .with_context(|| format!("open trusted manifest {}", manifest.display()))?;
            let trusted: Checkpoint =
                serde_json::from_reader(file).context("decode trusted manifest")?;
            StateStore::import(&db, &snapshot, &trusted)?;
            println!(
                "{}",
                serde_json::to_string(StateStore::open(&db)?.checkpoint())?
            );
        }
        CheckpointAction::Status { db } => {
            println!(
                "{}",
                serde_json::to_string(StateStore::open(&db)?.checkpoint())?
            );
        }
        CheckpointAction::Get { db, key } => {
            let store = StateStore::open(&db)?;
            let height = store.checkpoint().height;
            print_value(height, &key, |key| store.value_at(height, key))?;
        }
    }
    Ok(())
}

fn iavl(action: IavlAction) -> Result<()> {
    match action {
        IavlAction::Replay {
            db,
            trace,
            headers,
            through,
        } => {
            replay(&db, &trace, &load_headers(&headers)?, through)?;
        }
        IavlAction::Status { db } => {
            let store = BlockStore::open_existing(&db)?;
            println!(
                "{}",
                serde_json::to_string(&store.head().context("missing IAVL head")?)?
            );
        }
        IavlAction::History { db } => {
            print_history(&BlockStore::open_existing(&db)?);
        }
        IavlAction::Retain {
            db,
            versions,
            compact,
        } => {
            let mut store = BlockStore::open_existing(&db)?;
            store.set_keep_iavl_versions(versions)?;
            if compact {
                store.compact()?;
            }
            print_history(&store);
        }
        IavlAction::Verify { db, headers } => {
            let store = StateStore::open(&db)?;
            let headers = load_headers(&headers)?;
            let checkpoint = store.checkpoint();
            let head = store.head();
            let first = store.earliest_height();
            verify_anchors(&headers, checkpoint, head)?;
            for height in first..=head.height {
                ensure!(
                    store.root_at(height)? == headers.committed_root(height)?,
                    "stored IAVL root differs from the header at height {height}",
                );
            }
            println!(
                "{}",
                serde_json::json!({
                    "fromHeight": first,
                    "head": head,
                    "roots": head.height - first + 1,
                })
            );
        }
        IavlAction::Get { db, key, height } => {
            let store = BlockStore::open_existing(&db)?;
            let height = height.unwrap_or(store.head().context("missing IAVL head")?.height);
            print_value(height, &key, |key| store.value_at(height, key))?;
        }
        IavlAction::Prove { db, key, height } => {
            let store = BlockStore::open_existing(&db)?;
            println!("{}", make_proof(&store, height, &key)?);
        }
        IavlAction::VerifyProof {
            proof,
            height,
            app_hash,
            key,
        } => {
            let mut root = [0; 32];
            hex::decode_to_slice(app_hash, &mut root).context("decode trusted AppHash")?;
            let key = hex::decode(key).context("decode IAVL key")?;
            let file = File::open(&proof).context("open IAVL proof file")?;
            let proof: ProofFile = serde_json::from_reader(BufReader::new(file))?;
            let value = verify_proof_file(&proof, height, root, &key)?;
            println!(
                "{}",
                serde_json::json!({ "height": height, "key": hex::encode(key), "value": value })
            );
        }
    }
    Ok(())
}

fn print_history(store: &BlockStore) {
    println!(
        "{}",
        serde_json::json!({
            "earliestHeight": store.earliest_height(),
            "head": store.head(),
            "keepIavlVersions": store.keep_iavl_versions(),
        })
    );
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProofFile {
    height: u64,
    #[serde(default)]
    iavl_version: Option<u64>,
    #[serde(with = "hex::serde")]
    app_hash: Hash,
    #[serde(with = "hex::serde")]
    key: Vec<u8>,
    value: Option<String>,
    #[serde(with = "hex::serde")]
    proof: Vec<u8>,
}

fn make_proof(store: &BlockStore, height: Option<u64>, key: &str) -> Result<serde_json::Value> {
    let reader = store.reader().context("missing IAVL reader")?;
    let state = match height {
        Some(height) => reader.at(height)?,
        None => reader.latest()?,
    };
    let key = hex::decode(key).context("decode IAVL key")?;
    let proof = state.proof(&key)?;
    let head = state.head();
    let mut result = serde_json::json!({
        "height": head.height, "appHash": hex::encode(head.app_hash),
        "key": hex::encode(key), "value": proof.value.map(hex::encode),
        "proof": hex::encode(proof.proof),
    });
    if state.iavl_version() != head.height {
        result["iavlVersion"] = state.iavl_version().into();
    }
    Ok(result)
}

fn verify_proof_file(
    proof: &ProofFile,
    height: u64,
    root: Hash,
    key: &[u8],
) -> Result<Option<String>> {
    ensure!(
        proof.height == height && proof.app_hash == root && proof.key == key,
        "IAVL proof file does not match the trusted height, root and key"
    );
    let version = proof.iavl_version.unwrap_or(height);
    ensure!(
        version == height || Some(version) == height.checked_add(1),
        "invalid IAVL version offset"
    );
    let value = minter_iavl::verify_proof(root, version, key, &proof.proof)?.map(hex::encode);
    ensure!(
        proof.value == value,
        "advertised IAVL value differs from proof"
    );
    Ok(value)
}

fn print_value(
    height: u64,
    key: &str,
    read: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<()> {
    let key = hex::decode(key).context("decode IAVL key")?;
    println!(
        "{}",
        serde_json::json!({
            "height": height,
            "key": hex::encode(&key),
            "value": read(&key)?.map(hex::encode),
        })
    );
    Ok(())
}

struct Header {
    block_hash: Hash,
    previous_root: Hash,
}

struct Headers(BTreeMap<u64, Header>);

impl Headers {
    fn at(&self, height: u64) -> Result<&Header> {
        self.0
            .get(&height)
            .with_context(|| format!("missing header at {height}"))
    }

    fn committed_root(&self, height: u64) -> Result<Hash> {
        Ok(self
            .at(height.checked_add(1).context("invalid header height")?)?
            .previous_root)
    }
}

fn compressed_lines(path: &Path) -> Result<Lines<BufReader<MultiGzDecoder<File>>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(BufReader::new(MultiGzDecoder::new(file)).lines())
}

fn load_headers(path: &Path) -> Result<Headers> {
    let mut rows = compressed_lines(path)?;
    let columns = rows.next().context("empty header index")??;
    ensure!(
        columns
            .split(',')
            .take(3)
            .eq(["height", "block_hash", "header_app_hash"]),
        "invalid header index columns",
    );
    let mut headers = BTreeMap::new();
    for row in rows {
        let row = row?;
        let mut fields = row.split(',');
        let height = fields.next().context("missing header height")?.parse()?;
        let mut block_hash = [0; 32];
        let mut previous_root = [0; 32];
        hex::decode_to_slice(
            fields.next().context("missing block hash")?,
            &mut block_hash,
        )
        .with_context(|| format!("invalid block hash at {height}"))?;
        hex::decode_to_slice(
            fields.next().context("missing header AppHash")?,
            &mut previous_root,
        )
        .with_context(|| format!("invalid header AppHash at {height}"))?;
        let header = Header {
            block_hash,
            previous_root,
        };
        ensure!(
            headers.insert(height, header).is_none(),
            "duplicate header at {height}"
        );
    }
    Ok(Headers(headers))
}

#[derive(Deserialize)]
struct TraceBlock {
    height: u64,
    #[serde(with = "hex::serde")]
    block_hash: Hash,
    #[serde(with = "hex::serde")]
    app_hash: Hash,
    iavl_operations: Option<Vec<TraceMutation>>,
}

#[derive(Deserialize)]
struct TraceMutation {
    #[serde(rename = "key_hex", with = "hex::serde")]
    key: Vec<u8>,
    #[serde(default)]
    value_hex: String,
    #[serde(default)]
    remove: bool,
}

impl TraceMutation {
    fn into_mutation(self) -> Result<Mutation> {
        // Go omits value_hex both for a zero-length value and for a removal.
        if self.remove {
            ensure!(self.value_hex.is_empty(), "IAVL removal contains a value");
            Ok(Mutation::Remove(self.key))
        } else {
            Ok(Mutation::Set {
                key: self.key,
                value: hex::decode(self.value_hex)?,
            })
        }
    }
}

fn verify_anchors(headers: &Headers, checkpoint: &Checkpoint, head: IavlHead) -> Result<()> {
    ensure!(
        headers.at(checkpoint.height)?.block_hash == checkpoint.block_hash
            && headers.committed_root(checkpoint.height)? == checkpoint.app_hash,
        "checkpoint differs from authenticated headers at {}",
        checkpoint.height,
    );
    ensure!(
        headers.committed_root(head.height)? == head.app_hash,
        "stored IAVL head differs from authenticated header at {}",
        head.height + 1,
    );
    Ok(())
}

fn replay(db: &Path, trace: &Path, headers: &Headers, through: u64) -> Result<()> {
    let start_time = Instant::now();
    let mut writer = IavlWriter::open(db)?;
    let checkpoint = writer.checkpoint();
    let start = writer.head();
    verify_anchors(headers, checkpoint, start)?;
    ensure!(
        through >= start.height,
        "target height precedes the IAVL head"
    );
    headers.committed_root(through)?;

    let mut operations = 0usize;
    if start.height < through {
        let heights = checkpoint.height + 1..=through;
        for (height, line) in heights.zip(compressed_lines(trace)?) {
            let block: TraceBlock = serde_json::from_str(&line?)
                .with_context(|| format!("decode IAVL trace at {height}"))?;
            ensure!(
                block.height == height,
                "expected trace height {height}, got {}",
                block.height
            );
            let root = headers.committed_root(height)?;
            ensure!(
                block.block_hash == headers.at(height)?.block_hash && block.app_hash == root,
                "Go trace differs from authenticated headers at height {height}",
            );
            if height > start.height {
                let mutations = block
                    .iavl_operations
                    .unwrap_or_default()
                    .into_iter()
                    .map(TraceMutation::into_mutation)
                    .collect::<Result<Vec<_>>>()
                    .with_context(|| format!("decode IAVL mutations at height {height}"))?;
                operations += mutations.len();
                writer
                    .apply_block(height, mutations, root)
                    .with_context(|| format!("commit IAVL height {height}"))?;
                if (height - start.height).is_multiple_of(1000) {
                    eprintln!("committed {height}");
                }
            }
        }
    }
    let head = writer.head();
    ensure!(
        head.height == through,
        "trace ended at {}, expected {through}",
        head.height
    );
    println!(
        "{}",
        serde_json::json!({
            "initial": start,
            "head": head,
            "blocks": head.height - start.height,
            "operations": operations,
            "elapsedSeconds": start_time.elapsed().as_secs_f64(),
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_genesis_proofs_support_inclusion_absence_and_version_offsets() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for height in [0, 99] {
            let path = directory.path().join(height.to_string());
            let mut store = BlockStore::open(&path)?;
            store.initialize(
                height,
                [Mutation::Set {
                    key: vec![0],
                    value: vec![7],
                }],
                b"genesis",
            )?;
            for current in [height, height + 1] {
                if current > height {
                    store.commit(
                        current,
                        [Mutation::Set {
                            key: vec![0],
                            value: vec![8],
                        }],
                        b"block",
                    )?;
                }
                for (key, expected) in [
                    ("00", Some(if current == height { "07" } else { "08" })),
                    ("01", None),
                ] {
                    let json = make_proof(&store, Some(current), key)?;
                    assert_eq!(json, make_proof(&store, None, key)?);
                    let mut proof: ProofFile = serde_json::from_value(json.clone())?;
                    let value = verify_proof_file(
                        &proof,
                        current,
                        store.head().unwrap().app_hash,
                        &hex::decode(key)?,
                    )?;
                    assert_eq!(value.as_deref(), expected);
                    assert_eq!(json.get("iavlVersion").is_some(), height == 0);
                    proof.iavl_version = Some(current + 2);
                    assert!(
                        verify_proof_file(
                            &proof,
                            current,
                            store.head().unwrap().app_hash,
                            &hex::decode(key)?
                        )
                        .is_err()
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn proof_commands_preserve_support_for_raw_go_snapshot_databases() -> Result<()> {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir()?;
        let mut original = BlockStore::open(&directory.path().join("original"))?;
        let head = original.initialize(
            99,
            [Mutation::Set {
                key: vec![0],
                value: vec![7],
            }],
            b"genesis",
        )?;
        let expected = make_proof(&original, None, "00")?;
        let file = directory.path().join("snapshot.zlib");
        let mut output = File::create(&file)?;
        let reader = original.reader().unwrap();
        let state = reader.latest()?;
        let nodes = state.write_go_snapshot(
            &[
                ("height", 99u64.to_be_bytes().to_vec()),
                ("hash", head.app_hash.to_vec()),
            ],
            &mut output,
        )?;
        drop(output);
        let bytes = std::fs::read(&file)?;
        let trusted = Checkpoint {
            format: 1,
            network: "test-proofs".into(),
            height: 99,
            block_hash: [8; 32],
            app_hash: head.app_hash,
            snapshot_sha256: Sha256::digest(&bytes).into(),
            bytes: bytes.len() as u64,
            nodes,
        };
        let imported = directory.path().join("imported");
        StateStore::import(&imported, &file, &trusted)?;
        let store = BlockStore::open_existing(&imported)?;
        assert_eq!(make_proof(&store, None, "00")?, expected);
        Ok(())
    }

    #[test]
    fn trace_distinguishes_empty_values_and_removals() {
        let decode = |json| {
            serde_json::from_str::<TraceMutation>(json)
                .unwrap()
                .into_mutation()
                .unwrap()
        };
        assert!(matches!(
            decode(r#"{"key_hex":"6d"}"#),
            Mutation::Set { key, value } if key == b"m" && value.is_empty()
        ));
        assert!(matches!(
            decode(r#"{"key_hex":"6d","remove":true}"#),
            Mutation::Remove(key) if key == b"m"
        ));
    }
}
