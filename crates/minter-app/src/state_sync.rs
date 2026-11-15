use anyhow::{Context, Result, ensure};
use minter_p2p::snapshot::{SNAPSHOT_CHUNK_BYTES as CHUNK_BYTES, SnapshotMetadata};
use minter_state::Checkpoint;
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use tendermint_proto::v0_34::abci::{
    RequestApplySnapshotChunk, RequestOfferSnapshot, ResponseApplySnapshotChunk,
    response_apply_snapshot_chunk::Result as Apply, response_offer_snapshot::Result as Offer,
};

pub(crate) struct Receiver {
    directory: PathBuf,
    trusted: Checkpoint,
    chunk_count: u32,
    transfer: Option<Transfer>,
}

struct Transfer {
    file: File,
    chunk_hashes: Vec<Vec<u8>>,
    digest: Sha256,
    next: u32,
}

pub(crate) enum Chunk {
    Reply(ResponseApplySnapshotChunk),
    Ready(File),
}

impl Receiver {
    pub fn new(directory: &Path, trusted: Checkpoint) -> Result<Self> {
        ensure!(trusted.bytes > 0, "empty State Sync snapshot");
        let chunk_count = u32::try_from(trusted.bytes.div_ceil(CHUNK_BYTES))
            .context("too many State Sync chunks")?;
        Ok(Self {
            directory: directory.to_owned(),
            trusted,
            chunk_count,
            transfer: None,
        })
    }

    pub fn checkpoint(&self) -> &Checkpoint {
        &self.trusted
    }

    pub fn offer(&mut self, request: RequestOfferSnapshot) -> Offer {
        let Some(snapshot) = request.snapshot else {
            return Offer::Reject;
        };
        if snapshot.format != self.trusted.format {
            return Offer::RejectFormat;
        }
        // Format 1 metadata contains one 32-byte checksum per chunk with a tag
        // and one-byte length for each entry.
        if snapshot.height != self.trusted.height
            || snapshot.chunks != self.chunk_count
            || snapshot.hash.as_ref() != self.trusted.snapshot_sha256
            || request.app_hash.as_ref() != self.trusted.app_hash
            || Some(snapshot.metadata.len()) != (self.chunk_count as usize).checked_mul(34)
        {
            return Offer::Reject;
        }
        let Ok(metadata) = SnapshotMetadata::decode(snapshot.metadata) else {
            return Offer::Reject;
        };
        if metadata.chunk_hashes.len() != self.chunk_count as usize
            || metadata.chunk_hashes.iter().any(|hash| hash.len() != 32)
        {
            return Offer::Reject;
        }
        match tempfile::tempfile_in(&self.directory) {
            Ok(file) => {
                self.transfer = Some(Transfer {
                    file,
                    chunk_hashes: metadata.chunk_hashes,
                    digest: Sha256::new(),
                    next: 0,
                });
                Offer::Accept
            }
            Err(error) => {
                eprintln!("State Sync could not create a staging file: {error}");
                Offer::Abort
            }
        }
    }

    pub fn apply(&mut self, request: RequestApplySnapshotChunk) -> Chunk {
        let Some(transfer) = self.transfer.as_mut() else {
            return Chunk::Reply(chunk_response(Apply::Abort));
        };
        let index = request.index;
        if index > transfer.next || index >= self.chunk_count {
            self.transfer = None;
            return Chunk::Reply(chunk_response(Apply::RejectSnapshot));
        }
        let bytes = (self.trusted.bytes - u64::from(index) * CHUNK_BYTES).min(CHUNK_BYTES);
        if request.chunk.len() as u64 != bytes
            || transfer.chunk_hashes[index as usize].as_slice()
                != &Sha256::digest(&request.chunk)[..]
        {
            return Chunk::Reply(ResponseApplySnapshotChunk {
                result: Apply::Retry.into(),
                refetch_chunks: vec![index],
                reject_senders: (!request.sender.is_empty())
                    .then_some(request.sender)
                    .into_iter()
                    .collect(),
            });
        }
        if index == transfer.next {
            if let Err(error) = transfer.file.write_all(&request.chunk) {
                self.transfer = None;
                eprintln!("State Sync could not write a staging chunk: {error}");
                return Chunk::Reply(chunk_response(Apply::Abort));
            }
            transfer.digest.update(&request.chunk);
            transfer.next += 1;
        }
        if transfer.next < self.chunk_count {
            return Chunk::Reply(chunk_response(Apply::Accept));
        }
        let transfer = self.transfer.take().expect("completed State Sync transfer");
        let digest: [u8; 32] = transfer.digest.finalize().into();
        if digest != self.trusted.snapshot_sha256 {
            return Chunk::Reply(chunk_response(Apply::RejectSnapshot));
        }
        Chunk::Ready(transfer.file)
    }
}

pub(crate) fn chunk_response(result: Apply) -> ResponseApplySnapshotChunk {
    ResponseApplySnapshotChunk {
        result: result.into(),
        ..Default::default()
    }
}
