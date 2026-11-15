use anyhow::{Context, Result};
use minter_state::{GenesisStage, OperationCancelled, OperationControl, OperationReader};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
};

pub(crate) const CHUNK_BYTES: usize = 64 * 1024;

struct Reader<R> {
    input: OperationReader<R>,
    control: OperationControl,
    phase: GenesisStage,
    done: u64,
}

impl<R> Reader<R> {
    fn new(input: R, phase: GenesisStage, control: &OperationControl) -> Result<Self> {
        control.genesis_progress(phase, 0)?;
        Ok(Self {
            input: control.reader(input),
            control: control.clone(),
            phase,
            done: 0,
        })
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let limit = output.len().min(CHUNK_BYTES);
        let count = self.input.read(&mut output[..limit])?;
        if count > 0 {
            self.done += count as u64;
            self.control
                .genesis_progress(self.phase, self.done)
                .map_err(|error| {
                    if OperationCancelled::caused(&error) {
                        io::Error::other(OperationCancelled)
                    } else {
                        io::Error::other(error)
                    }
                })?;
        }
        Ok(count)
    }
}

pub(crate) fn read_file(path: &Path, control: &OperationControl) -> Result<Vec<u8>> {
    control.check()?;
    let file = File::open(path).with_context(|| format!("read genesis {}", path.display()))?;
    let mut bytes = Vec::new();
    Reader::new(file, GenesisStage::FileBytes, control)?.read_to_end(&mut bytes)?;
    control.check()?;
    Ok(bytes)
}

pub(crate) fn decode<T: DeserializeOwned>(
    bytes: &[u8],
    phase: GenesisStage,
    control: &OperationControl,
) -> Result<T> {
    let input = Reader::new(bytes, phase, control)?;
    let result =
        serde_json::from_reader(BufReader::with_capacity(CHUNK_BYTES, input)).map_err(|error| {
            // serde_json drops an io::Error's typed payload. This reader is
            // backed by a byte slice: its only possible I/O error is cancellation.
            if error.is_io() {
                anyhow::Error::new(OperationCancelled)
            } else {
                error.into()
            }
        })?;
    control.check()?;
    Ok(result)
}

pub(crate) fn hash_bytes(
    bytes: &[u8],
    phase: GenesisStage,
    control: &OperationControl,
) -> Result<[u8; 32]> {
    control.genesis_progress(phase, 0)?;
    let mut hash = Sha256::new();
    let mut done = 0;
    for chunk in bytes.chunks(CHUNK_BYTES) {
        control.check()?;
        hash.update(chunk);
        done += chunk.len() as u64;
        control.genesis_progress(phase, done)?;
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use minter_state::OperationCancelled;

    #[test]
    fn chunked_genesis_io_preserves_typed_cancellation_and_json_bytes() -> Result<()> {
        let value = serde_json::json!({"note": "<>&", "values": vec![7; 100_000]});
        let bytes = serde_json::to_vec_pretty(&value)?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("genesis.json");
        std::fs::write(&path, &bytes)?;
        let default = OperationControl::default();
        assert_eq!(read_file(&path, &default)?, bytes);
        assert_eq!(
            decode::<serde_json::Value>(&bytes, GenesisStage::AppStateBytes, &default)?,
            value
        );
        assert_eq!(
            hash_bytes(&bytes, GenesisStage::FileHashBytes, &default)?,
            <[u8; 32]>::from(Sha256::digest(&bytes))
        );
        for phase in [
            GenesisStage::FileBytes,
            GenesisStage::AppStateBytes,
            GenesisStage::FileHashBytes,
        ] {
            let control = OperationControl::default();
            let stop = control.clone();
            let control = control.with_genesis_progress(move |current, done| {
                if current == phase && done >= CHUNK_BYTES as u64 {
                    stop.cancel();
                }
            });
            let error = match phase {
                GenesisStage::FileBytes => read_file(&path, &control).unwrap_err(),
                GenesisStage::AppStateBytes => {
                    decode::<serde_json::Value>(&bytes, phase, &control).unwrap_err()
                }
                _ => hash_bytes(&bytes, phase, &control).unwrap_err(),
            };
            assert!(OperationCancelled::caused(&error), "{phase}: {error:#}");
            assert_eq!(std::fs::read(&path)?, bytes);
        }
        Ok(())
    }
}
