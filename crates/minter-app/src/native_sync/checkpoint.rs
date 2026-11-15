use super::{
    BlockRecord, CheckpointSource, Cursor, NativeSync, PreviousBlock, proposer, update_params,
};
use crate::{
    Application, checkpoint,
    commission::Fees,
    state_sync::{Chunk, Receiver},
};
use anyhow::{Context, Result, bail, ensure};
use minter_p2p::{Endpoint, ReceivedBlock, snapshot::SnapshotPeer, verify};
use minter_state::{
    BlockStore, Checkpoint, NativeCheckpointWrite, OperationCancelled, OperationControl,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{BufReader, Read},
    path::Path,
    time::Duration,
};
use tendermint::{Hash, block, chain, crypto::Sha256, validator::Set};
use tendermint_proto::v0_34::{
    abci::{
        RequestApplySnapshotChunk, RequestOfferSnapshot, Snapshot,
        response_apply_snapshot_chunk::Result as Apply, response_offer_snapshot::Result as Offer,
    },
    state, types,
};

pub(super) const MAX_MANIFEST_BYTES: u64 = super::MAX_CHECKPOINT_MANIFEST_BYTES;
const MAX_CONSENSUS_STATE_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const MAINNET_INITIAL_HEIGHT: u64 = 10_197_000;

mod context;
mod downloads;

pub(super) fn cleanup_downloads(sync: &NativeSync, database: &Path) {
    let cleanup = || -> Result<u64> {
        let required = sync.history_context_requirement()?;
        downloads::cleanup(database, required.as_ref(), sync.history_context_ready()?)
    };
    match cleanup() {
        Ok(0) => {}
        Ok(count) => eprintln!("Removed {count} stale history download files"),
        Err(error) => eprintln!("Cannot inspect stale history downloads: {error:#}"),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Manifest {
    #[serde(flatten)]
    pub(super) snapshot: Checkpoint,
    #[serde(with = "hex::serde")]
    pub(super) consensus_state_sha256: [u8; 32],
    #[serde(with = "hex::serde")]
    pub(super) blocks_sha256: [u8; 32],
    #[serde(default, skip_serializing_if = "is_false")]
    pub(super) proposer_priorities: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) history_context: Option<HistoryContext>,
    #[serde(default, skip_serializing_if = "crate::frozen::References::is_empty")]
    pub(super) frozen_references: crate::frozen::References,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) genesis_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) transaction_chain_id: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryContext {
    pub from: u64,
    pub bytes: u64,
    #[serde(with = "hex::serde")]
    pub sha256: [u8; 32],
}

impl HistoryContext {
    pub(super) fn validate(&self, height: u64, initial: u64) -> Result<()> {
        ensure!(
            self.from >= initial
                && self.from < height
                && self.bytes > 0
                && self
                    .bytes
                    .div_ceil(minter_p2p::snapshot::SNAPSHOT_CHUNK_BYTES)
                    <= u64::from(minter_p2p::snapshot::MAX_SNAPSHOT_CHUNKS),
            "invalid required history context descriptor"
        );
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

fn package_identity(
    package: &Manifest,
    network: u8,
    genesis: Option<&Path>,
) -> Result<checkpoint::Identity> {
    let mainnet = package.snapshot.network == "minter-mainnet-5";
    let identity = if let Some(path) = genesis {
        super::read_genesis_identity(path, network)?
    } else {
        checkpoint::validate_network(&package.snapshot, network)?;
        checkpoint::Identity {
            chain_id: package.snapshot.network.parse()?,
            initial_height: MAINNET_INITIAL_HEIGHT,
            transaction_chain_id: network,
            genesis_sha256: None,
        }
    };
    identity.validate_snapshot(&package.snapshot)?;
    if let Some(id) = package.transaction_chain_id {
        ensure!(
            id == network,
            "checkpoint transaction chain ID differs from the selected network"
        );
    } else {
        ensure!(mainnet, "custom checkpoint requires transactionChainId");
    }
    if let Some(encoded) = &package.genesis_sha256 {
        let digest: [u8; 32] = hex::decode(encoded)
            .context("invalid checkpoint genesis SHA-256")?
            .as_slice()
            .try_into()
            .context("invalid checkpoint genesis SHA-256 length")?;
        let expected = if mainnet {
            hex::decode(super::MAINNET_GENESIS)?.as_slice().try_into()?
        } else {
            identity
                .genesis_sha256
                .context("provide --genesis for a custom checkpoint")?
        };
        ensure!(
            digest == expected,
            "checkpoint genesis SHA-256 differs from trusted genesis"
        );
    } else {
        ensure!(mainnet, "custom checkpoint requires genesisSha256");
    }
    Ok(identity)
}

fn check_recovery_identity(
    app: &Application,
    cursor: &Cursor,
    identity: &checkpoint::Identity,
) -> Result<()> {
    let chain = app
        .chain
        .as_ref()
        .context("missing checkpoint chain metadata")?;
    ensure!(
        chain.id == identity.chain_id.as_str()
            && chain.initial_height == identity.initial_height
            && chain.transaction_chain_id == identity.transaction_chain_id,
        "saved checkpoint chain differs from trusted genesis"
    );
    if let Some(actual) = cursor.genesis_sha256 {
        let expected = match identity.genesis_sha256 {
            Some(digest) => digest,
            None if identity.chain_id.as_str() == "minter-mainnet-5" => {
                hex::decode(super::MAINNET_GENESIS)?.as_slice().try_into()?
            }
            None => bail!("custom checkpoint lacks a trusted genesis digest"),
        };
        ensure!(
            actual == expected,
            "saved checkpoint genesis differs from trusted genesis"
        );
    } else {
        ensure!(
            identity.chain_id.as_str() == "minter-mainnet-5",
            "saved custom checkpoint lacks its genesis digest"
        );
    }
    Ok(())
}

pub(super) fn open(
    path: &Path,
    network: u8,
    source: CheckpointSource<'_>,
    keep: Option<u64>,
    control: &OperationControl,
) -> Result<NativeSync> {
    control.check()?;
    let CheckpointSource {
        package,
        trusted_sha256,
        genesis,
        peers,
    } = source;
    let manifest_bytes = read_limited(&package.join("manifest.json"), MAX_MANIFEST_BYTES)?;
    ensure!(
        super::Hasher::digest(&manifest_bytes) == trusted_sha256,
        "checkpoint manifest SHA-256 differs from the trusted digest"
    );
    let package_manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("decode checkpoint manifest")?;
    let manifest = &package_manifest.snapshot;
    let identity = package_identity(&package_manifest, network, genesis)?;
    // Explicit identities are checked before any database is opened. Keep the
    // legacy mainnet recovery path's lazy use of already-installed anchor files.
    let mut preflight = if genesis.is_some()
        || package_manifest.genesis_sha256.is_some()
        || package_manifest.transaction_chain_id.is_some()
    {
        Some(verify_package(
            package,
            &package_manifest,
            &identity,
            control,
        )?)
    } else {
        None
    };
    control.check()?;
    let store = BlockStore::open_for_native_checkpoint_with_control(
        path,
        manifest,
        trusted_sha256,
        control,
    )?;
    let mut app = Application::from_store(store, network, None)?;
    let chain_id: chain::Id = manifest.network.parse()?;
    if let Some(cursor) = app
        .chain
        .as_ref()
        .and_then(|chain| chain.native_sync.as_ref())
    {
        ensure!(
            cursor.checkpoint_sha256 == Some(trusted_sha256)
                && app.store.checkpoint() == Some(manifest)
                && cursor.history_context == package_manifest.history_context,
            "checkpoint differs from the native chain database"
        );
        check_recovery_identity(&app, cursor, &identity)?;
        cursor.check_recovery(&app, &chain_id)?;
        if let Some(keep) = keep {
            app.set_keep_iavl_versions(keep)?;
        }
        let base = app
            .store
            .native_block_base()?
            .context("missing native archive")?;
        let anchor = if app
            .store
            .evidence_index_start()?
            .is_none_or(|start| start > base)
        {
            Some(match preflight.take() {
                Some((cursor, _)) => cursor,
                None => verify_package(package, &package_manifest, &identity, control)?.0,
            })
        } else {
            None
        };
        return context::complete(
            NativeSync::new(app, chain_id, anchor)?,
            package,
            package_manifest.history_context.as_ref(),
            peers,
            control,
        );
    }
    ensure!(
        app.head().is_none(),
        "native checkpoint import needs a fresh database"
    );

    let (mut cursor, record) = match preflight {
        Some(verified) => verified,
        None => verify_package(package, &package_manifest, &identity, control)?,
    };
    cursor.checkpoint_sha256 = Some(trusted_sha256);
    let validators = cursor
        .previous
        .as_ref()
        .context("missing checkpoint validators")?;
    let validators = proposer::to_wire(&validators.validators).encode_to_vec();
    let block = BlockRecord::decode(record.as_slice())?;
    let block = ReceivedBlock::decode(block.block_response)?;
    let committed_evidence = block
        .evidence()
        .evidence
        .iter()
        .map(minter_p2p::evidence::hash)
        .collect::<Result<Vec<_>>>()?;
    let snapshot = match peers {
        Some((peers, timeout)) => download(
            app.store.directory(),
            manifest,
            &chain_id,
            peers,
            timeout,
            control,
        )?,
        None => File::open(package.join("snapshot.zlib")).context("open Go snapshot")?,
    };
    if let Some(keep) = keep {
        app.set_keep_iavl_versions(keep)?;
    }
    let (chain, fees) = app.store.install_native_checkpoint_with_control(
        snapshot,
        manifest,
        trusted_sha256,
        control,
        |view| {
            let mut chain = checkpoint::restore_for_identity(manifest, &identity, |name| {
                Ok(view.metadata(name).map(<[u8]>::to_vec))
            })?;
            chain.native_sync = Some(cursor);
            package_manifest
                .frozen_references
                .validate(manifest.height, |height| {
                    view.value(&crate::frozen::key(height))
                })?;
            chain.frozen_references = package_manifest.frozen_references.clone();
            let metadata = serde_json::to_vec(&chain)?;
            let fees = Fees::decode(&view.value(b"p")?.context("missing commission prices")?)?;
            Ok((
                (chain, fees),
                NativeCheckpointWrite {
                    metadata,
                    block_record: record,
                    validators,
                    committed_evidence,
                },
            ))
        },
    )?;
    app.chain = Some(chain);
    app.fees = Some(fees);
    context::complete(
        NativeSync::new(app, chain_id, None)?,
        package,
        package_manifest.history_context.as_ref(),
        peers,
        control,
    )
}

fn download(
    path: &Path,
    manifest: &Checkpoint,
    chain: &chain::Id,
    peers: &[Endpoint],
    timeout: Duration,
    control: &OperationControl,
) -> Result<File> {
    for endpoint in peers {
        control.check()?;
        match download_from(path, manifest, chain, *endpoint, timeout, control) {
            Ok(file) => return Ok(file),
            Err(error) if OperationCancelled::caused(&error) || control.is_cancelled() => {
                return Err(error);
            }
            Err(error) => eprintln!(
                "State Sync peer {}@{}: {error:#}",
                endpoint.id, endpoint.address
            ),
        }
    }
    bail!(
        "no State Sync peer served the trusted snapshot at height {}",
        manifest.height
    )
}

fn download_from(
    path: &Path,
    manifest: &Checkpoint,
    chain: &chain::Id,
    endpoint: Endpoint,
    timeout: Duration,
    control: &OperationControl,
) -> Result<File> {
    control.check()?;
    let mut peer = SnapshotPeer::connect(endpoint, chain, timeout)?;
    control.check()?;
    let advertised = peer.find(manifest.height, manifest.format, &manifest.snapshot_sha256)?;
    let mut receiver = Receiver::new(path, manifest.clone())?;
    let chunks = advertised.chunks;
    let offered = receiver.offer(RequestOfferSnapshot {
        snapshot: Some(Snapshot {
            height: advertised.height,
            format: advertised.format,
            chunks,
            hash: advertised.hash.into(),
            metadata: advertised.metadata.into(),
        }),
        app_hash: manifest.app_hash.to_vec().into(),
    });
    ensure!(
        offered == Offer::Accept,
        "snapshot metadata rejected: {offered:?}"
    );
    for index in 0..chunks {
        control.check()?;
        let chunk = peer.chunk(manifest.height, manifest.format, index)?;
        match receiver.apply(RequestApplySnapshotChunk {
            index,
            chunk: chunk.into(),
            sender: endpoint.id.to_string(),
        }) {
            Chunk::Ready(file) => {
                eprintln!(
                    "State Sync downloaded {} bytes in {chunks} chunks from {}",
                    manifest.bytes, endpoint.id
                );
                return Ok(file);
            }
            Chunk::Reply(reply) => {
                ensure!(
                    reply.result() == Apply::Accept,
                    "snapshot chunk {index} rejected: {:?}",
                    reply.result()
                );
            }
        }
    }
    bail!("snapshot download did not complete")
}

fn verify_package(
    package: &Path,
    package_manifest: &Manifest,
    identity: &checkpoint::Identity,
    control: &OperationControl,
) -> Result<(Cursor, Vec<u8>)> {
    let raw = read_limited_with_control(
        &package.join("state.pb"),
        MAX_CONSENSUS_STATE_BYTES,
        control,
    )?;
    let blocks = read_limited_with_control(
        &package.join("blocks.pb"),
        3 * (super::MAX_BLOCK_BYTES + 1024),
        control,
    )?;
    verify_anchor(package_manifest, identity, &raw, &blocks, control)
}

pub(super) fn hash_anchor_bytes(bytes: &[u8], control: &OperationControl) -> Result<[u8; 32]> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    control.export_progress(minter_state::ExportStage::AnchorHashBytes, 0)?;
    let mut done = 0;
    for chunk in bytes.chunks(64 * 1024) {
        control.check()?;
        hash.update(chunk);
        done += chunk.len() as u64;
        if done.is_multiple_of(1024 * 1024) || done == bytes.len() as u64 {
            control.export_progress(minter_state::ExportStage::AnchorHashBytes, done)?;
        }
    }
    Ok(hash.finalize().into())
}

pub(super) fn verify_anchor(
    package_manifest: &Manifest,
    identity: &checkpoint::Identity,
    raw: &[u8],
    blocks: &[u8],
    control: &OperationControl,
) -> Result<(Cursor, Vec<u8>)> {
    let chain = &identity.chain_id;
    let manifest = &package_manifest.snapshot;
    ensure!(
        hash_anchor_bytes(raw, control)? == package_manifest.consensus_state_sha256,
        "Tendermint state SHA-256 differs from the trusted manifest"
    );
    let state = state::State::decode(raw).context("decode Tendermint 0.34 State")?;
    let height = u64::try_from(state.last_block_height)?;
    ensure!(
        height == manifest.height
            && state.chain_id == chain.as_str()
            && state.initial_height == identity.initial_height as i64
            && state.app_hash == manifest.app_hash,
        "Tendermint state differs from the trusted checkpoint"
    );
    let last_id: block::Id = state
        .last_block_id
        .context("missing state BlockID")?
        .try_into()?;
    ensure!(
        last_id.hash.as_bytes() == manifest.block_hash,
        "checkpoint BlockID differs from Tendermint state"
    );
    let last_time = state
        .last_block_time
        .context("missing state block time")?
        .try_into()?;
    if let Some(context) = &package_manifest.history_context {
        context.validate(height, identity.initial_height)?;
    }
    let priorities = package_manifest.proposer_priorities;
    let previous = decode_set(state.last_validators, priorities)?;
    let validators = decode_set(state.validators, priorities)?;
    let next_validators = decode_set(state.next_validators, priorities)?;
    let params = update_params(
        state
            .consensus_params
            .context("missing consensus parameters")?
            .try_into()?,
        None,
    )?;
    let consensus = state
        .version
        .and_then(|version| version.consensus)
        .context("missing consensus version")?;
    ensure!(
        consensus.block == 11,
        "unexpected Tendermint block protocol"
    );
    let results_hash = Hash::from_bytes(
        tendermint::hash::Algorithm::Sha256,
        &state.last_results_hash,
    )?;
    ensure!(results_hash != Hash::None, "missing state LastResultsHash");

    ensure!(
        hash_anchor_bytes(blocks, control)? == package_manifest.blocks_sha256,
        "checkpoint blocks SHA-256 differs from the trusted manifest"
    );
    let mut input = BufReader::new(control.reader(blocks));
    let last = ReceivedBlock::read(&mut input)?.context("missing snapshot block")?;
    let first = ReceivedBlock::read(&mut input)?.context("missing snapshot successor")?;
    let witness = ReceivedBlock::read(&mut input)?.context("missing second snapshot successor")?;
    ensure!(
        input.read(&mut [0u8])? == 0,
        "trailing checkpoint block data"
    );
    let header = &last.block().header;
    ensure!(
        header.height.value() == height
            && header.chain_id == *chain
            && header.time == last_time
            && last.id() == last_id
            && header.validators_hash == previous.hash()
            && header.next_validators_hash == validators.hash(),
        "snapshot block differs from Tendermint state"
    );
    let commit =
        verify::verify_link(&last, &first, &previous).context("verify snapshot block commit")?;
    control.check()?;
    let cursor = Cursor {
        genesis_sha256: identity.genesis_sha256,
        genesis_time: None,
        genesis_app_hash: Vec::new(),
        checkpoint_sha256: None,
        history_context: package_manifest.history_context.clone(),
        app_version: Some(consensus.app),
        height,
        root: manifest.app_hash,
        previous: Some(PreviousBlock {
            id: last_id,
            time: last_time,
            validators: previous,
        }),
        validators,
        next_validators,
        params,
        results_hash,
    };
    cursor
        .check_header(&first, chain)
        .context("verify snapshot successor header")?;
    ensure!(
        witness.block().header.validators_hash == cursor.next_validators.hash(),
        "checkpoint successor differs from its validator set or parameters"
    );
    verify::verify_link(&first, &witness, &cursor.validators)
        .context("authenticate the header containing the snapshot AppHash")?;
    control.check()?;
    let record = BlockRecord {
        block_response: last.wire().to_vec(),
        commit: Some(types::Commit::from(commit.clone())),
        deliver_tx: Vec::new(),
        end_block: None,
    }
    .encode_to_vec();
    Ok((cursor, record))
}

fn decode_set(raw: Option<types::ValidatorSet>, priorities: bool) -> Result<Set> {
    let mut raw = raw.context("missing checkpoint validator set")?;
    if priorities {
        ensure!(
            raw.proposer.is_some(),
            "checkpoint lacks the selected proposer"
        );
    } else {
        raw.proposer = None;
        for validator in &mut raw.validators {
            validator.proposer_priority = 0;
        }
    }
    proposer::validate_wire(raw)
}

fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>> {
    read_limited_with_control(path, max, &OperationControl::default())
}

fn read_limited_with_control(path: &Path, max: u64, control: &OperationControl) -> Result<Vec<u8>> {
    control.check()?;
    let mut bytes = Vec::new();
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    control.reader(file).take(max + 1).read_to_end(&mut bytes)?;
    control.check()?;
    ensure!(bytes.len() as u64 <= max, "{} is too large", path.display());
    Ok(bytes)
}

#[cfg(test)]
mod checkpoint_genesis_tests {
    use super::*;

    #[test]
    fn anchor_hashing_stops_between_portions_and_preserves_sha256() -> Result<()> {
        let bytes = vec![7; 3 * 1024 * 1024 + 1];
        assert_eq!(
            hash_anchor_bytes(&bytes, &OperationControl::default())?,
            super::super::Hasher::digest(&bytes)
        );
        let control = OperationControl::default();
        let stop = control.clone();
        let control = control.with_export_progress(move |phase, done| {
            if phase == minter_state::ExportStage::AnchorHashBytes && done == 1024 * 1024 {
                stop.cancel();
            }
        });
        let error = hash_anchor_bytes(&bytes, &control).unwrap_err();
        assert!(OperationCancelled::caused(&error));
        Ok(())
    }

    fn manifest(name: &str) -> Manifest {
        Manifest {
            snapshot: Checkpoint {
                format: 1,
                network: name.into(),
                height: 110,
                block_hash: [1; 32],
                app_hash: [2; 32],
                snapshot_sha256: [3; 32],
                bytes: 1,
                nodes: 1,
            },
            consensus_state_sha256: [4; 32],
            blocks_sha256: [5; 32],
            proposer_priorities: false,
            history_context: None,
            frozen_references: Default::default(),
            genesis_sha256: None,
            transaction_chain_id: None,
        }
    }

    #[test]
    fn checkpoint_genesis_binds_custom_package_to_file_and_transaction_network() {
        let (_directory, path) = super::super::checkpoint_genesis_tests::fixture();
        let expected = super::super::read_genesis_identity(&path, 1).unwrap();
        let mut package = manifest(expected.chain_id.as_str());
        package.genesis_sha256 = Some(hex::encode(expected.genesis_sha256.unwrap()));
        package.transaction_chain_id = Some(1);
        let actual = package_identity(&package, 1, Some(&path)).unwrap();
        assert_eq!(actual.chain_id, expected.chain_id);
        assert_eq!(actual.initial_height, 101);
        assert!(package_identity(&package, 1, None).is_err());
        assert!(package_identity(&package, 2, Some(&path)).is_err());
        package.transaction_chain_id = None;
        assert!(package_identity(&package, 1, Some(&path)).is_err());
        package.transaction_chain_id = Some(1);
        package.genesis_sha256 = None;
        assert!(package_identity(&package, 1, Some(&path)).is_err());
        package.genesis_sha256 = Some("00".repeat(32));
        assert!(package_identity(&package, 1, Some(&path)).is_err());
    }

    #[test]
    fn checkpoint_genesis_preserves_legacy_mainnet_and_checks_present_identity_fields() {
        let mut package = manifest("minter-mainnet-5");
        package.snapshot.height = 40_700_741;
        let identity = package_identity(&package, 1, None).unwrap();
        assert_eq!(identity.initial_height, MAINNET_INITIAL_HEIGHT);
        assert!(identity.genesis_sha256.is_none());
        package.transaction_chain_id = Some(2);
        assert!(package_identity(&package, 1, None).is_err());
        package.transaction_chain_id = Some(1);
        package.genesis_sha256 = Some("00".repeat(32));
        assert!(package_identity(&package, 1, None).is_err());
        package.genesis_sha256 = Some(super::super::MAINNET_GENESIS.into());
        assert!(package_identity(&package, 1, None).is_ok());
    }

    #[test]
    fn checkpoint_genesis_rejects_foreign_initial_height_before_creating_database() {
        let (directory, path) = super::super::checkpoint_genesis_tests::fixture();
        let identity = super::super::read_genesis_identity(&path, 1).unwrap();
        let package_dir = directory.path().join("package");
        fs::create_dir(&package_dir).unwrap();
        let mut package = manifest(identity.chain_id.as_str());
        package.genesis_sha256 = Some(hex::encode(identity.genesis_sha256.unwrap()));
        package.transaction_chain_id = Some(1);
        let state = state::State {
            chain_id: identity.chain_id.to_string(),
            initial_height: 102,
            last_block_height: 110,
            app_hash: package.snapshot.app_hash.to_vec(),
            ..Default::default()
        }
        .encode_to_vec();
        package.consensus_state_sha256 = super::super::Hasher::digest(&state);
        package.blocks_sha256 = super::super::Hasher::digest([]);
        fs::write(package_dir.join("state.pb"), state).unwrap();
        fs::write(package_dir.join("blocks.pb"), []).unwrap();
        let raw = serde_json::to_vec(&package).unwrap();
        let digest = super::super::Hasher::digest(&raw);
        fs::write(package_dir.join("manifest.json"), raw).unwrap();
        let db = directory.path().join("must-not-exist");
        let error = open(
            &db,
            1,
            CheckpointSource {
                package: &package_dir,
                trusted_sha256: digest,
                genesis: Some(&path),
                peers: None,
            },
            None,
            &OperationControl::default(),
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("Tendermint state differs"),
            "{error:#}"
        );
        assert!(
            !db.exists(),
            "invalid state created a database before validation"
        );
    }

    #[test]
    fn checkpoint_genesis_checks_saved_identity_and_allows_legacy_mainnet_digest() {
        let (directory, path) = super::super::checkpoint_genesis_tests::fixture();
        let identity = super::super::read_genesis_identity(&path, 1).unwrap();
        let mut app = Application::open(&directory.path().join("state"), 1).unwrap();
        super::super::initialize(&mut app, &path).unwrap();
        let mut cursor = app.chain.as_ref().unwrap().native_sync.clone().unwrap();
        check_recovery_identity(&app, &cursor, &identity).unwrap();
        app.chain.as_mut().unwrap().initial_height += 1;
        assert!(check_recovery_identity(&app, &cursor, &identity).is_err());
        app.chain.as_mut().unwrap().initial_height -= 1;
        app.chain.as_mut().unwrap().id = "another-chain".into();
        assert!(check_recovery_identity(&app, &cursor, &identity).is_err());
        app.chain.as_mut().unwrap().id = identity.chain_id.to_string();
        app.chain.as_mut().unwrap().transaction_chain_id = 2;
        assert!(check_recovery_identity(&app, &cursor, &identity).is_err());
        app.chain.as_mut().unwrap().transaction_chain_id = 1;
        cursor.genesis_sha256 = None;
        assert!(check_recovery_identity(&app, &cursor, &identity).is_err());
        cursor.genesis_sha256 = Some([0; 32]);
        assert!(check_recovery_identity(&app, &cursor, &identity).is_err());
        let mainnet = checkpoint::Identity {
            chain_id: "minter-mainnet-5".parse().unwrap(),
            initial_height: MAINNET_INITIAL_HEIGHT,
            transaction_chain_id: 1,
            genesis_sha256: Some(
                hex::decode(super::super::MAINNET_GENESIS)
                    .unwrap()
                    .as_slice()
                    .try_into()
                    .unwrap(),
            ),
        };
        app.chain.as_mut().unwrap().id = mainnet.chain_id.to_string();
        app.chain.as_mut().unwrap().initial_height = mainnet.initial_height;
        cursor.genesis_sha256 = None;
        check_recovery_identity(&app, &cursor, &mainnet).unwrap();
    }
}
