use base64::{Engine, engine::general_purpose::STANDARD};
use minter_api::{decode_events, event_struct, protobuf_struct};
use minter_app::Application;
use minter_state::{BlockStore, Checkpoint, StateStore};
use prost::Message;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, path::Path};
use tempfile::TempDir;
use tendermint_proto::v0_34::abci::{Request, RequestCommit, Response, request};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    init: Exchange,
    genesis_hash: String,
    blocks: Vec<Block>,
    pending: Snapshot,
    unknown: Snapshot,
    blocked: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HaltFixture {
    init: Exchange,
    genesis_hash: String,
    blocks: Vec<Block>,
    pending: Snapshot,
    blocked: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommissionFixture {
    init: Exchange,
    genesis_hash: String,
    blocks: Vec<Block>,
    pending: Snapshot,
}

#[derive(Deserialize)]
struct Snapshot {
    manifest: Checkpoint,
    snapshot: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Block {
    height: u64,
    exchanges: Vec<Exchange>,
    app_hash: String,
    versions: Value,
    events: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct Exchange {
    label: String,
    request: String,
    response: String,
}

fn request(input: &str) -> Request {
    Request::decode(STANDARD.decode(input).unwrap().as_slice()).unwrap()
}

fn exchange(app: &mut Application, step: &Exchange) {
    let actual = app
        .handle(request(&step.request))
        .unwrap_or_else(|err| panic!("{}: {err:#}", step.label));
    let expected = Response::decode(STANDARD.decode(&step.response).unwrap().as_slice()).unwrap();
    assert_eq!(actual, expected, "{}", step.label);
}

#[test]
fn matches_go_for_historical_transactions() {
    let steps: Vec<Exchange> = serde_json::from_str(include_str!(
        "fixtures/go-historical-transactions-v034.json"
    ))
    .unwrap();
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("state");
    let mut app = Application::open(&db, 1).unwrap();
    for step in &steps {
        exchange(&mut app, step);
        if step.label == "Commit 717" {
            let head = app.head().unwrap();
            assert_eq!(head.height, 717);
            drop(app);
            app = Application::open(&db, 1).unwrap();
            assert_eq!(app.head(), Some(head));
        }
    }
    assert_eq!(app.head().unwrap().height, 718);
}

fn assert_block(path: &Path, block: &Block) {
    let store = BlockStore::open_existing(path).unwrap();
    let reader = store.reader().unwrap();
    let state = reader.latest().unwrap();
    let head = state.head();
    assert_eq!(head.height, block.height);
    assert_eq!(
        hex::encode(head.app_hash),
        block.app_hash,
        "height {}",
        block.height
    );
    let metadata: Value =
        serde_json::from_slice(&state.execution_metadata().unwrap().unwrap()).unwrap();
    assert_eq!(
        metadata["versions"], block.versions,
        "height {}",
        block.height
    );
    let raw = state.block_events(block.height).unwrap().unwrap();
    let actual: Vec<_> = decode_events(&raw)
        .unwrap()
        .into_iter()
        .map(|event| event_struct(event, &HashSet::new()).unwrap().unwrap())
        .collect();
    let expected: Vec<_> = block
        .events
        .iter()
        .flatten()
        .map(|event| protobuf_struct(event.clone()).unwrap())
        .collect();
    assert_eq!(actual, expected, "events at {}", block.height);
    let expired = [b"u".as_slice(), &block.height.to_le_bytes()].concat();
    assert!(
        state.value(&expired).unwrap().is_none(),
        "expired proposal at {}",
        block.height
    );
}

fn replay(path: &Path, fixture: &Fixture, from: usize) {
    for (index, block) in fixture.blocks.iter().enumerate().skip(from) {
        if block.height == 720 {
            let mut app = Application::open(path, 1).unwrap();
            for step in &block.exchanges[..block.exchanges.len() - 1] {
                exchange(&mut app, step);
            }
            drop(app);
            assert_block(path, &fixture.blocks[index - 1]);
            let store = BlockStore::open_existing(path).unwrap();
            let reader = store.reader().unwrap();
            assert!(
                reader
                    .latest()
                    .unwrap()
                    .block_events(block.height)
                    .unwrap()
                    .is_none()
            );
        }
        let mut app = Application::open(path, 1).unwrap();
        for step in &block.exchanges {
            exchange(&mut app, step);
        }
        drop(app);
        assert_block(path, block);
    }
    assert_stops(path, fixture);
}

fn assert_stops(path: &Path, fixture: &Fixture) {
    let mut app = Application::open(path, 1).unwrap();
    let head = app.head().unwrap();
    assert_eq!(head.height, 724);
    let error = app
        .handle(request(&fixture.blocked))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "network upgrade v999_local requires a compatible node at height 725"
    );
    let commit = Request {
        value: Some(request::Value::Commit(RequestCommit {})),
    };
    assert!(app.handle(commit).is_err());
    assert_eq!(app.head(), Some(head));
    drop(app);
    assert_eq!(Application::open(path, 1).unwrap().head(), Some(head));
}

fn import(path: &Path, reference: &Snapshot) {
    let file = path.with_extension("zlib");
    std::fs::write(&file, STANDARD.decode(&reference.snapshot).unwrap()).unwrap();
    StateStore::import(path, &file, &reference.manifest).unwrap();
}

#[test]
fn matches_go_network_votes_and_recovers_the_upgrade_schedule() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/go-update-votes-v034.json")).unwrap();
    let temp = TempDir::new().unwrap();

    let genesis = temp.path().join("genesis");
    let mut app = Application::open(&genesis, 1).unwrap();
    exchange(&mut app, &fixture.init);
    assert_eq!(
        hex::encode(app.head().unwrap().app_hash),
        fixture.genesis_hash
    );
    drop(app);
    replay(&genesis, &fixture, 0);

    let pending = temp.path().join("pending");
    import(&pending, &fixture.pending);
    let from = fixture
        .blocks
        .iter()
        .position(|block| block.height == fixture.pending.manifest.height + 1)
        .unwrap();
    replay(&pending, &fixture, from);

    let unknown = temp.path().join("unknown");
    import(&unknown, &fixture.unknown);
    assert_stops(&unknown, &fixture);
}

fn has_halt_row(path: &Path, height: u64) -> bool {
    let store = BlockStore::open_existing(path).unwrap();
    let reader = store.reader().unwrap();
    let state = reader.latest().unwrap();
    let key = [b"h".as_slice(), &height.to_le_bytes()].concat();
    state.value(&key).unwrap().is_some()
}

#[test]
fn matches_go_halt_votes_and_preserves_the_last_commit() {
    let fixture: HaltFixture =
        serde_json::from_str(include_str!("fixtures/go-halt-votes-v034.json")).unwrap();
    let temp = TempDir::new().unwrap();
    let genesis = temp.path().join("genesis");
    let mut app = Application::open(&genesis, 1).unwrap();
    exchange(&mut app, &fixture.init);
    assert_eq!(
        hex::encode(app.head().unwrap().app_hash),
        fixture.genesis_hash
    );
    drop(app);
    assert!(
        !has_halt_row(&genesis, 717),
        "Go skips exported genesis halts"
    );

    let pending = temp.path().join("pending");
    import(&pending, &fixture.pending);
    for (path, start) in [
        (&genesis, 716),
        (&pending, fixture.pending.manifest.height + 1),
    ] {
        for block in fixture.blocks.iter().filter(|block| block.height >= start) {
            let mut app = Application::open(path, 1).unwrap();
            if block.height == 718 {
                let prior = app.head();
                for step in &block.exchanges[..block.exchanges.len() - 1] {
                    exchange(&mut app, step);
                }
                drop(app);
                assert!(has_halt_row(path, 718));
                app = Application::open(path, 1).unwrap();
                assert_eq!(app.head(), prior);
            }
            for step in &block.exchanges {
                exchange(&mut app, step);
            }
            let head = app.head().unwrap();
            assert_eq!(head.height, block.height);
            assert_eq!(hex::encode(head.app_hash), block.app_hash);
            drop(app);
            match block.height {
                718 | 719 => assert!(!has_halt_row(path, block.height)),
                720 => assert!(has_halt_row(path, 720)),
                _ => {}
            }
        }
        assert!(has_halt_row(path, 722));
        let mut app = Application::open(path, 1).unwrap();
        let head = app.head();
        assert_eq!(head.unwrap().height, 721);
        for _ in 0..2 {
            let error = app.handle(request(&fixture.blocked)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "application halted by validator votes at height 722"
            );
            assert_eq!(app.head(), head);
        }
        let commit = Request {
            value: Some(request::Value::Commit(RequestCommit {})),
        };
        assert!(app.handle(commit).is_err());
        drop(app);
        assert_eq!(Application::open(path, 1).unwrap().head(), head);
    }
}

#[test]
fn matches_go_commission_votes_and_uses_the_committed_prices() {
    let fixture: CommissionFixture =
        serde_json::from_str(include_str!("fixtures/go-commission-votes-v034.json")).unwrap();
    let temp = TempDir::new().unwrap();
    let genesis = temp.path().join("genesis");
    let mut app = Application::open(&genesis, 1).unwrap();
    exchange(&mut app, &fixture.init);
    assert_eq!(
        hex::encode(app.head().unwrap().app_hash),
        fixture.genesis_hash
    );
    for block in &fixture.blocks {
        for step in &block.exchanges {
            exchange(&mut app, step);
        }
    }
    drop(app);
    assert_block(&genesis, fixture.blocks.last().unwrap());

    let pending = temp.path().join("pending");
    import(&pending, &fixture.pending);
    for block in fixture
        .blocks
        .iter()
        .filter(|block| block.height > fixture.pending.manifest.height)
    {
        let mut app = Application::open(&pending, 1).unwrap();
        if block.height == 720 {
            let previous = app.head();
            for step in &block.exchanges[..block.exchanges.len() - 1] {
                exchange(&mut app, step);
            }
            drop(app);
            app = Application::open(&pending, 1).unwrap();
            assert_eq!(app.head(), previous);
        }
        for step in &block.exchanges {
            exchange(&mut app, step);
        }
        drop(app);
        assert_block(&pending, block);
        let store = BlockStore::open_existing(&pending).unwrap();
        let reader = store.reader().unwrap();
        let state = reader.latest().unwrap();
        let key = [b"p".as_slice(), &block.height.to_le_bytes()].concat();
        assert!(state.value(&key).unwrap().is_none());
    }
}
