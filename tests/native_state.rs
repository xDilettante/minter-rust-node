use minter_state::{BlockStore, Mutation};
use tempfile::TempDir;

fn set(value: &[u8]) -> Mutation {
    Mutation::Set {
        key: b"a".to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn genesis_nodes_use_version_one_and_metadata_survives_reopen() {
    let temp = TempDir::new().unwrap();
    let mut zero = BlockStore::open(&temp.path().join("zero")).unwrap();
    let native_path = temp.path().join("native");
    let mut native = BlockStore::open(&native_path).unwrap();
    let first = zero
        .initialize(0, [set(b"genesis")], b"genesis-zero")
        .unwrap();
    let imported = native
        .initialize(100, [set(b"genesis")], b"genesis-native")
        .unwrap();
    assert_eq!(first.app_hash, imported.app_hash);
    assert_eq!(first.height, 0);
    assert_eq!(imported.height, 100);
    assert!(
        native
            .commit(102, [set(b"wrong height")], b"wrong")
            .is_err()
    );
    let committed = native.commit(101, [set(b"next")], b"executed").unwrap();
    drop(native);
    let native = BlockStore::open(&native_path).unwrap();
    assert_eq!(native.head(), Some(committed));
    assert_eq!(
        native.metadata().unwrap().as_deref(),
        Some(b"executed".as_slice())
    );
    assert_eq!(
        native.value_at(100, b"a").unwrap().as_deref(),
        Some(b"genesis".as_slice())
    );
    assert_eq!(
        native.value_at(101, b"a").unwrap().as_deref(),
        Some(b"next".as_slice())
    );
    assert!(native.value_at(102, b"a").is_err());
    let committed = zero.commit(1, [set(b"next")], b"executed").unwrap();
    assert_eq!(committed.height, 1);
    assert_eq!(
        zero.value_at(1, b"a").unwrap().as_deref(),
        Some(b"next".as_slice())
    );
}

#[test]
fn first_key_at_or_after_seeks_across_branches_and_versions() {
    let temp = TempDir::new().unwrap();
    let mut store = BlockStore::open(temp.path()).unwrap();
    let entries = [b"a", b"c", b"m", b"x"].map(|key| Mutation::Set {
        key: key.to_vec(),
        value: Vec::new(),
    });
    store.initialize(100, entries, b"genesis").unwrap();
    store
        .commit(101, [Mutation::Remove(b"c".to_vec())], b"block")
        .unwrap();

    let first = |height, key: &[u8]| store.first_key_at_or_after(height, key).unwrap();
    assert_eq!(first(100, b""), Some(b"a".to_vec()));
    assert_eq!(first(100, b"b"), Some(b"c".to_vec()));
    assert_eq!(first(100, b"c"), Some(b"c".to_vec()));
    assert_eq!(first(100, b"m0"), Some(b"x".to_vec()));
    assert_eq!(first(100, b"z"), None);
    assert_eq!(first(101, b"b"), Some(b"m".to_vec()));
    let last = |height, key: &[u8]| store.last_key_before(height, key).unwrap();
    assert_eq!(last(100, b"z"), Some(b"x".to_vec()));
    assert_eq!(last(100, b"m"), Some(b"c".to_vec()));
    assert_eq!(last(101, b"m"), Some(b"a".to_vec()));
    assert_eq!(last(100, b"a"), None);

    let range = |height, start: &[u8], end: &[u8]| {
        let mut found = Vec::new();
        store
            .visit_range_at(height, start..end, |key, value| {
                assert!(value.is_empty());
                found.push(key.to_vec());
                Ok(())
            })
            .unwrap();
        found
    };
    assert_eq!(range(100, b"b", b"x"), [b"c", b"m"]);
    assert_eq!(range(101, b"b", b"x"), [b"m"]);
    assert!(range(101, b"a", b"a").is_empty());

    let keys = [b"a".as_slice(), b"c", b"m", b"z"];
    let lookup = |height| {
        let mut found = Vec::new();
        store
            .visit_keys_at(height, &keys, |key, _| {
                found.push(key.to_vec());
                Ok(())
            })
            .unwrap();
        found
    };
    assert_eq!(lookup(100), [b"a", b"c", b"m"]);
    assert_eq!(lookup(101), [b"a", b"m"]);
    assert!(
        store
            .visit_keys_at(101, &[b"c", b"a"], |_, _| Ok(()))
            .is_err()
    );
}
