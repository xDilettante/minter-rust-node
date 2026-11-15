use super::{Cursor, NativeSync, checkpoint, params_app_version, proposer};
use crate::{Chain, state::magnitude};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use minter_p2p::{ReceivedBlock, snapshot::SnapshotFile};
use minter_state::{Checkpoint, ExportStage};
use prost::Message;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};
use tendermint_proto::v0_34::{state, version};

pub struct ExportedSnapshot {
    pub snapshot: Checkpoint,
    pub checkpoint_sha256: Option<[u8; 32]>,
    pub history_context: Option<checkpoint::HistoryContext>,
}

struct CheckpointPackage {
    state: Vec<u8>,
    blocks: Vec<u8>,
    manifest: checkpoint::Manifest,
}

impl NativeSync {
    /// Export the current committed state, with an optional signed checkpoint anchor.
    pub fn export_go_snapshot(
        &self,
        output: &Path,
        anchor: Option<[ReceivedBlock; 3]>,
    ) -> Result<ExportedSnapshot> {
        self.export_snapshot(output, anchor, false)
    }

    pub fn export_snapshot_with_history(
        &self,
        output: &Path,
        anchor: [ReceivedBlock; 3],
    ) -> Result<ExportedSnapshot> {
        self.export_snapshot(output, Some(anchor), true)
    }

    fn export_snapshot(
        &self,
        output: &Path,
        anchor: Option<[ReceivedBlock; 3]>,
        include_history: bool,
    ) -> Result<ExportedSnapshot> {
        let control = self.app.store.maintenance_control();
        control.check()?;
        ensure!(!output.try_exists()?, "snapshot destination already exists");
        self.ensure_history_context()?;
        let reader = self.app.store.reader().context("missing native state")?;
        let state = reader.latest()?;
        let head = state.head();
        let chain: Chain = serde_json::from_slice(
            &state
                .execution_metadata()?
                .context("missing execution metadata")?,
        )?;
        chain.frozen_references.validate(head.height, |height| {
            control.check()?;
            state.value(&crate::frozen::key(height))
        })?;
        let cursor = chain
            .native_sync
            .as_ref()
            .context("missing native sync cursor")?;
        ensure!(
            cursor.height == head.height
                && cursor.root == head.app_hash
                && chain.id == self.chain_id.as_str(),
            "snapshot state differs from the native sync cursor"
        );
        let previous = cursor
            .previous
            .as_ref()
            .context("a snapshot needs a committed block")?;
        ensure!(
            !chain.block_times.is_empty() && chain.legacy_reward_price_time.is_none(),
            "incomplete Go snapshot metadata"
        );
        let validators = cursor
            .next_validators
            .validators()
            .iter()
            .map(|validator| {
                let key = validator
                    .pub_key
                    .ed25519()
                    .context("unsupported validator key in snapshot")?;
                Ok(json!({
                    "pub_key": {"Sum": {"type": "tendermint.crypto.PublicKey_Ed25519",
                        "value": {"ed25519": STANDARD.encode(key.as_bytes())}}},
                    "power": validator.power.value().to_string(),
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let block_times = chain
            .block_times
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>();
        let versions = chain
            .versions
            .iter()
            .map(|version| json!({"Name": version.name, "Height": version.height.to_string()}))
            .collect::<Vec<_>>();
        let mut emission = magnitude(&chain.emission);
        if emission.is_empty() {
            emission.push(0);
        }
        let mut metadata = vec![
            ("validators", serde_json::to_vec(&validators)?),
            ("height", head.height.to_be_bytes().to_vec()),
            ("hash", head.app_hash.to_vec()),
            ("versions", serde_json::to_vec(&versions)?),
            ("blockDelta", serde_json::to_vec(&block_times)?),
            (
                "startHeight",
                (chain.initial_height - 1).to_be_bytes().to_vec(),
            ),
            ("emission", emission),
        ];
        if let Some(price) = &chain.reward_price {
            metadata.push(("price", price.encode()));
        }
        let block_hash = previous.id.hash.as_bytes().try_into()?;
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        let nodes = state.write_go_snapshot_with_control(&metadata, file.as_file_mut(), control)?;
        file.as_file().sync_all()?;
        let snapshot = SnapshotFile::new_with_progress(file.reopen()?, head.height, |done| {
            control.export_progress(ExportStage::SnapshotHashBytes, done)
        })?;
        let advertised = snapshot.advertisement();
        let snapshot_manifest = Checkpoint {
            format: advertised.format,
            network: chain.id,
            height: head.height,
            block_hash,
            app_hash: head.app_hash,
            snapshot_sha256: advertised.hash.as_slice().try_into()?,
            bytes: snapshot.bytes(),
            nodes,
        };
        drop(snapshot);
        let mut files = vec![("snapshot.zlib", file)];
        let history_context = if include_history
            && let Some(from) = self.required_history_floor()?
        {
            let mut history = tempfile::NamedTempFile::new_in(parent)?;
            self.export_history_context(head.height, from, history.as_file_mut())?;
            history.as_file().sync_all()?;
            let view =
                SnapshotFile::new_context_with_progress(history.reopen()?, head.height, |done| {
                    control.export_progress(ExportStage::HistoryHashBytes, done)
                })?;
            let description = checkpoint::HistoryContext {
                from,
                bytes: view.bytes(),
                sha256: view.advertisement().hash.as_slice().try_into()?,
            };
            description.validate(head.height, chain.initial_height)?;
            files.push(("history.pb", history));
            Some(description)
        } else {
            None
        };
        let (mut manifest_bytes, packaged) = if let Some(blocks) = anchor {
            let mut package = self.checkpoint_package(
                &snapshot_manifest,
                cursor,
                chain.initial_height,
                &chain.frozen_references,
                blocks,
            )?;
            package.manifest.history_context = history_context.clone();
            files.push(("state.pb", write_temporary(parent, &package.state)?));
            files.push(("blocks.pb", write_temporary(parent, &package.blocks)?));
            (serde_json::to_vec_pretty(&package.manifest)?, true)
        } else {
            (serde_json::to_vec_pretty(&snapshot_manifest)?, false)
        };
        manifest_bytes.push(b'\n');
        ensure!(
            manifest_bytes.len() as u64 <= checkpoint::MAX_MANIFEST_BYTES,
            "snapshot manifest exceeds supported size"
        );
        let checkpoint_sha256 = packaged.then(|| Sha256::digest(&manifest_bytes).into());
        files.push(("manifest.json", write_temporary(parent, &manifest_bytes)?));
        // All large files are complete. Finish publication even if a late stop
        // arrives; the manifest is last and existing destinations are never used.
        control.check()?;
        std::fs::create_dir(output).context("create snapshot destination")?;
        for (name, file) in files {
            file.persist_noclobber(output.join(name))
                .with_context(|| format!("publish snapshot {name}"))?;
        }
        self.app
            .store
            .remember_exported_snapshot(&snapshot_manifest)?;
        Ok(ExportedSnapshot {
            snapshot: snapshot_manifest,
            checkpoint_sha256,
            history_context,
        })
    }

    fn checkpoint_package(
        &self,
        snapshot: &Checkpoint,
        cursor: &Cursor,
        initial_height: u64,
        frozen_references: &crate::frozen::References,
        anchor: [ReceivedBlock; 3],
    ) -> Result<CheckpointPackage> {
        let control = self.app.store.maintenance_control();
        control.check()?;
        let custom = self.chain_id.as_str() != "minter-mainnet-5";
        if custom {
            ensure!(
                cursor.genesis_sha256.is_some(),
                "custom checkpoint export needs the preserved genesis digest"
            );
        } else {
            crate::checkpoint::validate_network(snapshot, self.app.network)?;
            ensure!(
                initial_height == checkpoint::MAINNET_INITIAL_HEIGHT,
                "mainnet checkpoint initial height differs"
            );
        }
        let height = i64::try_from(snapshot.height)?;
        let previous = cursor
            .previous
            .as_ref()
            .context("missing checkpoint block")?;
        let state = state::State {
            version: Some(state::Version {
                consensus: Some(version::Consensus {
                    block: 11,
                    app: cursor
                        .app_version
                        .unwrap_or_else(|| params_app_version(&cursor.params)),
                }),
                software: "0.34.19".into(),
            }),
            chain_id: snapshot.network.clone(),
            initial_height: initial_height.try_into()?,
            last_block_height: height,
            last_block_id: Some(previous.id.into()),
            last_block_time: Some(previous.time.into()),
            next_validators: Some(proposer::to_wire(&cursor.next_validators)),
            validators: Some(proposer::to_wire(&cursor.validators)),
            last_validators: Some(proposer::to_wire(&previous.validators)),
            last_height_validators_changed: height
                .checked_add(2)
                .context("invalid checkpoint height")?,
            consensus_params: Some(cursor.params.clone().into()),
            last_height_consensus_params_changed: height
                .checked_add(1)
                .context("invalid checkpoint height")?,
            last_results_hash: cursor.results_hash.as_bytes().to_vec(),
            app_hash: snapshot.app_hash.to_vec(),
        }
        .encode_to_vec();
        let mut blocks = Vec::new();
        for block in anchor {
            control.check()?;
            block.write(&mut blocks)?;
        }
        let manifest = checkpoint::Manifest {
            snapshot: snapshot.clone(),
            consensus_state_sha256: checkpoint::hash_anchor_bytes(&state, control)?,
            blocks_sha256: checkpoint::hash_anchor_bytes(&blocks, control)?,
            proposer_priorities: cursor.has_proposer_state(),
            history_context: None,
            frozen_references: frozen_references.clone(),
            genesis_sha256: custom.then(|| {
                hex::encode(
                    cursor
                        .genesis_sha256
                        .expect("checked custom genesis digest"),
                )
            }),
            transaction_chain_id: custom.then_some(self.app.network),
        };
        let identity = crate::checkpoint::Identity {
            chain_id: self.chain_id.clone(),
            initial_height,
            transaction_chain_id: self.app.network,
            genesis_sha256: cursor.genesis_sha256,
        };
        checkpoint::verify_anchor(&manifest, &identity, &state, &blocks, control)?;
        control.check()?;
        Ok(CheckpointPackage {
            state,
            blocks,
            manifest,
        })
    }

    pub fn verify_snapshot_anchor(&self, checkpoint: &Checkpoint) -> Result<()> {
        ensure!(
            checkpoint.height <= self.head()?.height,
            "snapshot anchor exceeds the committed head"
        );
        ensure!(
            checkpoint.format == 1 && checkpoint.network == self.chain_id.as_str(),
            "served snapshot is for a different network or format"
        );
        if self.app.store.known_native_snapshot(checkpoint)? {
            return Ok(());
        }
        let (header, commit) = self.history_header(checkpoint.height)?;
        ensure!(
            header.hash().as_bytes() == checkpoint.block_hash,
            "snapshot block hash differs from the archive"
        );
        let head = self.head()?;
        if checkpoint.height == head.height {
            ensure!(
                checkpoint.app_hash == head.app_hash,
                "snapshot AppHash differs from the committed head"
            );
        } else {
            let (next, _) = self.history_header(
                checkpoint
                    .height
                    .checked_add(1)
                    .context("invalid snapshot height")?,
            )?;
            ensure!(
                next.app_hash.as_bytes() == checkpoint.app_hash
                    && next.last_block_id == Some(commit.block_id),
                "snapshot AppHash differs from the committed successor"
            );
        }
        Ok(())
    }

    /// Call only after all configured snapshot files have passed their checks.
    pub fn remember_served_snapshot(&self, checkpoint: &Checkpoint) -> Result<()> {
        self.verify_snapshot_anchor(checkpoint)?;
        self.app.store.remember_served_snapshot(checkpoint)
    }
}

fn write_temporary(parent: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}
