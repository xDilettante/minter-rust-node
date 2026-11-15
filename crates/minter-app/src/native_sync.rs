//! Sequential Tendermint 0.34 block sync for the Minter application.

pub const MAX_CHECKPOINT_MANIFEST_BYTES: u64 = 1024 * 1024;

mod checkpoint;
mod consensus_round;
mod evidence;
mod export;
mod mempool;
mod proposer;

use crate::{APP_PROTOCOL_VERSION, Application};
use anyhow::{Context, Result, ensure};
pub use checkpoint::HistoryContext;
pub use consensus_round::NativeConsensus;
pub use evidence::NativeRetentionReport;
pub use mempool::Submission;
use minter_p2p::{BlockRange, ReceivedBlock, server::BlockSource, verify};
use minter_state::{IavlHead, NativeBlockWrite, OperationControl, RetentionPolicy, StateReader};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use tendermint::{
    Genesis, Hash, Time, account,
    block::{self, Commit},
    chain, consensus,
    crypto::{Sha256, default::Sha256 as Hasher},
    merkle,
    validator::{Info, Set},
};
use tendermint_proto::v0_34::{abci, types};

const MAINNET_GENESIS: &str = "906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5";
const MAX_BLOCK_BYTES: u64 = 100 * 1024 * 1024;

/// The checkpoint identity is authenticated before its imported state is exposed.
#[derive(Clone, Copy)]
pub struct CheckpointSource<'a> {
    pub package: &'a Path,
    pub trusted_sha256: [u8; 32],
    pub genesis: Option<&'a Path>,
    pub peers: Option<(&'a [minter_p2p::Endpoint], std::time::Duration)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Cursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    genesis_sha256: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    genesis_time: Option<Time>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    genesis_app_hash: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint_sha256: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_context: Option<checkpoint::HistoryContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_version: Option<u64>,
    height: u64,
    root: [u8; 32],
    previous: Option<PreviousBlock>,
    #[serde(with = "proposer::stored")]
    validators: Set,
    #[serde(with = "proposer::stored")]
    next_validators: Set,
    params: consensus::Params,
    results_hash: Hash,
}

#[derive(Clone, Serialize, Deserialize)]
struct PreviousBlock {
    id: block::Id,
    time: Time,
    #[serde(with = "proposer::stored")]
    validators: Set,
}

#[derive(Message)]
struct BlockRecord {
    #[prost(bytes, tag = "1")]
    block_response: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    commit: Option<types::Commit>,
    #[prost(message, repeated, tag = "3")]
    deliver_tx: Vec<abci::ResponseDeliverTx>,
    #[prost(message, optional, tag = "4")]
    end_block: Option<abci::ResponseEndBlock>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedBlock {
    pub height: u64,
    pub app_hash: String,
    pub block_hash: String,
    pub part_set_hash: String,
    pub parts: u32,
    pub results_hash: String,
    pub transactions: usize,
    pub validator_updates: usize,
    pub begin_request_sha256: String,
    pub deliver_responses_sha256: String,
    pub end_response_sha256: String,
    pub validators_hash: String,
    pub next_validators_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposer_state_sha256: Option<String>,
}

pub struct NativeSync {
    app: Application,
    chain_id: chain::Id,
    mempool: Option<mempool::Pool>,
    peer_height: u64,
    evidence: evidence::Pool,
}

/// Serialize block execution and mempool checks while the network and API run independently.
#[derive(Clone)]
pub struct NativeNode(Arc<Mutex<NativeSync>>);

impl NativeNode {
    pub fn new(sync: NativeSync) -> Self {
        Self(Arc::new(Mutex::new(sync)))
    }

    pub fn lock(&self) -> Result<MutexGuard<'_, NativeSync>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("native node lock"))
    }

    pub fn submit(&self, tx: Vec<u8>, source: Option<tendermint::node::Id>) -> Result<Submission> {
        let mut sync = self.lock()?;
        sync.submit_transaction(tx, source)
    }
}

/// Access the durable P2P archive while the sync worker is applying blocks.
#[derive(Clone)]
pub struct NativeBlocks(StateReader);

impl NativeBlocks {
    pub(crate) fn read(reader: &StateReader, height: u64) -> Result<Option<ReceivedBlock>> {
        reader
            .native_block_at(height)?
            .map(|raw| ReceivedBlock::decode(BlockRecord::decode(raw.as_slice())?.block_response))
            .transpose()
    }
}

impl BlockSource for NativeBlocks {
    fn range(&self) -> Result<BlockRange> {
        let (base, height) = self.0.native_block_range()?.unwrap_or_default();
        Ok(BlockRange { base, height })
    }

    fn block(&self, height: u64) -> Result<Option<Vec<u8>>> {
        self.0
            .native_block_at(height)?
            .map(|raw| Ok(BlockRecord::decode(raw.as_slice())?.block_response))
            .transpose()
    }
}

impl NativeSync {
    pub(crate) fn application(&self) -> &Application {
        &self.app
    }

    fn new(app: Application, chain_id: chain::Id, anchor: Option<Cursor>) -> Result<Self> {
        evidence::restore_history(&app, &chain_id, anchor.as_ref())?;
        let evidence = evidence::Pool::load(&app.store)?;
        Ok(Self {
            app,
            chain_id,
            mempool: None,
            peer_height: 0,
            evidence,
        })
    }

    pub fn enable_mempool(&mut self) -> Result<()> {
        let height = self.head()?.height;
        let round = match self.app.store.native_block_at(height)? {
            Some(record) => {
                BlockRecord::decode(record.as_slice())?
                    .commit
                    .context("missing stored commit")?
                    .round
            }
            None => -1,
        };
        let pool = self.mempool.get_or_insert_with(mempool::Pool::default);
        pool.last_commit_round = round;
        Ok(())
    }

    fn submit_transaction(
        &mut self,
        tx: Vec<u8>,
        source: Option<tendermint::node::Id>,
    ) -> Result<Submission> {
        let height = self.head()?.height;
        let Some(pool) = &mut self.mempool else {
            return Ok(Submission::Disabled);
        };
        if self.peer_height > height.saturating_add(3) {
            return Ok(Submission::CatchingUp);
        }
        pool.submit(tx, height, source, |tx, price| {
            Ok(self.app.check(tx, price)?.check())
        })
    }

    pub fn open(
        path: &Path,
        network: u8,
        genesis: Option<&Path>,
        keep_iavl_versions: Option<u64>,
    ) -> Result<Self> {
        Self::open_with_control(
            path,
            network,
            genesis,
            keep_iavl_versions,
            &OperationControl::default(),
        )
    }

    pub fn open_with_control(
        path: &Path,
        network: u8,
        genesis: Option<&Path>,
        keep_iavl_versions: Option<u64>,
        control: &OperationControl,
    ) -> Result<Self> {
        let store = minter_state::BlockStore::open_with_control(path, control)?;
        let mut app = Application::from_store(store, network, None)?;
        control.check()?;
        let cursor = app
            .chain
            .as_ref()
            .and_then(|chain| chain.native_sync.as_ref());
        let chain_id = if let Some(cursor) = cursor {
            if let Some(genesis) = genesis {
                ensure!(
                    Some(crate::genesis::control::hash_bytes(
                        &crate::genesis::control::read_file(genesis, control)?,
                        minter_state::GenesisStage::FileHashBytes,
                        control,
                    )?) == cursor.genesis_sha256,
                    "genesis differs from the native chain database"
                );
            }
            let chain_id = app.chain.as_ref().context("missing chain")?.id.parse()?;
            cursor.check_recovery(&app, &chain_id)?;
            chain_id
        } else {
            let genesis = genesis.context("provide --genesis to initialize native sync")?;
            initialize(&mut app, genesis)?
        };
        if let Some(keep) = keep_iavl_versions {
            app.set_keep_iavl_versions(keep)?;
        }
        let sync = Self::new(app, chain_id, None)?;
        sync.cleanup_history_downloads();
        Ok(sync)
    }

    pub fn open_checkpoint(
        path: &Path,
        network: u8,
        package: &Path,
        trusted_sha256: [u8; 32],
        keep_iavl_versions: Option<u64>,
    ) -> Result<Self> {
        Self::open_checkpoint_with_control(
            path,
            network,
            CheckpointSource {
                package,
                trusted_sha256,
                genesis: None,
                peers: None,
            },
            keep_iavl_versions,
            &OperationControl::default(),
        )
    }

    pub fn open_network_checkpoint(
        path: &Path,
        network: u8,
        package: &Path,
        trusted_sha256: [u8; 32],
        keep_iavl_versions: Option<u64>,
        peers: &[minter_p2p::Endpoint],
        timeout: std::time::Duration,
    ) -> Result<Self> {
        Self::open_checkpoint_with_control(
            path,
            network,
            CheckpointSource {
                package,
                trusted_sha256,
                genesis: None,
                peers: Some((peers, timeout)),
            },
            keep_iavl_versions,
            &OperationControl::default(),
        )
    }

    pub fn open_checkpoint_for_genesis(
        path: &Path,
        network: u8,
        package: &Path,
        trusted_sha256: [u8; 32],
        genesis: &Path,
        keep_iavl_versions: Option<u64>,
        peers: Option<(&[minter_p2p::Endpoint], std::time::Duration)>,
    ) -> Result<Self> {
        Self::open_checkpoint_with_control(
            path,
            network,
            CheckpointSource {
                package,
                trusted_sha256,
                genesis: Some(genesis),
                peers,
            },
            keep_iavl_versions,
            &OperationControl::default(),
        )
    }

    pub fn open_checkpoint_with_control(
        path: &Path,
        network: u8,
        source: CheckpointSource<'_>,
        keep_iavl_versions: Option<u64>,
        control: &OperationControl,
    ) -> Result<Self> {
        checkpoint::open(path, network, source, keep_iavl_versions, control)
    }

    pub fn chain_id(&self) -> &chain::Id {
        &self.chain_id
    }

    pub fn blocks(&self) -> Result<NativeBlocks> {
        Ok(NativeBlocks(
            self.app
                .store
                .reader()
                .context("missing native block store")?,
        ))
    }

    pub fn head(&self) -> Result<IavlHead> {
        self.app.head().context("missing native chain head")
    }

    pub fn retention_policy(&self) -> Result<RetentionPolicy> {
        self.app.store.retention_policy()
    }

    pub fn configure_retention(&mut self, policy: RetentionPolicy) -> Result<()> {
        self.app.store.configure_retention(policy)
    }

    pub fn event_block_range(&self) -> Result<Option<(u64, u64)>> {
        self.app
            .store
            .reader()
            .context("missing event store")?
            .event_block_range()
    }

    pub fn iavl_range(&self) -> Result<(u64, u64)> {
        Ok((
            self.app
                .store
                .earliest_height()
                .context("missing IAVL history")?,
            self.head()?.height,
        ))
    }

    pub fn compact_storage(&self) -> Result<()> {
        self.app.store.compact()
    }

    pub fn cleanup_history_downloads(&self) {
        checkpoint::cleanup_downloads(self, self.app.store.directory());
    }

    pub(crate) fn peer_height(&self) -> u64 {
        self.peer_height
    }

    /// Update the estimate after applying a verified block from this peer.
    pub fn observe_peer_height(&mut self, height: u64) {
        self.peer_height = height;
    }

    /// Two successors authenticate the block, its resulting AppHash and its ABCI result root.
    pub fn apply(
        &mut self,
        block: &ReceivedBlock,
        next: &ReceivedBlock,
        witness: &ReceivedBlock,
    ) -> Result<AppliedBlock> {
        let result = self
            .ensure_history_context()
            .and_then(|()| self.apply_block(block, next, witness));
        if result.is_err() {
            self.app.pending = None;
        }
        result
    }

    fn apply_block(
        &mut self,
        block: &ReceivedBlock,
        next: &ReceivedBlock,
        witness: &ReceivedBlock,
    ) -> Result<AppliedBlock> {
        let cursor = self.cursor()?;
        self.validate_candidate(cursor, block)?;
        let commit = verify::verify_link(block, next, &cursor.validators)?;
        ensure!(
            next.block().header.validators_hash == cursor.next_validators.hash(),
            "successor validator set differs at height {}",
            next.block().header.height
        );
        verify::verify_link(next, witness, &cursor.next_validators)?;
        self.execute_committed(block, commit, Some(next), Vec::new())
    }

    fn cursor(&self) -> Result<&Cursor> {
        self.app
            .chain
            .as_ref()
            .and_then(|chain| chain.native_sync.as_ref())
            .context("missing native sync state")
    }

    fn validate_candidate(&self, cursor: &Cursor, block: &ReceivedBlock) -> Result<()> {
        cursor.check_header(block, &self.chain_id)?;
        ensure!(
            block.evidence_size() as i64 <= cursor.params.evidence.max_bytes,
            "block exceeds the evidence byte limit"
        );
        if let Some(previous) = &cursor.previous {
            let commit = block
                .block()
                .last_commit
                .as_ref()
                .context("missing previous commit")?;
            verify::verify_commit(
                &self.chain_id,
                cursor.height.try_into()?,
                previous.id,
                &previous.validators,
                commit,
            )?;
        }
        Ok(())
    }

    fn execute_committed(
        &mut self,
        block: &ReceivedBlock,
        commit: &Commit,
        successor: Option<&ReceivedBlock>,
        detected: Vec<types::Evidence>,
    ) -> Result<AppliedBlock> {
        let cursor = self.cursor()?.clone();
        let height = block.block().header.height;
        let request = cursor.begin_request(block);
        let begin_request_hash = Hasher::digest(request.encode_to_vec());
        self.app.begin(request)?;
        let deliveries = block
            .block()
            .data
            .iter()
            .map(|tx| self.app.deliver(tx).map(|outcome| outcome.deliver()))
            .collect::<Result<Vec<_>>>()?;
        let end = self.app.end(height.into())?;
        let mut future = proposer::update(Some(&cursor.next_validators), &end.validator_updates)?;
        if future.proposer.is_some() {
            proposer::increment(&mut future, 1)?;
        }
        let params_update = end.consensus_param_updates.as_ref();
        let next_params = update_params(cursor.params, params_update)?;
        let app_version = if params_update.is_some() {
            Some(params_app_version(&next_params))
        } else {
            cursor.app_version
        };
        let expected_root = successor
            .map(|next| {
                next.block()
                    .header
                    .app_hash
                    .as_bytes()
                    .try_into()
                    .context("signed successor lacks a 32-byte AppHash")
            })
            .transpose()?;
        let evidence_hashes = block
            .evidence()
            .evidence
            .iter()
            .map(minter_p2p::evidence::hash)
            .collect::<Result<Vec<_>>>()?;
        let validator_change = if cursor
            .previous
            .as_ref()
            .is_none_or(|previous| previous.validators.hash() != cursor.validators.hash())
            || self.app.store.evidence_index_start()?.is_none()
        {
            Some(proposer::to_wire(&cursor.validators).encode_to_vec())
        } else {
            None
        };
        let evidence = self.evidence.after_commit(
            height.value(),
            block.block().header.time,
            &next_params.evidence,
            &evidence_hashes,
            detected,
        )?;
        let pending = evidence.as_ref().map(evidence::Pool::encode);
        let results_hash = results_hash(&deliveries);
        let new_cursor = Cursor {
            height: height.value(),
            root: expected_root.unwrap_or_default(),
            previous: Some(PreviousBlock {
                id: block.id(),
                time: block.block().header.time,
                validators: cursor.validators,
            }),
            validators: cursor.next_validators,
            next_validators: future,
            params: next_params,
            app_version,
            results_hash,
            ..cursor
        };
        if let Some(next) = successor {
            new_cursor.check_header(next, &self.chain_id)?;
        }
        let mut report = AppliedBlock {
            height: height.value(),
            app_hash: String::new(),
            block_hash: hex::encode(block.id().hash.as_bytes()),
            part_set_hash: hex::encode(block.id().part_set_header.hash.as_bytes()),
            parts: block.id().part_set_header.total,
            results_hash: hex::encode(results_hash.as_bytes()),
            transactions: deliveries.len(),
            validator_updates: end.validator_updates.len(),
            begin_request_sha256: hex::encode(begin_request_hash),
            deliver_responses_sha256: hex::encode(delivery_digest(&deliveries)),
            end_response_sha256: hex::encode(Hasher::digest(end.encode_to_vec())),
            validators_hash: hex::encode(block.block().header.validators_hash.as_bytes()),
            next_validators_hash: hex::encode(block.block().header.next_validators_hash.as_bytes()),
            proposer_state_sha256: new_cursor.proposer_state_sha256(),
        };
        let stored = BlockRecord {
            block_response: block.wire().to_vec(),
            commit: Some(types::Commit::from(commit.clone())),
            deliver_tx: deliveries,
            end_block: Some(end),
        };
        let record = stored.encode_to_vec();
        self.app
            .pending
            .as_mut()
            .context("missing staged block")?
            .chain
            .native_sync = Some(new_cursor);
        let applied = self.app.commit(Some(NativeBlockWrite {
            expected_root,
            record: &record,
            validators: validator_change.as_deref(),
            committed_evidence: &evidence_hashes,
            pending_evidence: pending.as_deref(),
        }))?;
        if let Some(evidence) = evidence {
            self.evidence = evidence;
        }
        report.app_hash = hex::encode(applied.data);
        if let Some(pool) = &mut self.mempool {
            pool.committed(&block.block().data, &stored.deliver_tx);
            pool.last_commit_round = commit.round.into();
        }
        self.maintain_history_after_commit();
        Ok(report)
    }
}

fn read_genesis_identity(path: &Path, network: u8) -> Result<crate::checkpoint::Identity> {
    let raw = fs::read(path).with_context(|| format!("read genesis {}", path.display()))?;
    let doc: Genesis<&RawValue> =
        serde_json::from_slice(&raw).context("decode Tendermint genesis")?;
    checked_genesis_identity(&doc, Hasher::digest(&raw), network)
}

fn checked_genesis_identity<T>(
    doc: &Genesis<T>,
    digest: [u8; 32],
    network: u8,
) -> Result<crate::checkpoint::Identity> {
    if doc.chain_id.as_str() == "minter-mainnet-5" {
        ensure!(
            hex::encode(digest) == MAINNET_GENESIS && network == 1,
            "minter-mainnet-5 requires the official genesis and Minter transaction chain ID 1"
        );
    }
    let height = u64::try_from(doc.initial_height)
        .context("negative genesis initial height")?
        .max(1);
    Ok(crate::checkpoint::Identity {
        chain_id: doc.chain_id.clone(),
        initial_height: height,
        transaction_chain_id: network,
        genesis_sha256: Some(digest),
    })
}

fn initialize(app: &mut Application, path: &Path) -> Result<chain::Id> {
    use minter_state::GenesisStage;
    let control = app.store.maintenance_control().clone();
    let raw = crate::genesis::control::read_file(path, &control)?;
    let digest = crate::genesis::control::hash_bytes(&raw, GenesisStage::FileHashBytes, &control)?;
    let doc: Genesis<Box<RawValue>> =
        crate::genesis::control::decode(&raw, GenesisStage::EnvelopeBytes, &control)
            .context("decode Tendermint genesis")?;
    checked_genesis_identity(&doc, digest, app.network)?;
    let chain_id = doc.chain_id.clone();
    let app_state = String::from(Box::<str>::from(doc.app_state)).into_bytes();
    drop(raw);
    let request = abci::RequestInitChain {
        time: Some(doc.genesis_time.into()),
        chain_id: doc.chain_id.to_string(),
        consensus_params: Some(doc.consensus_params.clone().into()),
        initial_height: doc.initial_height,
        app_state_bytes: app_state.into(),
        ..Default::default()
    };
    app.init_with(request, move |chain, response| {
        let validators = if response.validators.is_empty() {
            let mut validators = validator_set(doc.validators)?;
            for validator in &mut validators.validators {
                validator.proposer_priority = Default::default();
            }
            proposer::increment(&mut validators, 1)?;
            validators
        } else {
            proposer::update(None, &response.validators)?
        };
        let mut next_validators = validators.clone();
        proposer::increment(&mut next_validators, 1)?;
        let params = update_params(doc.consensus_params, response.consensus_params.as_ref())?;
        // Tendermint sets the header app version from Info at handshake. InitChain
        // or EndBlock consensus parameter updates replace it with the params version.
        let app_version = if response.consensus_params.is_some() {
            params_app_version(&params)
        } else {
            APP_PROTOCOL_VERSION
        };
        let genesis_app_hash = if response.app_hash.is_empty() {
            doc.app_hash.as_bytes().to_vec()
        } else {
            response.app_hash.to_vec()
        };
        chain.native_sync = Some(Cursor {
            genesis_sha256: Some(digest),
            genesis_time: Some(doc.genesis_time),
            genesis_app_hash,
            checkpoint_sha256: None,
            history_context: None,
            app_version: Some(app_version),
            height: chain.initial_height - 1,
            root: [0; 32],
            previous: None,
            next_validators,
            validators,
            params,
            results_hash: results_hash(&[]),
        });
        Ok(true)
    })?;
    Ok(chain_id)
}

impl Cursor {
    pub(crate) fn set_genesis_root(&mut self, head: IavlHead) -> Result<()> {
        ensure!(
            self.previous.is_none() && self.height == head.height,
            "native genesis height differs"
        );
        self.root = head.app_hash;
        Ok(())
    }

    pub(crate) fn set_committed_root(&mut self, root: [u8; 32]) {
        self.root = root;
    }

    fn has_proposer_state(&self) -> bool {
        self.validators.proposer.is_some()
            && self.next_validators.proposer.is_some()
            && self
                .previous
                .as_ref()
                .is_none_or(|block| block.validators.proposer.is_some())
    }

    fn proposer_state_sha256(&self) -> Option<String> {
        self.has_proposer_state().then(|| {
            let mut bytes = Vec::new();
            for set in self
                .previous
                .iter()
                .map(|block| &block.validators)
                .chain([&self.validators, &self.next_validators])
            {
                bytes.extend(proposer::to_wire(set).encode_length_delimited_to_vec());
            }
            hex::encode(Hasher::digest(bytes))
        })
    }

    fn check_recovery(&self, app: &Application, chain: &chain::Id) -> Result<()> {
        let head = app.head().context("missing committed application state")?;
        ensure!(
            self.height == head.height && self.root == head.app_hash,
            "native sync state differs from RocksDB head"
        );
        if self.previous.is_none() {
            ensure!(
                app.chain
                    .as_ref()
                    .is_some_and(|state| self.height == state.initial_height - 1)
                    && self.genesis_sha256.is_some()
                    && self.genesis_time.is_some()
                    && self.checkpoint_sha256.is_none()
                    && self.history_context.is_none()
                    && app.store.native_block_base()?.is_none(),
                "incomplete native genesis state"
            );
            return Ok(());
        }
        let previous = self
            .previous
            .as_ref()
            .context("missing native block at RocksDB head")?;
        let raw = app
            .store
            .native_block_at(self.height)?
            .context("missing stored native block")?;
        let record = BlockRecord::decode(raw.as_slice())?;
        let block = ReceivedBlock::decode(record.block_response)?;
        let header = &block.block().header;
        let checkpoint_head = app.store.checkpoint().is_some_and(|checkpoint| {
            self.checkpoint_sha256.is_some() && checkpoint.height == self.height
        });
        ensure!(
            header.chain_id == *chain
                && header.height.value() == self.height
                && block.id() == previous.id
                && header.time == previous.time
                && header.validators_hash == previous.validators.hash()
                && header.next_validators_hash == self.validators.hash()
                && if checkpoint_head {
                    record.end_block.is_none() && record.deliver_tx.is_empty()
                } else {
                    record.end_block.is_some()
                        && results_hash(&record.deliver_tx) == self.results_hash
                },
            "stored native block differs from sync state"
        );
        let commit: Commit = record.commit.context("missing stored commit")?.try_into()?;
        verify::verify_commit(
            chain,
            header.height,
            block.id(),
            &previous.validators,
            &commit,
        )
    }

    fn check_header(&self, received: &ReceivedBlock, chain: &chain::Id) -> Result<()> {
        let block = received.block();
        let header = &block.header;
        let height = header.height;
        ensure!(
            header.chain_id == *chain && self.height.checked_add(1) == Some(height.value()),
            "unexpected chain or block height {height}"
        );
        ensure!(
            received.encoded_len() as u64 <= self.params.block.max_bytes,
            "block exceeds consensus size limit at height {height}"
        );
        let version = self
            .app_version
            .unwrap_or_else(|| params_app_version(&self.params));
        ensure!(
            header.version.app == version && header.consensus_hash == consensus_hash(&self.params)?,
            "consensus parameters differ at height {height}"
        );
        ensure!(
            header.validators_hash == self.validators.hash()
                && header.next_validators_hash == self.next_validators.hash(),
            "validator sets differ at height {height}"
        );
        ensure!(
            self.validators.validator(header.proposer_address).is_some(),
            "block proposer is absent from the validator set at height {height}"
        );
        ensure!(
            header.last_results_hash == Some(self.results_hash),
            "ABCI result root differs in the header at height {height}"
        );
        let expected = if let Some(previous) = &self.previous {
            let commit = block
                .last_commit
                .as_ref()
                .context("missing previous commit")?;
            ensure!(
                header.last_block_id == Some(previous.id)
                    && commit.block_id == previous.id
                    && commit.height.value() == self.height,
                "previous BlockID differs at height {height}"
            );
            ensure!(
                header.time > previous.time
                    && header.time == verify::median_time(commit, &previous.validators)?,
                "invalid consensus timestamp at height {height}"
            );
            self.root.as_slice()
        } else {
            ensure!(
                header.last_block_id.is_none()
                    && block.last_commit.is_none()
                    && Some(header.time) == self.genesis_time,
                "first block differs from genesis"
            );
            self.genesis_app_hash.as_slice()
        };
        ensure!(
            header.last_commit_hash == Some(verify::commit_hash(block.last_commit.as_ref())),
            "LastCommitHash differs at height {height}"
        );
        ensure!(
            header.app_hash.as_bytes() == expected,
            "previous AppHash differs at height {height}"
        );
        Ok(())
    }

    fn begin_request(&self, received: &ReceivedBlock) -> abci::RequestBeginBlock {
        let block = received.block();
        let mut last_commit = abci::LastCommitInfo::default();
        if let Some((previous, commit)) = self.previous.as_ref().zip(block.last_commit.as_ref()) {
            last_commit.round = commit.round.into();
            last_commit.votes = previous
                .validators
                .validators()
                .iter()
                .zip(&commit.signatures)
                .map(|(validator, signature)| abci::VoteInfo {
                    validator: Some(abci_validator(validator.address, validator.power.into())),
                    signed_last_block: !signature.is_absent(),
                })
                .collect();
        }
        abci::RequestBeginBlock {
            hash: received.id().hash.as_bytes().to_vec().into(),
            header: Some(received.proto_header().clone()),
            last_commit_info: Some(last_commit),
            byzantine_validators: abci_evidence(&block.evidence.evidence),
        }
    }
}

fn validator_set(validators: Vec<Info>) -> Result<Set> {
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for validator in &validators {
        ensure!(
            validator.address == account::Id::from(validator.pub_key)
                && seen.insert(validator.address)
                && validator.power() != 0,
            "invalid or duplicate validator {}",
            validator.address
        );
        total = total
            .checked_add(validator.power())
            .context("voting power overflow")?;
    }
    ensure!(
        total != 0 && total <= Set::MAX_TOTAL_VOTING_POWER,
        "invalid total voting power"
    );
    Ok(Set::without_proposer(validators))
}

fn update_params(
    mut params: consensus::Params,
    updates: Option<&abci::ConsensusParams>,
) -> Result<consensus::Params> {
    if let Some(updates) = updates {
        if let Some(block) = &updates.block {
            params.block.max_bytes = block.max_bytes.try_into()?;
            params.block.max_gas = block.max_gas;
        }
        if let Some(evidence) = &updates.evidence {
            params.evidence = (*evidence).try_into()?;
        }
        if let Some(validator) = &updates.validator {
            params.validator = validator.clone().try_into()?;
        }
        if let Some(version) = &updates.version {
            params.version = Some((*version).try_into()?);
        }
    }
    ensure!(
        (1..=MAX_BLOCK_BYTES).contains(&params.block.max_bytes) && params.block.max_gas >= -1,
        "invalid consensus block size or gas limit"
    );
    Ok(params)
}

fn consensus_hash(params: &consensus::Params) -> Result<Hash> {
    let hashed = types::HashedParams {
        block_max_bytes: params.block.max_bytes.try_into()?,
        block_max_gas: params.block.max_gas,
    };
    Ok(Hash::Sha256(Hasher::digest(hashed.encode_to_vec())))
}

fn params_app_version(params: &consensus::Params) -> u64 {
    params.version.as_ref().map_or(0, |version| version.app)
}

fn results_hash(deliveries: &[abci::ResponseDeliverTx]) -> Hash {
    let leaves = deliveries
        .iter()
        .map(|response| {
            abci::ResponseDeliverTx {
                code: response.code,
                data: response.data.clone(),
                gas_wanted: response.gas_wanted,
                gas_used: response.gas_used,
                ..Default::default()
            }
            .encode_to_vec()
        })
        .collect::<Vec<_>>();
    Hash::Sha256(merkle::simple_hash_from_byte_vectors::<Hasher>(&leaves))
}

fn delivery_digest(deliveries: &[abci::ResponseDeliverTx]) -> [u8; 32] {
    use sha2::Digest;
    let mut hasher = Hasher::new();
    for response in deliveries {
        hasher.update(response.encode_length_delimited_to_vec());
    }
    hasher.finalize().into()
}

fn abci_validator(address: account::Id, power: i64) -> abci::Validator {
    abci::Validator {
        address: address.as_bytes().to_vec().into(),
        power,
    }
}

fn abci_evidence(evidence: &[types::Evidence]) -> Vec<abci::Evidence> {
    evidence
        .iter()
        .flat_map(|item| minter_p2p::evidence::to_abci(item).expect("validated block evidence"))
        .collect()
}

#[cfg(test)]
mod checkpoint_genesis_tests {
    use super::*;

    #[test]
    fn invalid_native_genesis_parameters_do_not_publish_an_application_head() -> Result<()> {
        let (directory, genesis) = fixture();
        let mut invalid: serde_json::Value = serde_json::from_slice(&fs::read(&genesis)?)?;
        invalid["consensus_params"]["block"]["max_bytes"] = "0".into();
        let invalid_path = directory.path().join("invalid-parameters.json");
        fs::write(&invalid_path, serde_json::to_vec(&invalid)?)?;
        let database = directory.path().join("state");
        let error = NativeSync::open(&database, 1, Some(&invalid_path), None)
            .err()
            .context("invalid native parameters should fail")?;
        assert!(
            format!("{error:#}").contains("invalid consensus block size"),
            "{error:#}"
        );
        let store = minter_state::BlockStore::open(&database)?;
        assert_eq!(store.head(), None);
        assert_eq!(store.metadata()?, None);
        drop(store);
        assert_eq!(
            NativeSync::open(&database, 1, Some(&genesis), None)?
                .head()?
                .height,
            100
        );
        Ok(())
    }

    #[test]
    fn legacy_genesis_can_adopt_native_identity_without_rebuilding_iavl() -> Result<()> {
        let (directory, genesis) = fixture();
        let database = directory.path().join("state");
        let sync = NativeSync::open(&database, 1, Some(&genesis), None)?;
        let head = sync.head()?;
        drop(sync);
        let store = minter_state::BlockStore::open(&database)?;
        let mut metadata: serde_json::Value = serde_json::from_slice(&store.metadata()?.unwrap())?;
        metadata.as_object_mut().unwrap().remove("native_sync");
        store.update_genesis_metadata(head.height, &serde_json::to_vec(&metadata)?)?;
        drop(store);
        let control = OperationControl::default().with_genesis_progress(|phase, _| {
            assert!(
                !matches!(
                    phase,
                    minter_state::GenesisStage::IavlMutations
                        | minter_state::GenesisStage::IavlNodes
                ),
                "legacy adoption must preserve the existing tree"
            );
        });
        let sync = NativeSync::open_with_control(&database, 1, Some(&genesis), None, &control)?;
        assert_eq!(sync.head()?, head);
        assert_eq!(sync.cursor()?.root, head.app_hash);
        assert_eq!(
            sync.cursor()?.genesis_sha256,
            Some(Hasher::digest(fs::read(&genesis)?))
        );
        drop(sync);
        assert_eq!(NativeSync::open(&database, 1, None, None)?.head()?, head);
        Ok(())
    }

    #[test]
    fn native_genesis_identity_survives_restart_before_the_first_block() -> Result<()> {
        let (directory, genesis) = fixture();
        let path = directory.path().join("state");
        let sync = NativeSync::open(&path, 1, Some(&genesis), None)?;
        let head = sync.head()?;
        let digest = sync.cursor()?.genesis_sha256;
        assert_eq!(head.height, 100);
        assert_eq!(sync.cursor()?.root, head.app_hash);
        drop(sync);
        let sync = NativeSync::open(&path, 1, None, None)?;
        assert_eq!(sync.head()?, head);
        assert_eq!(sync.cursor()?.genesis_sha256, digest);
        drop(sync);
        let original: serde_json::Value = serde_json::from_slice(&fs::read(&genesis)?)?;
        for change_time in [true, false] {
            let mut changed = original.clone();
            if change_time {
                changed["genesis_time"] = "2025-01-01T08:00:01Z".into();
            } else {
                changed["consensus_params"]["evidence"]["max_age_num_blocks"] = "99999".into();
            }
            assert_eq!(changed["app_state"], original["app_state"]);
            let changed_path = directory.path().join("changed-genesis.json");
            fs::write(&changed_path, serde_json::to_vec(&changed)?)?;
            let error = NativeSync::open(&path, 1, Some(&changed_path), None)
                .err()
                .context("changed outer genesis must be refused")?;
            assert!(
                format!("{error:#}").contains("genesis differs"),
                "{error:#}"
            );
            assert_eq!(NativeSync::open(&path, 1, None, None)?.head()?, head);
        }
        Ok(())
    }

    pub(super) fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("genesis.json");
        let bytes = include_str!("../tests/fixtures/native-four-validator-genesis.json")
            .replace("\r\n", "\n");
        fs::write(&path, bytes).unwrap();
        (directory, path)
    }

    #[test]
    fn checkpoint_genesis_reads_exact_custom_identity() {
        let (_directory, path) = fixture();
        let identity = read_genesis_identity(&path, 1).unwrap();
        assert_eq!(identity.chain_id.as_str(), "minter-rust-evidence-fixture");
        assert_eq!(identity.initial_height, 101);
        assert_eq!(identity.transaction_chain_id, 1);
        assert_eq!(
            hex::encode(identity.genesis_sha256.unwrap()),
            "28399dba9e9780651f437c01f1adcfde25d595d219e30dba8524e3052521bed3"
        );
    }

    #[test]
    fn checkpoint_genesis_distinguishes_changed_file_bytes() {
        let (_directory, path) = fixture();
        let original = read_genesis_identity(&path, 1).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        let changed = read_genesis_identity(&path, 1).unwrap();
        assert_eq!(changed.chain_id, original.chain_id);
        assert_ne!(changed.genesis_sha256, original.genesis_sha256);
    }

    #[test]
    fn checkpoint_genesis_rejects_a_synthetic_mainnet_file() {
        let (_directory, path) = fixture();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["chain_id"] = "minter-mainnet-5".into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            read_genesis_identity(&path, 1)
                .err()
                .unwrap()
                .to_string()
                .contains("official genesis")
        );
    }
}
