//! Tendermint 0.34 State Sync discovery and chunk requests.

use crate::{Endpoint, Session, wire::Channel};
use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::Mutex,
    time::Duration,
};
use tendermint::chain;
use tendermint_proto::v0_34::statesync::{
    ChunkRequest, ChunkResponse, Message as Envelope, SnapshotsRequest, SnapshotsResponse,
    message::Sum,
};

const MAX_ADVERTISEMENTS: usize = 10;
pub const SNAPSHOT_CHUNK_BYTES: u64 = 10_000_000;
const SNAPSHOT_FORMAT: u32 = 1;
/// Minter's compact historical context, separate from the Go application snapshot.
pub const CONTEXT_FORMAT: u32 = 0x4d43_5458;
pub const MAX_SNAPSHOT_CHUNKS: u32 = 100_000;

#[derive(Message)]
pub struct SnapshotMetadata {
    #[prost(bytes, repeated, tag = "1")]
    pub chunk_hashes: Vec<Vec<u8>>,
}

/// A snapshot held open for its advertised lifetime. Chunk reads are serialized
/// around the shared cursor, and the advertised hashes are checked before sending.
pub struct SnapshotFile {
    advertisement: SnapshotsResponse,
    file: Mutex<File>,
    chunk_hashes: SnapshotMetadata,
    size: u64,
}

impl SnapshotFile {
    pub fn new(file: File, height: u64) -> Result<Self> {
        Self::new_with_progress(file, height, |_| Ok(()))
    }

    pub fn new_context(file: File, height: u64) -> Result<Self> {
        Self::new_context_with_progress(file, height, |_| Ok(()))
    }

    /// Report hashed bytes at chunk boundaries. An error stops construction.
    pub fn new_with_progress(
        file: File,
        height: u64,
        progress: impl FnMut(u64) -> Result<()>,
    ) -> Result<Self> {
        Self::with_format(file, height, SNAPSHOT_FORMAT, progress)
    }

    pub fn new_context_with_progress(
        file: File,
        height: u64,
        progress: impl FnMut(u64) -> Result<()>,
    ) -> Result<Self> {
        Self::with_format(file, height, CONTEXT_FORMAT, progress)
    }

    fn with_format(
        mut file: File,
        height: u64,
        format: u32,
        mut progress: impl FnMut(u64) -> Result<()>,
    ) -> Result<Self> {
        progress(0)?;
        ensure!(height > 0, "snapshot height must be positive");
        let metadata = file.metadata()?;
        let size = metadata.len();
        ensure!(
            metadata.is_file() && size > 0,
            "snapshot must be a nonempty file"
        );
        let chunks = u32::try_from(size.div_ceil(SNAPSHOT_CHUNK_BYTES))?;
        ensure!(
            chunks <= MAX_SNAPSHOT_CHUNKS,
            "snapshot metadata exceeds the State Sync message limit"
        );
        file.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0; (size.min(SNAPSHOT_CHUNK_BYTES)) as usize];
        let mut digest = Sha256::new();
        let mut chunk_hashes = SnapshotMetadata {
            chunk_hashes: Vec::with_capacity(chunks as usize),
        };
        for index in 0..chunks {
            let (_, length) = chunk_extent(size, index);
            let chunk = &mut buf[..length];
            file.read_exact(chunk).context("read snapshot chunk")?;
            digest.update(&*chunk);
            chunk_hashes
                .chunk_hashes
                .push(Sha256::digest(chunk).to_vec());
            progress((u64::from(index + 1) * SNAPSHOT_CHUNK_BYTES).min(size))?;
        }
        ensure!(
            file.read(&mut [0])? == 0,
            "snapshot size changed while reading"
        );
        let advertisement = SnapshotsResponse {
            height,
            format,
            chunks,
            hash: digest.finalize().to_vec(),
            metadata: chunk_hashes.encode_to_vec(),
        };
        Ok(Self {
            advertisement,
            file: Mutex::new(file),
            chunk_hashes,
            size,
        })
    }

    pub fn advertisement(&self) -> &SnapshotsResponse {
        &self.advertisement
    }

    pub fn bytes(&self) -> u64 {
        self.size
    }

    pub(crate) fn chunk(&self, request: &ChunkRequest) -> Result<Option<Vec<u8>>> {
        if request.height != self.advertisement.height
            || request.format != self.advertisement.format
            || request.index >= self.advertisement.chunks
        {
            return Ok(None);
        }
        let (offset, length) = chunk_extent(self.size, request.index);
        let mut bytes = vec![0; length];
        {
            let mut file = self
                .file
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot file lock"))?;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut bytes)?;
        }
        ensure!(
            self.chunk_hashes.chunk_hashes[request.index as usize].as_slice()
                == &Sha256::digest(&bytes)[..],
            "snapshot chunk changed after advertisement"
        );
        Ok(Some(bytes))
    }
}

fn chunk_extent(size: u64, index: u32) -> (u64, usize) {
    let offset = u64::from(index) * SNAPSHOT_CHUNK_BYTES;
    (offset, (size - offset).min(SNAPSHOT_CHUNK_BYTES) as usize)
}

/// The caller authenticates the expected snapshot and its chunks.
pub struct SnapshotPeer(Session);

impl SnapshotPeer {
    pub fn connect(endpoint: Endpoint, chain_id: &chain::Id, timeout: Duration) -> Result<Self> {
        let (_, session) = Session::connect(
            endpoint,
            chain_id,
            timeout,
            &[Channel::Snapshots, Channel::Chunks],
        )?;
        Ok(Self(session))
    }

    pub fn find(&mut self, height: u64, format: u32, hash: &[u8; 32]) -> Result<SnapshotsResponse> {
        self.0.reset_deadline()?;
        self.send(
            Channel::Snapshots,
            Sum::SnapshotsRequest(SnapshotsRequest {}),
        )?;
        for _ in 0..MAX_ADVERTISEMENTS {
            let Sum::SnapshotsResponse(snapshot) = self.response()? else {
                bail!("unexpected State Sync reply to snapshot request");
            };
            if snapshot.height == height && snapshot.format == format && snapshot.hash == hash {
                return Ok(snapshot);
            }
        }
        bail!("peer did not advertise the requested snapshot {height}")
    }

    pub fn chunk(&mut self, height: u64, format: u32, index: u32) -> Result<Vec<u8>> {
        self.0.reset_deadline()?;
        self.send(
            Channel::Chunks,
            Sum::ChunkRequest(ChunkRequest {
                height,
                format,
                index,
            }),
        )?;
        loop {
            match self.response()? {
                Sum::SnapshotsResponse(_) => continue,
                Sum::ChunkResponse(response) => {
                    ensure!(
                        response.height == height
                            && response.format == format
                            && response.index == index,
                        "unexpected State Sync chunk identity"
                    );
                    ensure!(
                        !response.missing && !response.chunk.is_empty(),
                        "peer is missing snapshot chunk {index}"
                    );
                    return Ok(response.chunk);
                }
                _ => bail!("unexpected State Sync reply to chunk request"),
            }
        }
    }

    fn send(&mut self, channel: Channel, sum: Sum) -> Result<()> {
        self.0.send(channel, &Envelope { sum: Some(sum) })
    }

    fn response(&mut self) -> Result<Sum> {
        loop {
            let (channel, bytes) = self
                .0
                .receive()
                .context("receive Tendermint State Sync message")?;
            let message = Envelope::decode(bytes.as_slice())?
                .sum
                .context("empty State Sync message")?;
            match (channel, message) {
                (Channel::Snapshots, Sum::SnapshotsRequest(_)) => {}
                (Channel::Chunks, Sum::ChunkRequest(request)) => {
                    self.send(
                        Channel::Chunks,
                        Sum::ChunkResponse(ChunkResponse {
                            height: request.height,
                            format: request.format,
                            index: request.index,
                            missing: true,
                            chunk: Vec::new(),
                        }),
                    )?;
                }
                (Channel::Snapshots, reply @ Sum::SnapshotsResponse(_))
                | (Channel::Chunks, reply @ Sum::ChunkResponse(_)) => return Ok(reply),
                _ => bail!("unexpected message on State Sync channel {channel:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn snapshot_hashing_can_stop_between_chunks_and_preserves_the_format() -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&vec![7; SNAPSHOT_CHUNK_BYTES as usize + 23])?;
        let before = std::fs::read(file.path())?;
        let mut calls = Vec::new();
        let error = SnapshotFile::new_with_progress(file.reopen()?, 10, |done| {
            calls.push(done);
            ensure!(done < SNAPSHOT_CHUNK_BYTES, "requested test stop");
            Ok(())
        })
        .err()
        .context("hashing should stop")?;
        assert_eq!(error.to_string(), "requested test stop");
        assert_eq!(calls, [0, SNAPSHOT_CHUNK_BYTES]);
        assert_eq!(std::fs::read(file.path())?, before);
        let original = SnapshotFile::new(file.reopen()?, 10)?;
        let mut calls = Vec::new();
        let controlled = SnapshotFile::new_with_progress(file.reopen()?, 10, |done| {
            calls.push(done);
            Ok(())
        })?;
        assert_eq!(calls, [0, SNAPSHOT_CHUNK_BYTES, before.len() as u64]);
        assert_eq!(original.advertisement(), controlled.advertisement());
        assert_eq!(original.bytes(), controlled.bytes());
        let context = SnapshotFile::new_context_with_progress(file.reopen()?, 10, |_| Ok(()))?;
        assert_eq!(context.advertisement().format, CONTEXT_FORMAT);
        assert_eq!(context.advertisement().hash, original.advertisement().hash);
        Ok(())
    }
}
