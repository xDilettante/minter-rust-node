use super::{Cursor, NativeSync, proposer};
use anyhow::{Context, Result, ensure};
use minter_p2p::{
    ReceivedBlock,
    consensus::{EvidenceKey, PendingEvidence},
    evidence as wire,
};
use minter_state::BlockStore;
use prost::Message;
use std::collections::{BTreeMap, BTreeSet};
use tendermint::{Time, block::Commit, evidence::Params, validator::Set};
use tendermint_proto::v0_34::types::{self, evidence::Sum};

const MAX_PENDING_BYTES: usize = 4 * wire::MAX_MESSAGE;

mod history;
pub use history::NativeRetentionReport;

#[test]
fn raw_light_evidence_has_the_same_abci_bytes_as_go() -> Result<()> {
    let reference: serde_json::Value = serde_json::from_str(include_str!(
        "../../../minter-p2p/tests/fixtures/go-light-wire.json"
    ))?;
    for case in reference["cases"].as_array().context("fixture cases")? {
        if let Some(expected) = case["abciEvidence"].as_array() {
            let proof = types::Evidence::decode(
                hex::decode(case["proof"].as_str().context("proof")?)?.as_slice(),
            )?;
            let proof = wire::canonicalize(&proof)?;
            let actual = super::abci_evidence(&[proof])
                .iter()
                .map(|entry| hex::encode(entry.encode_to_vec()))
                .collect::<Vec<_>>();
            let expected = expected
                .iter()
                .map(|entry| entry.as_str().context("Go ABCI bytes"))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(actual, expected, "{}", case["name"]);
        }
    }
    Ok(())
}

/// Upgrade archives written before historical validator sets were indexed.
/// EndBlock changes become effective at H+2, as in Tendermint 0.34.19.
pub(super) fn restore_history(
    app: &crate::Application,
    chain_id: &tendermint::chain::Id,
    anchor: Option<&Cursor>,
) -> Result<()> {
    let control = app.store.maintenance_control();
    control.check()?;
    let Some(base) = app.store.native_block_base()? else {
        return Ok(());
    };
    if app
        .store
        .evidence_index_start()?
        .is_some_and(|start| start <= base)
    {
        return Ok(());
    }
    let chain = app.chain.as_ref().context("missing chain history")?;
    let cursor = chain
        .native_sync
        .as_ref()
        .context("missing native cursor")?;
    let (mut validators, mut next, mut checkpoint_next) = if let Some(anchor) = anchor {
        ensure!(
            app.store
                .checkpoint()
                .is_some_and(|checkpoint| checkpoint.height == base)
                && anchor.height == base,
            "history anchor differs from the database checkpoint"
        );
        (
            anchor
                .previous
                .as_ref()
                .context("missing checkpoint validators")?
                .validators
                .clone(),
            anchor.validators.clone(),
            Some(anchor.next_validators.clone()),
        )
    } else {
        ensure!(
            app.store.checkpoint().is_none(),
            "upgrade this database once with --checkpoint and --checkpoint-sha256 to restore its validator history"
        );
        ensure!(
            base == chain.initial_height,
            "native archive does not begin at genesis"
        );
        let initial = crate::genesis_reply(&chain.initial_validators)?;
        let validators = proposer::update(None, &initial.validators)?;
        let mut next = validators.clone();
        proposer::increment(&mut next, 1)?;
        (validators, next, None)
    };
    let mut previous_hash = None;
    let mut last_id = None;
    let mut last_time = None;
    control.maintenance_progress(minter_state::MaintenanceStage::NativeIndexBlocks, 0)?;
    for height in base..=cursor.height {
        control.check()?;
        let raw = app
            .store
            .native_block_at(height)?
            .with_context(|| format!("missing native block {height} during history upgrade"))?;
        let record = super::BlockRecord::decode(raw.as_slice())?;
        let block = ReceivedBlock::decode(record.block_response)?;
        let header = &block.block().header;
        ensure!(
            header.height.value() == height
                && header.chain_id == *chain_id
                && header.validators_hash == validators.hash()
                && header.next_validators_hash == next.hash()
                && last_id.is_none_or(|id| header.last_block_id == Some(id)),
            "validator history differs from native block {height}"
        );
        let future = if let Some(future) = checkpoint_next.take() {
            ensure!(record.end_block.is_none(), "unexpected checkpoint EndBlock");
            future
        } else {
            let end = record.end_block.context("missing historical EndBlock")?;
            let mut future = proposer::update(Some(&next), &end.validator_updates)?;
            if future.proposer.is_some() {
                proposer::increment(&mut future, 1)?;
            }
            future
        };
        let hash = validators.hash();
        let change =
            (previous_hash != Some(hash)).then(|| proposer::to_wire(&validators).encode_to_vec());
        let committed = block
            .evidence()
            .evidence
            .iter()
            .map(wire::hash)
            .collect::<Result<Vec<_>>>()?;
        if change.is_some() || !committed.is_empty() {
            app.store
                .backfill_evidence_height(height, change.as_deref(), &committed)?;
        }
        previous_hash = Some(hash);
        last_id = Some(block.id());
        last_time = Some(header.time);
        validators = next;
        next = future;
        control.maintenance_progress(
            minter_state::MaintenanceStage::NativeIndexBlocks,
            height - base + 1,
        )?;
    }
    let previous = cursor
        .previous
        .as_ref()
        .context("missing committed history head")?;
    ensure!(
        last_id == Some(previous.id)
            && last_time == Some(previous.time)
            && previous_hash == Some(previous.validators.hash())
            && validators.hash() == cursor.validators.hash()
            && next.hash() == cursor.next_validators.hash(),
        "restored validator history differs from the native cursor"
    );
    // All change points and committed hashes are durable before publishing.
    // A crash before this write restarts the scan without exposing a partial index.
    control.check()?;
    app.store.publish_evidence_index_start(base)?;
    println!(
        "Restored Tendermint validator history from {base} through {}",
        cursor.height
    );
    Ok(())
}

#[derive(Clone, Default)]
pub(super) struct Pool {
    entries: BTreeMap<EvidenceKey, types::Evidence>,
    bytes: usize,
}

impl Pool {
    pub(super) fn load(store: &BlockStore) -> Result<Self> {
        let Some(bytes) = store.pending_evidence()? else {
            return Ok(Self::default());
        };
        ensure!(
            bytes.len() <= MAX_PENDING_BYTES,
            "oversized pending evidence store"
        );
        let mut pool = Self::default();
        for item in types::EvidenceList::decode(bytes.as_slice())?.evidence {
            pool.insert(item)?;
        }
        Ok(pool)
    }

    pub(super) fn encode(&self) -> Vec<u8> {
        types::EvidenceList {
            evidence: self.entries.values().cloned().collect(),
        }
        .encode_to_vec()
    }

    fn insert(&mut self, item: types::Evidence) -> Result<bool> {
        let key = (wire::height(&item)?, wire::hash(&item)?);
        let bytes = list_size(&item);
        if self.entries.contains_key(&key)
            || bytes > wire::MAX_MESSAGE
            || self.bytes + bytes > MAX_PENDING_BYTES
        {
            return Ok(false);
        }
        self.entries.insert(key, item);
        self.bytes += bytes;
        Ok(true)
    }

    pub(super) fn after_commit(
        &self,
        height: u64,
        time: Time,
        params: &Params,
        included: &[[u8; 32]],
        detected: Vec<types::Evidence>,
    ) -> Result<Option<Self>> {
        if self.entries.is_empty() && detected.is_empty() {
            return Ok(None);
        }
        let included = included.iter().collect::<BTreeSet<_>>();
        let mut next = Self::default();
        for (&(h, hash), item) in &self.entries {
            if !included.contains(&hash) && !expired(height, time, h, wire::time(item)?, params) {
                next.insert(item.clone())?;
            }
        }
        let mut changed = next.entries.len() != self.entries.len();
        for item in detected {
            if !included.contains(&wire::hash(&item)?) {
                changed |= next.insert(item)?;
            }
        }
        Ok(changed.then_some(next))
    }

    pub(super) fn proposed(&self, max_bytes: usize) -> types::EvidenceList {
        let mut bytes = 0;
        let evidence = self
            .entries
            .values()
            .take_while(|item| {
                bytes += list_size(item);
                bytes <= max_bytes
            })
            .cloned()
            .collect();
        types::EvidenceList { evidence }
    }

    pub(super) fn for_peer(
        &self,
        height: u64,
        max_age: u64,
        after: Option<EvidenceKey>,
    ) -> Option<PendingEvidence> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.entries
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .find(|((h, _), _)| *h < height && height - *h <= max_age)
            .map(|(&key, item)| (key, item.clone()))
    }
}

fn list_size(item: &types::Evidence) -> usize {
    let len = item.encoded_len();
    1 + prost::length_delimiter_len(len) + len
}

fn expired(head: u64, now: Time, height: u64, time: Time, params: &Params) -> bool {
    head.saturating_sub(height) > params.max_age_num_blocks
        && now
            .duration_since(time)
            .is_ok_and(|age| age > params.max_age_duration.0)
}

#[test]
fn evidence_age_requires_both_limits_to_be_strictly_exceeded() {
    use std::time::Duration;
    let time = Time::unix_epoch();
    let params = Params {
        max_age_num_blocks: 10,
        max_age_duration: tendermint::evidence::Duration(Duration::from_secs(60)),
        max_bytes: 1024,
    };
    let boundary = time.checked_add(Duration::from_secs(60)).unwrap();
    let late = boundary.checked_add(Duration::from_nanos(1)).unwrap();
    assert!(!expired(20, late, 10, time, &params));
    assert!(!expired(21, boundary, 10, time, &params));
    assert!(expired(21, late, 10, time, &params));
}

impl NativeSync {
    pub(super) fn validate_live_evidence(
        &self,
        cursor: &Cursor,
        block: &ReceivedBlock,
    ) -> Result<()> {
        let mut hashes = BTreeSet::new();
        for item in &block.evidence().evidence {
            let hash = wire::hash(item)?;
            ensure!(hashes.insert(hash), "repeated evidence in a block");
            ensure!(
                !self.app.store.evidence_committed(&hash)?,
                "evidence already committed"
            );
            self.verify_evidence(cursor, item)?;
        }
        Ok(())
    }

    pub(super) fn receive_evidence(&mut self, item: types::Evidence) -> Result<()> {
        let item = wire::canonicalize(&item)?;
        let hash = wire::hash(&item)?;
        if self.app.store.evidence_committed(&hash)? {
            println!(
                "{}",
                serde_json::json!({
                    "event": "evidence_already_committed",
                    "evidenceHash": hex::encode(hash),
                })
            );
            return Ok(());
        }
        let cursor = self.cursor()?;
        let key = (wire::height(&item)?, hash);
        if self.evidence.entries.contains_key(&key) || key.0 > cursor.height {
            return Ok(());
        }
        // A peer may retain evidence below our snapshot or legacy index.
        if self
            .app
            .store
            .evidence_index_start()?
            .is_none_or(|start| key.0 < start)
        {
            return Ok(());
        }
        self.verify_evidence(cursor, &item)?;
        let mut next = self.evidence.clone();
        if next.insert(item)? {
            self.app.store.store_pending_evidence(&next.encode())?;
            self.evidence = next;
            println!("Accepted Tendermint evidence {}", hex::encode(hash));
        }
        Ok(())
    }

    fn verify_evidence(&self, cursor: &Cursor, item: &types::Evidence) -> Result<()> {
        let height = wire::height(item)?;
        let time = wire::time(item)?;
        let previous = cursor
            .previous
            .as_ref()
            .context("no committed evidence history")?;
        ensure!(
            height > 0 && height <= cursor.height,
            "invalid evidence height"
        );
        ensure!(
            !expired(
                cursor.height,
                previous.time,
                height,
                time,
                &cursor.params.evidence
            ),
            "evidence is past both age limits"
        );
        ensure!(
            self.app
                .store
                .evidence_index_start()?
                .is_some_and(|start| start <= height),
            "evidence requires historical context below the verified index"
        );
        let (block_time, validators) = self.evidence_context(cursor, height)?;
        ensure!(
            time == block_time,
            "evidence timestamp differs from the infraction block"
        );
        match item.sum.as_ref().context("missing evidence proof")? {
            Sum::DuplicateVoteEvidence(proof) => {
                wire::verify_duplicate(proof, &self.chain_id, &validators)
            }
            Sum::LightClientAttackEvidence(proof) => {
                self.verify_light_attack(cursor, proof, &validators)
            }
        }
    }

    fn verify_light_attack(
        &self,
        cursor: &Cursor,
        proof: &types::LightClientAttackEvidence,
        validators: &Set,
    ) -> Result<()> {
        let height = proof
            .conflicting_block
            .as_ref()
            .and_then(|block| block.signed_header.as_ref())
            .and_then(|signed| signed.header.as_ref())
            .context("missing conflicting header")?
            .height;
        let height = u64::try_from(height)?.min(cursor.height);
        let (header, commit) = self.history_header(height)?;
        wire::verify_light_attack(proof, validators, &header, &commit)
    }

    fn evidence_context(&self, cursor: &Cursor, height: u64) -> Result<(Time, Set)> {
        if height == cursor.height {
            let previous = cursor.previous.as_ref().context("missing previous block")?;
            return Ok((previous.time, previous.validators.clone()));
        }
        let (header, _) = self.history_header(height)?;
        let validators = self.history_validators(height, &header)?;
        ensure!(
            header.height.value() == height
                && header.chain_id == self.chain_id
                && header.validators_hash == validators.hash(),
            "historical evidence context differs from its header"
        );
        Ok((header.time, validators))
    }
}
