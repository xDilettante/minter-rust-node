use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use minter_state::{BlockStore, Checkpoint, IavlWriter, Mutation, StateStore};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
};
use tempfile::TempDir;

const SNAPSHOT: &[u8] = include_bytes!("fixtures/go-checkpoint.zlib");

fn fixture() -> Checkpoint {
    serde_json::from_str(include_str!("fixtures/go-checkpoint.json")).unwrap()
}

#[test]
fn imports_go_checkpoint_and_reads_committed_values_after_reopen() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    fs::write(&snapshot, SNAPSHOT).unwrap();
    let checkpoint = fixture();

    StateStore::import(&path, &snapshot, &checkpoint).unwrap();
    let store = StateStore::open(&path).unwrap();
    assert_eq!(store.checkpoint(), &checkpoint);
    assert_eq!(store.value(b"a").unwrap().as_deref(), Some(&b"current"[..]));
    assert_eq!(store.value(b"m").unwrap(), Some(Vec::new()));
    assert_eq!(store.value(b"c").unwrap(), None);
    assert_eq!(store.value(b"\x00").unwrap(), Some(vec![0, 255]));
    assert_eq!(
        store.app_metadata("height").unwrap(),
        Some(2u64.to_be_bytes().to_vec())
    );
    assert_eq!(
        store.app_metadata("hash").unwrap(),
        Some(checkpoint.app_hash.to_vec())
    );
    assert_eq!(store.app_metadata("versions").unwrap(), Some(vec![0xc0]));
}

#[test]
fn serves_go_compatible_proofs_for_the_committed_checkpoint() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    fs::write(&snapshot, SNAPSHOT).unwrap();
    let checkpoint = fixture();
    StateStore::import(&db, &snapshot, &checkpoint).unwrap();
    let store = StateStore::open(&db).unwrap();
    let reference: serde_json::Value = serde_json::from_str(include_str!(
        "../crates/minter-iavl/tests/fixtures/go-ics23.json"
    ))
    .unwrap();
    for item in reference["values"].as_array().unwrap() {
        let key = hex::decode(item["key"].as_str().unwrap()).unwrap();
        let expected = item["value"]
            .as_str()
            .map(|value| hex::decode(value).unwrap());
        let expected_proof = hex::decode(item["proof"].as_str().unwrap()).unwrap();
        let response = store.proof_at(checkpoint.height, &key).unwrap();
        assert_eq!(response.value, expected);
        assert_eq!(response.proof, expected_proof);
    }
    assert!(store.proof_at(checkpoint.height + 1, b"a").is_err());
}

#[test]
fn resumes_an_unpublished_import_only_with_the_same_checkpoint() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    fs::write(&snapshot, SNAPSHOT).unwrap();
    let checkpoint = fixture();
    let mut store = BlockStore::open_for_checkpoint(&path, &checkpoint).unwrap();
    store.set_keep_iavl_versions(120).unwrap();
    let aborted =
        store.install_checkpoint(fs::File::open(&snapshot).unwrap(), &checkpoint, |state| {
            assert_eq!(state.value(b"a").unwrap().as_deref(), Some(&b"current"[..]));
            anyhow::bail!("application did not accept metadata")
        });
    let aborted: anyhow::Result<()> = aborted;
    assert!(
        aborted
            .unwrap_err()
            .to_string()
            .contains("application did not accept")
    );
    assert_eq!(store.head(), None);
    drop(store);
    assert!(StateStore::open(&path).is_err());
    assert!(BlockStore::open(&path).is_err());
    let mut other = checkpoint.clone();
    other.block_hash[0] ^= 1;
    assert!(BlockStore::open_for_checkpoint(&path, &other).is_err());
    let mut store = BlockStore::open_for_checkpoint(&path, &checkpoint).unwrap();
    assert_eq!(store.head(), None);
    assert_eq!(store.keep_iavl_versions(), 120);
    store
        .install_checkpoint(fs::File::open(&snapshot).unwrap(), &checkpoint, |state| {
            assert_eq!(state.metadata("hash"), Some(checkpoint.app_hash.as_slice()));
            Ok(())
        })
        .unwrap();
    assert_eq!(store.head().unwrap().app_hash, checkpoint.app_hash);
    drop(store);
    let store = BlockStore::open(&path).unwrap();
    assert_eq!(store.head().unwrap().app_hash, checkpoint.app_hash);
    assert_eq!(store.keep_iavl_versions(), 120);
}

#[test]
fn opens_a_pruned_go_checkpoint_and_preserves_its_appdb_metadata() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    fs::write(&snapshot, SNAPSHOT).unwrap();
    let checkpoint = fixture();
    let mut store = BlockStore::open(&path).unwrap();
    store.set_keep_iavl_versions(1).unwrap();
    drop(store);
    StateStore::import(&path, &snapshot, &checkpoint).unwrap();

    let mut store = BlockStore::open(&path).unwrap();
    assert_eq!(store.keep_iavl_versions(), 1);
    let head = store
        .commit(
            checkpoint.height + 1,
            [Mutation::Remove(b"a".to_vec())],
            b"execution",
        )
        .unwrap();
    assert_eq!(store.earliest_height(), Some(head.height));
    drop(store);

    StateStore::import(&path, &snapshot, &checkpoint).unwrap();
    let state = StateStore::open(&path).unwrap();
    assert_eq!(state.checkpoint(), &checkpoint);
    assert_eq!(state.head(), head);
    assert_eq!(state.earliest_height(), head.height);
    assert!(state.root_at(checkpoint.height).is_err());
    assert_eq!(
        state.app_metadata("hash").unwrap(),
        Some(checkpoint.app_hash.to_vec())
    );
    assert_eq!(state.value_at(head.height, b"a").unwrap(), None);
    drop(state);
    let reopened = BlockStore::open(&path).unwrap();
    assert_eq!(reopened.head(), Some(head));
    assert_eq!(
        reopened.metadata().unwrap().as_deref(),
        Some(&b"execution"[..])
    );
}

#[test]
fn accepts_only_the_go_network_exporters_duplicate_zlib_checksum() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    let mut bytes = SNAPSHOT.to_vec();
    let checksum = &SNAPSHOT[SNAPSHOT.len() - 4..];
    bytes.extend(checksum);
    let mut checkpoint = fixture();
    checkpoint.bytes = bytes.len() as u64;
    checkpoint.snapshot_sha256 = Sha256::digest(&bytes).into();
    fs::write(&snapshot, &bytes).unwrap();
    StateStore::import(&path, &snapshot, &checkpoint).unwrap();
    assert_eq!(
        StateStore::open(&path).unwrap().head().app_hash,
        checkpoint.app_hash
    );

    *bytes.last_mut().unwrap() ^= 1;
    checkpoint.snapshot_sha256 = Sha256::digest(&bytes).into();
    fs::write(&snapshot, &bytes).unwrap();
    let path = temp.path().join("corrupt");
    let error = StateStore::import(&path, &snapshot, &checkpoint).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("trailing bytes after the zlib stream")
    );
    assert!(StateStore::open(&path).is_err());
}

#[test]
fn refuses_corrupted_bytes_without_creating_a_store() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    let mut corrupted = SNAPSHOT.to_vec();
    corrupted[5] ^= 1;
    fs::write(&snapshot, corrupted).unwrap();

    let error = StateStore::import(&path, &snapshot, &fixture()).unwrap_err();
    assert!(format!("{error:#}").contains("snapshot SHA-256"));
    assert!(!path.exists());
}

#[test]
fn rejects_untrusted_appdb_height_and_keeps_state_unavailable() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    fs::write(&snapshot, SNAPSHOT).unwrap();
    let mut checkpoint = fixture();
    checkpoint.height += 1;

    let error = StateStore::import(&path, &snapshot, &checkpoint).unwrap_err();
    assert!(format!("{error:#}").contains("AppDB height"));
    assert!(StateStore::open(&path).is_err());
}

#[test]
fn calculates_the_root_instead_of_trusting_the_appdb_hash() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    let mut frames = Vec::new();
    ZlibDecoder::new(SNAPSHOT).read_to_end(&mut frames).unwrap();
    let leaf = frames
        .windows(b"current".len())
        .position(|part| part == b"current")
        .unwrap();
    frames[leaf] = b't';
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&frames).unwrap();
    let corrupted = encoder.finish().unwrap();
    fs::write(&snapshot, &corrupted).unwrap();
    let mut checkpoint = fixture();
    checkpoint.snapshot_sha256 = Sha256::digest(&corrupted).into();
    checkpoint.bytes = corrupted.len() as u64;

    let error = StateStore::import(&path, &snapshot, &checkpoint).unwrap_err();
    assert!(format!("{error:#}").contains("calculated AppHash"));
    assert!(StateStore::open(&path).is_err());
}

#[derive(Deserialize)]
struct GoBlock {
    height: u64,
    #[serde(with = "hex::serde")]
    app_hash: [u8; 32],
    iavl_operations: Option<Vec<GoMutation>>,
}

#[derive(Deserialize)]
struct GoMutation {
    #[serde(with = "hex::serde")]
    key_hex: Vec<u8>,
    #[serde(default, with = "hex::serde")]
    value_hex: Vec<u8>,
    #[serde(default)]
    remove: bool,
}

#[test]
fn matches_go_mutations_keeps_history_and_rejects_an_unverified_root() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("rocksdb");
    let snapshot = temp.path().join("checkpoint.zlib");
    fs::write(&snapshot, SNAPSHOT).unwrap();
    let checkpoint = fixture();
    StateStore::import(&path, &snapshot, &checkpoint).unwrap();

    let blocks: Vec<GoBlock> = include_str!("fixtures/go-mutations.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut writer = IavlWriter::open(&path).unwrap();
    assert!(IavlWriter::open(&path).is_err());
    let initial = writer.head();
    let invalid = [Mutation::Set {
        key: b"a".to_vec(),
        value: b"divergent".to_vec(),
    }];
    assert!(
        writer
            .apply_block(initial.height + 1, invalid, blocks[0].app_hash)
            .is_err()
    );
    assert_eq!(writer.head(), initial);
    drop(writer);

    let store = StateStore::open(&path).unwrap();
    assert_eq!(store.head(), initial);
    assert!(store.root_at(initial.height + 1).is_err());
    drop(store);

    let mut writer = IavlWriter::open(&path).unwrap();
    for block in &blocks {
        let ops = block.iavl_operations.iter().flatten().map(|op| {
            if op.remove {
                Mutation::Remove(op.key_hex.clone())
            } else {
                Mutation::Set {
                    key: op.key_hex.clone(),
                    value: op.value_hex.clone(),
                }
            }
        });
        writer
            .apply_block(block.height, ops, block.app_hash)
            .unwrap_or_else(|error| panic!("Go fixture height {}: {error:#}", block.height));
    }
    let head = writer.head();
    drop(writer);

    let store = StateStore::open(&path).unwrap();
    assert_eq!(store.head(), head);
    assert_eq!(store.checkpoint(), &checkpoint);
    assert_eq!(store.root_at(2).unwrap(), checkpoint.app_hash);
    for block in &blocks {
        assert_eq!(store.root_at(block.height).unwrap(), block.app_hash);
    }
    assert_eq!(store.root_at(3).unwrap(), store.root_at(4).unwrap());
    assert_eq!(store.root_at(5).unwrap(), store.root_at(6).unwrap());
    assert_eq!(
        store.value_at(2, b"a").unwrap().as_deref(),
        Some(b"current".as_slice())
    );
    assert_eq!(store.value_at(5, b"a").unwrap(), None);
    assert_eq!(store.value_at(7, b"").unwrap(), Some(Vec::new()));
    assert_eq!(store.value_at(9, b"").unwrap(), None);
    assert_eq!(store.value_at(9, b"c").unwrap(), Some(b"inner".to_vec()));
    drop(store);

    let mut offline = BlockStore::open_existing(&path).unwrap();
    offline.set_keep_iavl_versions(2).unwrap();
    assert_eq!(offline.earliest_height(), Some(head.height - 1));
    drop(offline);
    assert!(IavlWriter::open(&path).is_err());
    assert!(BlockStore::open(&path).is_err());
    let store = StateStore::open(&path).unwrap();
    assert!(store.root_at(2).is_err());
    assert_eq!(store.root_at(head.height).unwrap(), head.app_hash);
}
