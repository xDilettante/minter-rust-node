use super::*;
use crate::native_sync::{BlockRecord, Hasher};
use minter_p2p::verify;
use serde::Serialize;
use sha2::Digest;
use std::io::{Read, Write};
use tendermint::{Hash, block::Header, chain};

mod bounded;
mod retention;
mod window;
pub use retention::NativeRetentionReport;

const MAGIC: &[u8] = b"MINTER-CONTEXT\x01";
const MAX_HEADER: usize = 16 * 1024;
const MAX_SET: usize = 8 * 1024 * 1024;
const MAX_RECORD: usize = 108 * 1024 * 1024;

#[derive(Clone, PartialEq, Message)]
struct Package {
    #[prost(string, tag = "1")]
    chain: String,
    #[prost(uint64, tag = "2")]
    anchor: u64,
    #[prost(uint64, tag = "3")]
    from: u64,
    #[prost(message, optional, tag = "4")]
    anchor_id: Option<types::BlockId>,
}

// Wrapping submessages as bytes avoids allocating repeated objects before preflight.
#[derive(Clone, PartialEq, Message)]
struct Record {
    #[prost(bytes, tag = "1")]
    signed_header: Vec<u8>,
    #[prost(bytes, tag = "2")]
    validators: Vec<u8>,
    #[prost(bytes, tag = "3")]
    evidence: Vec<u8>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryContextReport {
    pub anchor: u64,
    pub from: u64,
    pub records: u64,
    pub appended: u64,
}

impl NativeSync {
    pub fn history_context_floor(&self) -> Result<Option<u64>> {
        self.app.store.evidence_index_start()
    }

    /// Export authenticated context in reverse order, without old transaction bodies.
    pub fn export_history_context(
        &self,
        anchor: u64,
        from: u64,
        output: &mut impl Write,
    ) -> Result<HistoryContextReport> {
        let control = self.app.store.maintenance_control();
        control.export_progress(minter_state::ExportStage::HistoryRecords, 0)?;
        ensure!(
            from > 0 && from < anchor && anchor <= self.head()?.height,
            "invalid context range"
        );
        let (_, commit) = self.history_header(anchor)?;
        let package = Package {
            chain: self.chain_id.to_string(),
            anchor,
            from,
            anchor_id: Some(commit.block_id.into()),
        };
        output.write_all(MAGIC)?;
        write_message(output, &package, MAX_HEADER)?;
        let mut successor = self.history_header(anchor)?.0;
        for height in (from..anchor).rev() {
            control.check()?;
            let record = self.history_record(height)?;
            let (header, _, _) = validate(&record, &successor, &self.chain_id)?;
            write_message(output, &record, MAX_RECORD)?;
            successor = header;
            control.export_progress(minter_state::ExportStage::HistoryRecords, anchor - height)?;
        }
        Ok(HistoryContextReport {
            anchor,
            from,
            records: anchor - from,
            appended: 0,
        })
    }

    fn history_record(&self, height: u64) -> Result<Record> {
        let (header, commit) = self.history_header(height)?;
        let set = self.history_validators(height, &header)?;
        let mut record = if let Some(raw) = self.app.store.native_block_at(height)? {
            Record {
                signed_header: types::SignedHeader {
                    header: Some(header.into()),
                    commit: Some(commit.into()),
                }
                .encode_to_vec(),
                validators: Vec::new(),
                evidence: ReceivedBlock::decode(
                    BlockRecord::decode(raw.as_slice())?.block_response,
                )?
                .evidence()
                .encode_to_vec(),
            }
        } else {
            // Preserve the signed included list for re-export and EvidenceHash binding.
            Record::decode(
                self.app
                    .store
                    .native_context_at(height)?
                    .context("missing historical context")?
                    .as_slice(),
            )?
        };
        record.validators = membership(&set).encode_to_vec();
        Ok(record)
    }

    /// Every published prefix is authenticated and durable, even if the file is interrupted.
    pub fn import_history_context(
        &mut self,
        input: &mut impl Read,
    ) -> Result<HistoryContextReport> {
        let mut input = HashedInput {
            input,
            hash: Hasher::new(),
            bytes: 0,
        };
        let report = self.import_history_stream(&mut input)?;
        self.finish_required_context(&report, input.bytes, input.hash.finalize().into())?;
        Ok(report)
    }

    fn import_history_stream(&mut self, input: &mut impl Read) -> Result<HistoryContextReport> {
        let mut magic = vec![0; MAGIC.len()];
        input.read_exact(&mut magic)?;
        ensure!(magic == MAGIC, "unknown history context format");
        let package = Package::decode(read_message(input, MAX_HEADER)?.as_slice())?;
        let base = self
            .app
            .store
            .native_block_base()?
            .context("no native archive anchor")?;
        ensure!(
            package.chain == self.chain_id.as_str()
                && package.anchor == base
                && package.from > 0
                && package.from < base,
            "context package differs from the snapshot anchor"
        );
        if let Some(required) = &self.cursor()?.history_context
            && !self.history_context_ready()?
        {
            ensure!(
                package.from == required.from,
                "history context range differs from the required snapshot window"
            );
        }
        let (mut successor, commit) = self.history_header(base)?;
        ensure!(
            package.anchor_id == Some(commit.block_id.into()),
            "context package anchor BlockID differs"
        );
        let mut appended = 0u64;
        for height in (package.from..package.anchor).rev() {
            let mut record = Record::decode(read_message(input, MAX_RECORD)?.as_slice())?;
            let (header, set, included) = validate(&record, &successor, &self.chain_id)?;
            ensure!(
                header.height.value() == height,
                "noncontiguous context package"
            );
            let floor = self
                .app
                .store
                .evidence_index_start()?
                .context("missing verified context floor")?;
            if height < floor {
                // Memberships are shared by authenticated hash. Keep included proofs
                // for re-export and EvidenceHash binding, but no transaction bodies.
                record.validators.clear();
                let raw = record.encode_to_vec();
                let hash: [u8; 32] = header.validators_hash.as_bytes().try_into()?;
                self.app.store.extend_native_context(
                    height,
                    &raw,
                    &hash,
                    &membership(&set).encode_to_vec(),
                    &included,
                )?;
                appended += 1;
                if appended.is_multiple_of(1000) {
                    eprintln!("Imported {appended} new history records; verified height {height}");
                }
            } else {
                ensure!(
                    self.history_header(height)?.0.hash() == header.hash(),
                    "context prefix changed"
                );
            }
            successor = header;
        }
        ensure!(input.read(&mut [0])? == 0, "trailing context package data");
        Ok(HistoryContextReport {
            anchor: package.anchor,
            from: package.from,
            records: package.anchor - package.from,
            appended,
        })
    }

    pub(in crate::native_sync) fn history_header(&self, height: u64) -> Result<(Header, Commit)> {
        if let Some(raw) = self.app.store.native_block_at(height)? {
            let record = BlockRecord::decode(raw.as_slice())?;
            let block = ReceivedBlock::decode(record.block_response)?;
            // A seen commit may contain late signatures absent from the next block.
            // Historical classification needs exactly the commit selected by the chain.
            let next = height
                .checked_add(1)
                .context("historical height overflow")?;
            let commit = if let Some(raw) = self.app.store.native_block_at(next)? {
                ReceivedBlock::decode(BlockRecord::decode(raw.as_slice())?.block_response)?
                    .block()
                    .last_commit
                    .clone()
                    .context("missing successor LastCommit")?
            } else {
                record.commit.context("missing stored commit")?.try_into()?
            };
            return Ok((block.block().header.clone(), commit));
        }
        let raw = self
            .app
            .store
            .native_context_at(height)?
            .context("missing historical signed header")?;
        let record = Record::decode(raw.as_slice())?;
        decode_signed(&record.signed_header)
    }

    pub(super) fn history_validators(&self, height: u64, header: &Header) -> Result<Set> {
        let raw = if self.app.store.native_block_at(height)?.is_some() {
            self.app.store.native_validators_at(height)?
        } else {
            self.app
                .store
                .native_context_validators(&header.validators_hash.as_bytes().try_into()?)?
        }
        .context("missing historical validator membership")?;
        let set = proposer::validate_wire(types::ValidatorSet::decode(raw.as_slice())?)?;
        ensure!(
            set.hash() == header.validators_hash,
            "historical validator hash differs"
        );
        Ok(set)
    }
}

struct HashedInput<'a, R> {
    input: &'a mut R,
    hash: Hasher,
    bytes: u64,
}

impl<R: Read> Read for HashedInput<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| std::io::Error::other("history context size overflow"))?;
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
}

fn membership(set: &Set) -> types::ValidatorSet {
    let mut wire = proposer::to_wire(set);
    wire.proposer = None;
    for validator in &mut wire.validators {
        validator.proposer_priority = 0;
    }
    wire
}

fn decode_signed(raw: &[u8]) -> Result<(Header, Commit)> {
    ensure!(raw.len() <= MAX_SET, "oversized historical signed header");
    bounded::signed_header(raw)?;
    let signed = types::SignedHeader::decode(raw)?;
    Ok((
        signed
            .header
            .context("missing historical header")?
            .try_into()?,
        signed
            .commit
            .context("missing historical commit")?
            .try_into()?,
    ))
}

fn validate(
    record: &Record,
    successor: &Header,
    chain: &chain::Id,
) -> Result<(Header, Set, Vec<[u8; 32]>)> {
    let (header, commit) = decode_signed(&record.signed_header)?;
    ensure!(
        record.validators.len() <= MAX_SET,
        "oversized historical validator set"
    );
    bounded::validators(&record.validators)?;
    let set = proposer::validate_wire(types::ValidatorSet::decode(record.validators.as_slice())?)?;
    let id = successor.last_block_id.context("missing historical link")?;
    ensure!(
        header.chain_id == *chain
            && successor.chain_id == *chain
            && header.height.value().checked_add(1) == Some(successor.height.value())
            && header.hash() == id.hash
            && commit.block_id == id
            && header.validators_hash == set.hash()
            && header.next_validators_hash == successor.validators_hash
            && header.time <= successor.time,
        "historical context differs from its trusted successor"
    );
    ensure!(
        Some(verify::commit_hash(Some(&commit))) == successor.last_commit_hash,
        "historical LastCommitHash differs from its trusted successor"
    );
    verify::verify_commit(chain, header.height, id, &set, &commit)?;
    let mut payloads = Vec::new();
    let mut included = BTreeSet::new();
    bounded::messages(&record.evidence, 1, MAX_RECORD / 2, |raw| {
        ensure!(raw.len() <= 3 * MAX_SET, "oversized historical proof");
        bounded::proof(raw)?;
        let item = types::Evidence::decode(raw)?;
        ensure!(
            included.insert(wire::hash(&item)?),
            "repeated historical evidence"
        );
        payloads.push(wire::payload(&item)?);
        Ok(())
    })?;
    let evidence_hash = Hash::Sha256(tendermint::merkle::simple_hash_from_byte_vectors::<Hasher>(
        &payloads,
    ));
    ensure!(
        header.evidence_hash == Some(evidence_hash),
        "historical EvidenceHash differs"
    );
    Ok((header, set, included.into_iter().collect()))
}

fn read_message(input: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut size = 0u64;
    for shift in (0..=63).step_by(7) {
        let mut byte = [0];
        input
            .read_exact(&mut byte)
            .context("truncated context package")?;
        ensure!(shift < 63 || byte[0] <= 1, "context length overflow");
        size |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] < 128 {
            let size: usize = size.try_into()?;
            ensure!(size <= limit, "oversized context record");
            let mut bytes = vec![0; size];
            input
                .read_exact(&mut bytes)
                .context("truncated context record")?;
            return Ok(bytes);
        }
    }
    anyhow::bail!("context length overflow")
}

fn write_message(output: &mut impl Write, value: &impl Message, limit: usize) -> Result<()> {
    ensure!(value.encoded_len() <= limit, "oversized context record");
    output.write_all(&value.encode_length_delimited_to_vec())?;
    Ok(())
}

#[cfg(test)]
mod tests;
