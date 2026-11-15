use base64::{Engine, engine::general_purpose::STANDARD};
use minter_api::{decode_events, event_struct, protobuf_struct};
use minter_app::Application;
use minter_state::{BlockStore, Checkpoint, StateStore};
use num_bigint::BigUint;
use prost::Message;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use tempfile::TempDir;
use tendermint_proto::v0_34::abci::{Request, Response};

#[derive(Deserialize)]
struct Fixture {
    name: String,
    manifest: Checkpoint,
    snapshot: String,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Block {
    height: u64,
    begin: Exchange,
    end: Exchange,
    commit: Exchange,
    app_hash: String,
    emission: String,
    events: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct Exchange {
    request: String,
    response: String,
}

fn exchange(app: &mut Application, case: &str, step: &str, pair: &Exchange) {
    let request = Request::decode(STANDARD.decode(&pair.request).unwrap().as_slice()).unwrap();
    let expected = Response::decode(STANDARD.decode(&pair.response).unwrap().as_slice()).unwrap();
    let actual = app
        .handle(request)
        .unwrap_or_else(|err| panic!("{case} {step}: {err:#}"));
    assert_eq!(actual, expected, "{case} {step}");
}

#[test]
fn matches_go_payouts_and_emission_through_restart() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("fixtures/go-reward-payouts-v034.json")).unwrap();
    for fixture in fixtures {
        let name = &fixture.name;
        let temp = TempDir::new().unwrap();
        let db = temp.path().join("rocksdb");
        let snapshot = temp.path().join("snapshot.zlib");
        std::fs::write(&snapshot, STANDARD.decode(fixture.snapshot).unwrap()).unwrap();
        StateStore::import(&db, &snapshot, &fixture.manifest).unwrap();

        let mut app = Application::open(&db, 1).unwrap();
        let initial = app.head().unwrap();
        assert_eq!(initial.height, fixture.manifest.height, "{name}");
        exchange(&mut app, name, "stage BeginBlock", &fixture.blocks[0].begin);
        exchange(&mut app, name, "stage EndBlock", &fixture.blocks[0].end);
        drop(app);
        let store = BlockStore::open_existing(&db).unwrap();
        assert_eq!(store.head(), Some(initial), "{name}");
        let reader = store.reader().unwrap();
        assert!(
            reader
                .latest()
                .unwrap()
                .block_events(initial.height + 1)
                .unwrap()
                .is_none(),
            "{name} exposed uncommitted events"
        );
        drop(reader);
        drop(store);

        for block in fixture.blocks {
            let mut app = Application::open(&db, 1).unwrap();
            exchange(&mut app, name, "BeginBlock", &block.begin);
            exchange(&mut app, name, "EndBlock", &block.end);
            exchange(&mut app, name, "Commit", &block.commit);
            drop(app);

            let store = BlockStore::open_existing(&db).unwrap();
            let reader = store.reader().unwrap();
            let state = reader.latest().unwrap();
            let head = state.head();
            assert_eq!(head.height, block.height, "{name}");
            assert_eq!(hex::encode(head.app_hash), block.app_hash, "{name}");
            let metadata: Value =
                serde_json::from_slice(&state.execution_metadata().unwrap().unwrap()).unwrap();
            let emission: BigUint = serde_json::from_value(metadata["emission"].clone()).unwrap();
            assert_eq!(emission.to_string(), block.emission, "{name}");

            let raw = state.block_events(block.height).unwrap().unwrap();
            let events: Vec<_> = decode_events(&raw)
                .unwrap()
                .into_iter()
                .map(|event| event_struct(event, &HashSet::new()).unwrap().unwrap())
                .collect();
            let expected: Vec<_> = block
                .events
                .unwrap_or_default()
                .into_iter()
                .map(|event| protobuf_struct(event).unwrap())
                .collect();
            assert_eq!(events, expected, "{name}");
        }
    }
}
