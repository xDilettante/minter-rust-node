use minter_iavl::{Content, Node, node_key, root_key, verify_proof};
use minter_state::{BlockStore, Mutation};
use rocksdb::{DB, IteratorMode, Options};
use std::collections::BTreeSet;
use tempfile::TempDir;

fn set(key: &[u8], value: &[u8]) -> Mutation {
    Mutation::Set {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn migrates_archive_and_pins_old_queries_during_cleanup() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("state");
    let mut store = BlockStore::open(&path).unwrap();
    store
        .initialize(
            0,
            [set(b"a", b"1"), set(b"b", b"2"), set(b"c", b"3")],
            b"genesis",
        )
        .unwrap();
    store.commit(1, [set(b"b", b"4")], b"one").unwrap();
    let two = store
        .commit(2, [Mutation::Remove(b"a".to_vec())], b"two")
        .unwrap();
    let three = store.commit(3, [], b"three").unwrap();
    assert_eq!(two.app_hash, three.app_hash);
    drop(store);

    let mut store = BlockStore::open(&path).unwrap();
    let reader = store.reader().unwrap();
    let old = reader.at(1).unwrap();
    assert_eq!(reader.at_version(1).unwrap().head().height, 0);
    assert_eq!(reader.at_version(2).unwrap().head(), old.head());
    assert!(old.execution_metadata().is_err());
    let pinned_head = reader.latest().unwrap();
    assert_eq!(pinned_head.head(), three);
    assert_eq!(
        pinned_head.execution_metadata().unwrap().as_deref(),
        Some(b"three".as_slice())
    );
    store.set_keep_iavl_versions(2).unwrap();
    store.compact().unwrap();
    assert_eq!(store.earliest_height(), Some(2));
    assert!(reader.at(1).is_err());
    assert_eq!(old.value(b"a").unwrap(), Some(b"1".to_vec()));
    let proof = old.proof(b"b").unwrap();
    assert_eq!(
        verify_proof(old.head().app_hash, 2, b"b", &proof.proof).unwrap(),
        Some(b"4".to_vec())
    );
    let mut exported = Vec::new();
    old.visit_range(b"a"..b"d", |key, value| {
        exported.push((key.to_vec(), value.to_vec()));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        exported,
        [
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"4".to_vec()),
            (b"c".to_vec(), b"3".to_vec())
        ]
    );
    let retained = reader.at(2).unwrap();
    store
        .commit(
            4,
            [
                Mutation::Remove(b"b".to_vec()),
                Mutation::Remove(b"c".to_vec()),
            ],
            b"four",
        )
        .unwrap();
    assert_eq!(
        pinned_head.execution_metadata().unwrap().as_deref(),
        Some(b"three".as_slice())
    );
    drop(pinned_head);
    assert!(reader.at(2).is_err());
    assert_eq!(retained.value(b"c").unwrap(), Some(b"3".to_vec()));
    drop(retained);
    drop(old);
    drop(reader);
    drop(store);

    let mut store = BlockStore::open(&path).unwrap();
    assert_eq!(store.earliest_height(), Some(3));
    assert_eq!(store.keep_iavl_versions(), 2);
    assert_eq!(store.metadata().unwrap().unwrap(), b"four");
    store.commit(5, [set(b"c", b"3")], b"five").unwrap();
    assert_eq!(store.earliest_height(), Some(4));
    store.set_keep_iavl_versions(0).unwrap();
    store.commit(6, [], b"six").unwrap();
    assert_eq!(store.earliest_height(), Some(4));
    store.commit(7, [set(b"c", b"new")], b"seven").unwrap();
    drop(store);
    assert_reachable(&path, 5..=8);

    let mut store = BlockStore::open(&path).unwrap();
    store.set_keep_iavl_versions(1).unwrap();
    drop(store);
    let mut store = BlockStore::open(&path).unwrap();
    assert_eq!(store.keep_iavl_versions(), 1);
    assert_eq!(store.earliest_height(), Some(7));
    store.set_keep_iavl_versions(120).unwrap();
    assert_eq!(store.earliest_height(), Some(7));
    drop(store);
    assert_reachable(&path, 8..=8);
}

#[test]
fn keeps_one_root_and_deletes_a_retired_subtree_in_the_same_commit() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("state");
    let mut store = BlockStore::open(&path).unwrap();
    store.set_keep_iavl_versions(1).unwrap();
    store
        .initialize(99, [set(b"a", b"x"), set(b"b", b"y")], b"genesis")
        .unwrap();
    let latest = store
        .commit(100, [Mutation::Remove(b"a".to_vec())], b"latest")
        .unwrap();
    let proof = store.proof_at(100, b"b").unwrap();
    assert_eq!(
        verify_proof(latest.app_hash, 100, b"b", &proof.proof).unwrap(),
        Some(b"y".to_vec())
    );
    assert_eq!(store.earliest_height(), Some(100));
    assert!(store.proof_at(99, b"a").is_err());
    drop(store);
    assert_reachable(&path, 100..=100);
}

#[test]
fn retains_events_independently_of_iavl_across_restart() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("state");
    let mut store = BlockStore::open(&path).unwrap();
    store.set_keep_iavl_versions(1).unwrap();
    store.set_keep_event_blocks(2).unwrap();
    drop(store);

    let mut store = BlockStore::open(&path).unwrap();
    store.initialize(99, [set(b"a", b"x")], b"genesis").unwrap();
    let reader = store.reader().unwrap();
    let before = reader.latest().unwrap();
    store
        .commit_with_events(100, [], b"one", b"events")
        .unwrap();
    store.commit_with_events(101, [], b"two", b"").unwrap();
    assert_eq!(before.block_events(100).unwrap(), None);
    assert!(reader.at(100).is_err());
    assert_eq!(
        reader.latest().unwrap().block_events(100).unwrap(),
        Some(b"events".to_vec())
    );
    drop(before);
    drop(reader);
    drop(store);

    let mut store = BlockStore::open(&path).unwrap();
    let reader = store.reader().unwrap();
    let old = reader.latest().unwrap();
    store.commit_with_events(102, [], b"three", b"new").unwrap();
    let head = reader.latest().unwrap();
    assert_eq!(head.block_events(100).unwrap(), None);
    assert_eq!(head.block_events(101).unwrap(), Some(Vec::new()));
    assert_eq!(head.block_events(102).unwrap(), Some(b"new".to_vec()));
    assert_eq!(head.execution_metadata().unwrap(), Some(b"three".to_vec()));
    assert_eq!(old.block_events(100).unwrap(), Some(b"events".to_vec()));
}

fn assert_reachable(path: &std::path::Path, versions: std::ops::RangeInclusive<u64>) {
    let db = DB::open_for_read_only(&Options::default(), path, false).unwrap();
    let mut reachable = BTreeSet::new();
    for version in versions.clone() {
        let root = db.get(root_key(version)).unwrap().unwrap();
        if !root.is_empty() {
            collect(&db, root.try_into().unwrap(), &mut reachable);
        }
    }
    let mut nodes = BTreeSet::new();
    let mut roots = Vec::new();
    for item in db.iterator(IteratorMode::Start) {
        let (key, _) = item.unwrap();
        match key.first() {
            Some(b'n') => {
                nodes.insert(key[1..].try_into().unwrap());
            }
            Some(b'r') => roots.push(u64::from_be_bytes(key[1..].try_into().unwrap())),
            _ => {}
        }
    }
    assert_eq!(roots, versions.collect::<Vec<_>>());
    assert_eq!(nodes, reachable);
}

fn collect(db: &DB, hash: [u8; 32], reachable: &mut BTreeSet<[u8; 32]>) {
    if reachable.insert(hash) {
        let raw = db.get(node_key(hash)).unwrap().unwrap();
        if let Content::Branch { left, right } = Node::decode(&raw).unwrap().content {
            collect(db, left, reachable);
            collect(db, right, reachable);
        }
    }
}
