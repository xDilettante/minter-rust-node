use base64::{Engine, engine::general_purpose::STANDARD};
use minter_api::{decode_events, event_struct, protobuf_struct};
use minter_app::Application;
use minter_state::{BlockStore, Checkpoint, StateStore};
use prost::Message;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use tempfile::TempDir;
use tendermint_proto::v0_34::abci::{Request, Response};

#[derive(Deserialize)]
struct Scenario {
    name: String,
    manifest: Checkpoint,
    snapshot: String,
    blocks: Vec<Block>,
    #[serde(default, rename = "transactionChainId")]
    transaction_chain_id: Option<u8>,
    #[serde(default)]
    initialize: Vec<Exchange>,
}

#[derive(Deserialize)]
struct Block {
    height: u64,
    #[serde(default)]
    checks: Vec<Exchange>,
    exchanges: Vec<Exchange>,
    events: Option<Vec<Value>>,
    readings: Vec<Reading>,
}

#[derive(Deserialize)]
struct Reading {
    key: String,
    value: Option<String>,
}

#[derive(Deserialize)]
struct Exchange {
    request: String,
    response: String,
}

fn compare(app: &mut Application, steps: &[Exchange], name: &str, height: u64) {
    for (index, step) in steps.iter().enumerate() {
        let request = Request::decode(STANDARD.decode(&step.request).unwrap().as_slice()).unwrap();
        let expected =
            Response::decode(STANDARD.decode(&step.response).unwrap().as_slice()).unwrap();
        let actual = app
            .handle(request)
            .unwrap_or_else(|err| panic!("{name} at {height}, ABCI call {index}: {err:#}"));
        assert_eq!(actual, expected, "{name} at {height}, ABCI call {index}");
    }
}

#[test]
fn matches_go_candidate_admission_pruning_and_unbonding() {
    replay(
        include_str!("fixtures/go-candidate-lifecycle-v034.json"),
        false,
    );
}

#[test]
fn matches_go_candidate_commission_boundaries_and_imported_change_height() {
    replay(
        include_str!("fixtures/go-candidate-commission-v034.json"),
        true,
    );
}

#[test]
fn rejects_missing_reserve_for_a_reserve_coin_in_go_genesis() {
    let scenarios: Vec<Scenario> =
        serde_json::from_str(include_str!("fixtures/go-candidate-commission-v034.json")).unwrap();
    let scenario = scenarios
        .iter()
        .find(|row| !row.initialize.is_empty())
        .unwrap();
    let mut request = Request::decode(
        STANDARD
            .decode(&scenario.initialize[0].request)
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    let Some(tendermint_proto::v0_34::abci::request::Value::InitChain(init)) = &mut request.value
    else {
        panic!("expected InitChain fixture");
    };
    let mut genesis: Value = serde_json::from_slice(&init.app_state_bytes).unwrap();
    let coin = genesis["coins"][0].as_object_mut().unwrap();
    assert_ne!(coin["crr"], "0");
    assert!(coin.remove("reserve").is_some());
    init.app_state_bytes = serde_json::to_vec(&genesis).unwrap().into();
    let directory = TempDir::new().unwrap();
    let mut app = Application::open(&directory.path().join("state"), 2).unwrap();
    assert!(
        app.handle(request).is_err(),
        "a reserve coin needs an explicit reserve"
    );
}

#[test]
fn matches_go_candidate_public_key_replacement_and_catalogs() {
    replay(include_str!("fixtures/go-candidate-key-v034.json"), true);
}

#[test]
fn matches_go_candidate_keys_in_frozen_records_and_deferred_events() {
    replay(
        include_str!("fixtures/go-candidate-key-frozen-v034.json"),
        true,
    );
}

#[test]
fn preserves_committed_state_when_go_key_change_fails_during_rewards() {
    #[derive(Deserialize)]
    struct Initial {
        manifest: Checkpoint,
        snapshot: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FailedBlock {
        initial: Initial,
        exchanges: Vec<Exchange>,
        end_block_request: String,
        control: Scenario,
    }
    let case: FailedBlock =
        serde_json::from_str(include_str!("fixtures/go-candidate-key-payout-v034.json")).unwrap();
    assert_eq!(
        case.initial.manifest.app_hash,
        case.control.manifest.app_hash
    );
    let directory = TempDir::new().unwrap();
    let db = directory.path().join("state");
    let snapshot = directory.path().join("snapshot.zlib");
    std::fs::write(&snapshot, STANDARD.decode(&case.initial.snapshot).unwrap()).unwrap();
    StateStore::import(&db, &snapshot, &case.initial.manifest).unwrap();
    let before = StateStore::open(&db).unwrap().head();
    {
        let mut app = Application::open(&db, 1).unwrap();
        compare(&mut app, &case.exchanges, "payout-failure", 720);
        let end =
            Request::decode(STANDARD.decode(&case.end_block_request).unwrap().as_slice()).unwrap();
        assert!(
            app.handle(end).is_err(),
            "Go cannot finish this reward block"
        );
        assert_eq!(app.head().unwrap(), before);
    }
    let mut app = Application::open(&db, 1).unwrap();
    assert_eq!(app.head().unwrap(), before);
    compare(
        &mut app,
        &case.control.blocks[0].exchanges,
        "normal-payout-after-reopen",
        720,
    );
    assert_eq!(app.head().unwrap().height, 720);
}

fn replay(fixture: &str, snapshot_roundtrip: bool) {
    let scenarios: Vec<Scenario> = serde_json::from_str(fixture).unwrap();
    for scenario in scenarios {
        let name = &scenario.name;
        let network = scenario.transaction_chain_id.unwrap_or(1);
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("rocksdb");
        let snapshot = tmp.path().join("snapshot.zlib");
        std::fs::write(&snapshot, STANDARD.decode(scenario.snapshot).unwrap()).unwrap();
        if scenario.initialize.is_empty() {
            StateStore::import(&db, &snapshot, &scenario.manifest).unwrap();
        } else {
            let mut app = Application::open(&db, network).unwrap();
            compare(
                &mut app,
                &scenario.initialize,
                name,
                scenario.manifest.height,
            );
        }
        for block in scenario.blocks {
            let expected = block.events.unwrap_or_default();
            let mut app = Application::open(&db, network).unwrap();
            compare(&mut app, &block.checks, name, block.height);
            if expected
                .iter()
                .any(|event| event["type"] == "minter/RemoveCandidateEvent")
            {
                let head = app.head().unwrap();
                compare(
                    &mut app,
                    &block.exchanges[..block.exchanges.len() - 1],
                    name,
                    block.height,
                );
                drop(app);
                app = Application::open(&db, network).unwrap();
                assert_eq!(
                    app.head(),
                    Some(head),
                    "uncommitted removal changed the head"
                );
            }
            compare(&mut app, &block.exchanges, name, block.height);
            drop(app);

            let store = BlockStore::open_existing(&db).unwrap();
            let reader = store.reader().unwrap();
            let state = reader.latest().unwrap();
            assert_eq!(state.head().height, block.height);
            for reading in block.readings {
                let key = STANDARD.decode(&reading.key).unwrap();
                assert_eq!(
                    state.value(&key).unwrap(),
                    reading.value.map(|value| STANDARD.decode(value).unwrap()),
                    "IAVL key {} at {}",
                    hex::encode(key),
                    block.height,
                );
            }
            let raw = state.block_events(block.height).unwrap().unwrap();
            let events: Vec<_> = decode_events(&raw)
                .unwrap()
                .into_iter()
                .map(|event| event_struct(event, &HashSet::new()).unwrap().unwrap())
                .collect();
            let expected: Vec<_> = expected
                .into_iter()
                .map(|event| protobuf_struct(event).unwrap())
                .collect();
            assert_eq!(events, expected, "{name} at {}", block.height);
            if snapshot_roundtrip {
                let mut file = tempfile::NamedTempFile::new().unwrap();
                let metadata = [
                    ("height", block.height.to_be_bytes().to_vec()),
                    ("hash", state.head().app_hash.to_vec()),
                ];
                let nodes = state
                    .write_go_snapshot(&metadata, file.as_file_mut())
                    .unwrap();
                file.as_file().sync_all().unwrap();
                let Request {
                    value: Some(tendermint_proto::v0_34::abci::request::Value::BeginBlock(begin)),
                } = Request::decode(
                    STANDARD
                        .decode(&block.exchanges[0].request)
                        .unwrap()
                        .as_slice(),
                )
                .unwrap()
                else {
                    panic!("fixture block must begin with BeginBlock");
                };
                let bytes = std::fs::read(file.path()).unwrap();
                let manifest = Checkpoint {
                    height: block.height,
                    block_hash: begin.hash.as_ref().try_into().unwrap(),
                    app_hash: state.head().app_hash,
                    snapshot_sha256: Sha256::digest(&bytes).into(),
                    bytes: bytes.len() as u64,
                    nodes,
                    ..scenario.manifest.clone()
                };
                let restored = tmp.path().join(format!("restored-{}", block.height));
                StateStore::import(&restored, file.path(), &manifest).unwrap();
                let imported = StateStore::open(&restored).unwrap();
                assert_eq!(imported.head(), state.head());
                assert_eq!(
                    imported.value_at(block.height, b"c").unwrap(),
                    state.value(b"c").unwrap(),
                    "candidate models changed across Rust IAVL snapshot at {}",
                    block.height,
                );
            }
        }
    }
}
