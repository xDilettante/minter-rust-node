use anyhow::{Context, Result};
use minter_state::{
    BlockStore, Checkpoint, Mutation, NativeCheckpointWrite, OperationCancelled, OperationControl,
    StateStore,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

fn large_snapshot(root: &Path) -> Result<(PathBuf, Checkpoint)> {
    let mut source = BlockStore::open(&root.join("source"))?;
    let head = source.initialize(
        2,
        (0..10_001u32).map(|number| Mutation::Set {
            key: number.to_be_bytes().to_vec(),
            value: vec![42; 32],
        }),
        b"source",
    )?;
    let reader = source.reader().unwrap();
    let state = reader.latest()?;
    let path = root.join("snapshot.zlib");
    let mut file = File::create(&path)?;
    let nodes = state.write_go_snapshot(
        &[
            ("height", head.height.to_be_bytes().to_vec()),
            ("hash", head.app_hash.to_vec()),
        ],
        &mut file,
    )?;
    drop(file);
    let bytes = fs::read(&path)?;
    Ok((
        path,
        Checkpoint {
            format: 1,
            network: "minter-cancel-test".into(),
            height: 2,
            block_hash: [1; 32],
            app_hash: head.app_hash,
            snapshot_sha256: Sha256::digest(&bytes).into(),
            bytes: bytes.len() as u64,
            nodes,
        },
    ))
}

#[test]
fn cancelled_io_keeps_its_type_through_read_exact_and_context() -> Result<()> {
    struct OneByteThenCancel(OperationControl);
    impl Read for OneByteThenCancel {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer[0] = 7;
            self.0.cancel();
            Ok(1)
        }
    }
    let control = OperationControl::default();
    let mut reader = control.reader(OneByteThenCancel(control.clone()));
    let error = reader.read_exact(&mut [0; 2]).unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::Interrupted);
    let error = Err::<(), _>(error).context("hash snapshot").unwrap_err();
    assert!(OperationCancelled::caused(&error));
    let nested = anyhow::Error::new(io::Error::other(io::Error::other(OperationCancelled)));
    assert!(OperationCancelled::caused(&nested));
    let ordinary = anyhow::Error::new(io::Error::other("operation cancelled"));
    assert!(!OperationCancelled::caused(&ordinary));
    Ok(())
}

#[test]
fn cancelled_after_a_partial_batch_resumes_without_publishing_head() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (snapshot, trusted) = large_snapshot(temp.path())?;
    let path = temp.path().join("receiver");
    let mut store = BlockStore::open_for_checkpoint(&path, &trusted)?;
    let stop = OperationControl::default();
    let requested = stop.clone();
    let seen = Arc::new(AtomicU64::new(0));
    let recorded = seen.clone();
    let control = stop.with_snapshot_progress(move |done, total| {
        if done >= 10_000 && done < total {
            recorded.store(done, Ordering::Relaxed);
            requested.cancel();
        }
    });
    let error = store
        .install_checkpoint_with_control(File::open(&snapshot)?, &trusted, &control, |_| Ok(()))
        .unwrap_err();
    assert!(OperationCancelled::caused(&error), "{error:#}");
    assert!(seen.load(Ordering::Relaxed) >= 10_000);
    assert_eq!(store.head(), None);
    drop(store);
    assert!(StateStore::open(&path).is_err());
    let mut other = trusted.clone();
    other.block_hash = [2; 32];
    assert!(BlockStore::open_for_checkpoint(&path, &other).is_err());
    let mut store = BlockStore::open_for_checkpoint(&path, &trusted)?;
    store.install_checkpoint(File::open(&snapshot)?, &trusted, |_| Ok(()))?;
    assert_eq!(store.head().unwrap().app_hash, trusted.app_hash);
    drop(store);
    let reopened = StateStore::open(&path)?;
    assert_eq!(reopened.value(&0u32.to_be_bytes())?, Some(vec![42; 32]));
    assert_eq!(
        reopened.value(&10_000u32.to_be_bytes())?,
        Some(vec![42; 32])
    );
    Ok(())
}

#[test]
fn native_partial_import_pins_the_whole_package_digest() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (snapshot, trusted) = large_snapshot(temp.path())?;
    let path = temp.path().join("native-receiver");
    let digest = [7; 32];
    let mut store = BlockStore::open_for_native_checkpoint(&path, &trusted, digest)?;
    let stop = OperationControl::default();
    let requested = stop.clone();
    let control = stop.with_snapshot_progress(move |done, total| {
        if done >= 10_000 && done < total {
            requested.cancel();
        }
    });
    let error = store
        .install_native_checkpoint_with_control(
            File::open(&snapshot)?,
            &trusted,
            digest,
            &control,
            |_| {
                Ok((
                    (),
                    NativeCheckpointWrite {
                        metadata: b"native".to_vec(),
                        block_record: Vec::new(),
                        validators: Vec::new(),
                        committed_evidence: Vec::new(),
                    },
                ))
            },
        )
        .unwrap_err();
    assert!(OperationCancelled::caused(&error));
    drop(store);
    assert!(BlockStore::open_for_native_checkpoint(&path, &trusted, [8; 32]).is_err());
    assert!(BlockStore::open_for_checkpoint(&path, &trusted).is_err());
    let resumed = BlockStore::open_for_native_checkpoint(&path, &trusted, digest)?;
    assert_eq!(resumed.head(), None);
    drop(resumed);

    // A database from before native identity pinning adopts that identity only
    // when authenticated installation starts, not merely when it is opened.
    let legacy = temp.path().join("legacy-receiver");
    let mut store = BlockStore::open_for_checkpoint(&legacy, &trusted)?;
    let stop = OperationControl::default();
    let request = stop.clone();
    let control = stop.with_snapshot_progress(move |done, _| {
        if done > 0 {
            request.cancel();
        }
    });
    assert!(
        store
            .install_checkpoint_with_control(File::open(&snapshot)?, &trusted, &control, |_| Ok(()))
            .is_err()
    );
    drop(store);
    drop(BlockStore::open_for_native_checkpoint(
        &legacy, &trusted, [8; 32],
    )?);
    let mut store = BlockStore::open_for_native_checkpoint(&legacy, &trusted, digest)?;
    let stop = OperationControl::default();
    let request = stop.clone();
    let control = stop.with_snapshot_progress(move |_, _| request.cancel());
    let error = store
        .install_native_checkpoint_with_control(
            File::open(&snapshot)?,
            &trusted,
            digest,
            &control,
            |_| {
                Ok((
                    (),
                    NativeCheckpointWrite {
                        metadata: Vec::new(),
                        block_record: Vec::new(),
                        validators: Vec::new(),
                        committed_evidence: Vec::new(),
                    },
                ))
            },
        )
        .unwrap_err();
    assert!(OperationCancelled::caused(&error));
    drop(store);
    assert!(BlockStore::open_for_native_checkpoint(&legacy, &trusted, [8; 32]).is_err());
    Ok(())
}

#[test]
fn cancelled_in_application_prepare_does_not_publish_and_bad_hash_is_not_cancelled() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let snapshot = temp.path().join("go.zlib");
    fs::write(&snapshot, include_bytes!("fixtures/go-checkpoint.zlib"))?;
    let trusted: Checkpoint = serde_json::from_str(include_str!("fixtures/go-checkpoint.json"))?;
    let path = temp.path().join("receiver");
    let mut store = BlockStore::open_for_checkpoint(&path, &trusted)?;
    let control = OperationControl::default();
    let error = store
        .install_checkpoint_with_control(File::open(&snapshot)?, &trusted, &control, |_| {
            control.cancel();
            Ok(())
        })
        .unwrap_err();
    assert!(OperationCancelled::caused(&error));
    assert_eq!(store.head(), None);
    drop(store);
    let mut store = BlockStore::open_for_checkpoint(&path, &trusted)?;
    store.install_checkpoint(File::open(&snapshot)?, &trusted, |_| Ok(()))?;
    drop(store);
    let mut corrupted = trusted.clone();
    corrupted.snapshot_sha256 = [0; 32];
    let mut bad = BlockStore::open_for_checkpoint(&temp.path().join("bad"), &corrupted)?;
    let error = bad
        .install_checkpoint_with_control(
            File::open(&snapshot)?,
            &corrupted,
            &OperationControl::default(),
            |_| Ok(()),
        )
        .unwrap_err();
    assert!(!OperationCancelled::caused(&error));
    assert_eq!(bad.head(), None);
    Ok(())
}
