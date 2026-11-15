use super::*;
use minter_p2p::snapshot::{CONTEXT_FORMAT, SNAPSHOT_CHUNK_BYTES, SnapshotFile};
use sha2::Digest;
use std::io::{Seek, SeekFrom, Write};

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServedManifest {
    #[serde(flatten)]
    snapshot: Checkpoint,
    #[serde(default)]
    history_context: Option<HistoryContext>,
}

impl NativeSync {
    pub fn served_history_context(&self, package: &Path) -> Result<Option<SnapshotFile>> {
        let raw = read_limited(&package.join("manifest.json"), MAX_MANIFEST_BYTES)?;
        let manifest: ServedManifest = serde_json::from_slice(&raw)?;
        self.verify_snapshot_anchor(&manifest.snapshot)?;
        let Some(context) = manifest.history_context else {
            return Ok(None);
        };
        let initial = self
            .app
            .chain
            .as_ref()
            .context("missing chain")?
            .initial_height;
        context.validate(manifest.snapshot.height, initial)?;
        let file = SnapshotFile::new_context(
            File::open(package.join("history.pb"))?,
            manifest.snapshot.height,
        )?;
        ensure!(
            file.bytes() == context.bytes && file.advertisement().hash == context.sha256,
            "served history context differs from its manifest"
        );
        Ok(Some(file))
    }
}

pub(super) fn complete(
    mut sync: NativeSync,
    package: &Path,
    context: Option<&HistoryContext>,
    peers: Option<(&[Endpoint], Duration)>,
    control: &OperationControl,
) -> Result<NativeSync> {
    control.check()?;
    let directory = sync.app.store.directory().to_path_buf();
    let database = directory.as_path();
    sync.cleanup_history_downloads();
    let Some(context) = context else {
        return Ok(sync);
    };
    if sync.history_context_ready()? {
        return Ok(sync);
    }
    let height = sync
        .app
        .store
        .checkpoint()
        .context("missing snapshot")?
        .height;
    let local = package.join("history.pb");
    if local.is_file() {
        if let Some(file) = verified_file(File::open(&local)?, context, control)? {
            sync.import_history_context(&mut BufReader::new(control.reader(file)))?;
            sync.ensure_history_context()?;
            sync.cleanup_history_downloads();
            return Ok(sync);
        }
        eprintln!(
            "Local snapshot history failed its size or SHA-256 check; trying State Sync peers"
        );
    }
    let cache = database.join(super::downloads::cache_name(&context.sha256));
    control.check()?;
    let cached = match File::open(&cache) {
        Ok(file) => verified_file(file, context, control)?,
        Err(_) => None,
    };
    let mut file = if let Some(file) = cached {
        file
    } else {
        control.check()?;
        if cache.exists() {
            fs::remove_file(&cache).context("remove invalid history download")?;
        }
        let (peers, timeout) =
            peers.context("snapshot requires history.pb or --snapshot-peer for its history")?;
        let mut completed = None;
        for endpoint in peers {
            control.check()?;
            match download(
                database,
                context,
                height,
                sync.chain_id(),
                *endpoint,
                timeout,
                control,
            ) {
                Ok(file) => {
                    completed = Some(file);
                    break;
                }
                Err(error) if OperationCancelled::caused(&error) || control.is_cancelled() => {
                    return Err(error);
                }
                Err(error) => eprintln!(
                    "History peer {}@{}: {error:#}",
                    endpoint.id, endpoint.address
                ),
            }
        }
        let temporary =
            completed.context("no State Sync peer served the required snapshot history")?;
        // Once all chunks and their digest are verified, publish the cache even
        // if stop arrived on the last chunk. Import remains cancellable below.
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(&cache)
            .context("publish downloaded snapshot history")?
    };
    sync.import_history_context(&mut BufReader::new(control.reader(&mut file)))?;
    sync.ensure_history_context()?;
    drop(file);
    sync.cleanup_history_downloads();
    Ok(sync)
}

fn verified_file(
    mut file: File,
    context: &HistoryContext,
    control: &OperationControl,
) -> Result<Option<File>> {
    match verify_file(&mut file, context, control) {
        Ok(()) => Ok(Some(file)),
        Err(error) if OperationCancelled::caused(&error) || control.is_cancelled() => Err(error),
        Err(_) => Ok(None),
    }
}

fn verify_file(
    file: &mut File,
    context: &HistoryContext,
    control: &OperationControl,
) -> Result<()> {
    control.check()?;
    ensure!(
        file.metadata()?.len() == context.bytes,
        "history context size differs"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut digest = super::super::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    let mut input = control.reader(&mut *file);
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    ensure!(
        digest.finalize().as_slice() == context.sha256,
        "history context SHA-256 differs"
    );
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn download(
    database: &Path,
    context: &HistoryContext,
    height: u64,
    chain: &chain::Id,
    endpoint: Endpoint,
    timeout: Duration,
    control: &OperationControl,
) -> Result<tempfile::NamedTempFile> {
    control.check()?;
    let mut peer = SnapshotPeer::connect(endpoint, chain, timeout)?;
    control.check()?;
    let advertised = peer.find(height, CONTEXT_FORMAT, &context.sha256)?;
    let chunks: u32 = context.bytes.div_ceil(SNAPSHOT_CHUNK_BYTES).try_into()?;
    ensure!(
        advertised.chunks == chunks,
        "history context chunk count differs"
    );
    let mut file = tempfile::Builder::new()
        .prefix(super::downloads::TEMP_PREFIX)
        .tempfile_in(database)?;
    let mut digest = super::super::Hasher::new();
    for index in 0..chunks {
        control.check()?;
        let chunk = peer.chunk(height, CONTEXT_FORMAT, index)?;
        let expected =
            (context.bytes - u64::from(index) * SNAPSHOT_CHUNK_BYTES).min(SNAPSHOT_CHUNK_BYTES);
        ensure!(
            chunk.len() as u64 == expected,
            "history context chunk size differs"
        );
        digest.update(&chunk);
        file.write_all(&chunk)?;
    }
    ensure!(
        digest.finalize().as_slice() == context.sha256,
        "downloaded history context SHA-256 differs"
    );
    file.seek(SeekFrom::Start(0))?;
    eprintln!(
        "History State Sync downloaded {} bytes in {chunks} chunks from {}",
        context.bytes, endpoint.id
    );
    Ok(file)
}
