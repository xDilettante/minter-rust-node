use super::*;
use std::path::PathBuf;

struct CancelReader {
    file: File,
    control: OperationControl,
    remaining: Option<usize>,
}

impl Read for CancelReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let limit = self.remaining.unwrap_or(output.len()).min(output.len());
        let count = self.file.read(&mut output[..limit])?;
        if let Some(remaining) = &mut self.remaining {
            *remaining -= count;
            if *remaining == 0 {
                self.control.cancel();
            }
        } else if count == 0 && !output.is_empty() {
            // Request stop while returning the final, exact EOF. All bytes and
            // signatures are now verified; the ready marker may be published.
            self.control.cancel();
        }
        Ok(count)
    }
}

fn file_hash(path: &Path) -> Result<[u8; 32]> {
    let mut input = File::open(path)?;
    let mut hash = super::super::super::Hasher::new();
    std::io::copy(&mut input, &mut hash)?;
    Ok(hash.finalize().into())
}

#[test]
#[ignore = "requires a trusted real mainnet history bundle and an empty output directory"]
fn cancelled_required_history_keeps_cache_and_can_resume() -> Result<()> {
    let package = PathBuf::from(std::env::var("MINTER_TEST_HISTORY_PACKAGE")?);
    let output = PathBuf::from(std::env::var("MINTER_TEST_HISTORY_OUTPUT")?);
    let trusted: [u8; 32] = hex::decode(std::env::var("MINTER_TEST_HISTORY_SHA256")?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid test trusted digest"))?;
    ensure!(
        file_hash(&package.join("manifest.json"))? == trusted,
        "test manifest differs"
    );
    let manifest: Manifest = serde_json::from_slice(&fs::read(package.join("manifest.json"))?)?;
    let context = manifest
        .history_context
        .as_ref()
        .context("test needs required history")?;
    let original_hash = file_hash(&package.join("history.pb"))?;
    ensure!(original_hash == context.sha256, "test history differs");
    fs::create_dir(&output)?;
    let metadata = output.join("metadata");
    fs::create_dir(&metadata)?;
    for name in ["manifest.json", "state.pb", "blocks.pb", "snapshot.zlib"] {
        fs::copy(package.join(name), metadata.join(name))?;
    }
    let database = output.join("state");
    let error = NativeSync::open_checkpoint(&database, 1, &metadata, trusted, None)
        .err()
        .context("metadata-only package should require history")?;
    ensure!(
        format!("{error:#}").contains("snapshot requires history.pb"),
        "{error:#}"
    );
    let cache = database.join(super::super::downloads::cache_name(&context.sha256));
    fs::copy(package.join("history.pb"), &cache)?;
    let mut sync = NativeSync::open(&database, 1, None, None)?;
    assert!(!sync.history_context_ready()?);
    let head = sync.head()?;
    let control = OperationControl::default();
    let mut input = control.reader(CancelReader {
        file: File::open(&cache)?,
        control: control.clone(),
        remaining: Some(64 * 1024),
    });
    let error = sync
        .import_history_context(&mut input)
        .err()
        .context("import should cancel")?;
    assert!(OperationCancelled::caused(&error));
    assert!(!sync.history_context_ready()?);
    assert!(
        sync.history_context_floor()?
            .is_some_and(|floor| floor < head.height)
    );
    drop(input);
    let error = complete(sync, &metadata, Some(context), None, &control)
        .err()
        .context("cancelled completion should fail")?;
    assert!(OperationCancelled::caused(&error));
    assert_eq!(file_hash(&cache)?, original_hash);
    let cancelled = OperationControl::default();
    cancelled.cancel();
    let error = verified_file(File::open(&cache)?, context, &cancelled)
        .err()
        .context("verification should preserve cancellation")?;
    assert!(OperationCancelled::caused(&error));
    assert_eq!(file_hash(&cache)?, original_hash);

    // Clone only this test's closed RocksDB to exercise the last EOF boundary
    // separately. This fixture has no P2P or validator keys.
    let late_database = output.join("late-eof-state");
    fs::create_dir(&late_database)?;
    for entry in fs::read_dir(&database)? {
        let entry = entry?;
        ensure!(entry.file_type()?.is_file(), "unexpected database entry");
        fs::copy(entry.path(), late_database.join(entry.file_name()))?;
    }
    let sync = NativeSync::open(&database, 1, None, None)?;
    assert!(!sync.history_context_ready()?);
    let sync = complete(
        sync,
        &metadata,
        Some(context),
        None,
        &OperationControl::default(),
    )?;
    assert!(sync.history_context_ready()?);
    assert_eq!(sync.head()?.height, head.height);
    assert_eq!(sync.head()?.app_hash, head.app_hash);
    assert!(!cache.exists());
    drop(sync);

    let late_cache = late_database.join(super::super::downloads::cache_name(&context.sha256));
    let mut sync = NativeSync::open(&late_database, 1, None, None)?;
    let control = OperationControl::default();
    let mut input = control.reader(CancelReader {
        file: File::open(&late_cache)?,
        control: control.clone(),
        remaining: None,
    });
    sync.import_history_context(&mut input)?;
    assert!(control.is_cancelled());
    assert!(sync.history_context_ready()?);
    assert_eq!(sync.head()?.app_hash, head.app_hash);
    drop(input);
    let sync = complete(
        sync,
        &metadata,
        Some(context),
        None,
        &OperationControl::default(),
    )?;
    assert!(sync.history_context_ready()?);
    assert!(!late_cache.exists());
    assert_eq!(file_hash(&package.join("history.pb"))?, original_hash);
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "height": head.height,
            "appHash": hex::encode(head.app_hash),
            "manifestSha256": hex::encode(trusted),
            "historySha256": hex::encode(original_hash),
            "cancelledImport": true,
            "cachePreserved": true,
            "incompleteUntilVerified": true,
            "resumedFromCache": true,
            "cacheRemovedAfterReady": true,
            "stopAtExactEofPublishesReady": true
        }))?,
    )?;
    Ok(())
}
