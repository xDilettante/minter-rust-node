use base64::{Engine, engine::general_purpose::STANDARD};
use minter_app::Application;
use minter_state::{BlockStore, Checkpoint, StateStore};
use prost::Message;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tendermint_proto::v0_34::abci::{Request, Response};

#[derive(Deserialize)]
struct Scenario {
    name: String,
    manifest: Checkpoint,
    snapshot: String,
    #[serde(rename = "transactionChainId")]
    network: u8,
    #[serde(default)]
    initialize: Vec<Exchange>,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    height: u64,
    #[serde(default)]
    checks: Vec<Exchange>,
    exchanges: Vec<Exchange>,
    readings: Vec<Reading>,
}

#[derive(Deserialize)]
struct Exchange {
    request: String,
    response: String,
}

#[derive(Deserialize)]
struct Reading {
    key: String,
    value: Option<String>,
}

fn compare(app: &mut Application, steps: &[Exchange], name: &str, height: u64) {
    for (index, step) in steps.iter().enumerate() {
        let request = Request::decode(STANDARD.decode(&step.request).unwrap().as_slice()).unwrap();
        let expected =
            Response::decode(STANDARD.decode(&step.response).unwrap().as_slice()).unwrap();
        let actual = app
            .handle(request)
            .unwrap_or_else(|error| panic!("{name}, height {height}, call {index}: {error:#}"));
        assert_eq!(actual, expected, "{name}, height {height}, call {index}");
    }
}

fn replay(names: &[&str]) {
    let scenarios: Vec<Scenario> =
        serde_json::from_str(include_str!("fixtures/go-coin-recreation-v034.json")).unwrap();
    let mut count = 0;
    for scenario in scenarios {
        if !names.contains(&scenario.name.as_str()) {
            continue;
        }
        count += 1;
        let temporary = TempDir::new().unwrap();
        let database = temporary.path().join("state");
        let snapshot = temporary.path().join("initial.zlib");
        std::fs::write(&snapshot, STANDARD.decode(&scenario.snapshot).unwrap()).unwrap();
        if scenario.initialize.is_empty() {
            StateStore::import(&database, &snapshot, &scenario.manifest).unwrap();
        } else {
            let mut app = Application::open(&database, scenario.network).unwrap();
            compare(
                &mut app,
                &scenario.initialize,
                &scenario.name,
                scenario.manifest.height,
            );
        }
        for block in scenario.blocks {
            // Each Go probe uses an isolated mempool. Match that lifetime in Rust.
            for probe in &block.checks {
                let mut app = Application::open(&database, scenario.network).unwrap();
                compare(
                    &mut app,
                    std::slice::from_ref(probe),
                    &scenario.name,
                    block.height,
                );
            }
            let mut app = Application::open(&database, scenario.network).unwrap();
            compare(&mut app, &block.exchanges, &scenario.name, block.height);
            let committed = app.head().unwrap();
            assert_eq!(committed.height, block.height);
            drop(app);
            let reopened = Application::open(&database, scenario.network).unwrap();
            assert_eq!(reopened.head(), Some(committed));
            drop(reopened);
            let store = BlockStore::open_existing(&database).unwrap();
            let reader = store.reader().unwrap();
            let state = reader.latest().unwrap();
            let mut file = tempfile::NamedTempFile::new_in(temporary.path()).unwrap();
            let metadata = [
                ("height", block.height.to_be_bytes().to_vec()),
                ("hash", committed.app_hash.to_vec()),
            ];
            let nodes = state
                .write_go_snapshot(&metadata, file.as_file_mut())
                .unwrap();
            file.as_file().sync_all().unwrap();
            let bytes = std::fs::read(file.path()).unwrap();
            let manifest = Checkpoint {
                height: block.height,
                app_hash: committed.app_hash,
                snapshot_sha256: Sha256::digest(&bytes).into(),
                bytes: bytes.len() as u64,
                nodes,
                ..scenario.manifest.clone()
            };
            let restored = temporary.path().join(format!("restored-{}", block.height));
            StateStore::import(&restored, file.path(), &manifest).unwrap();
            let imported = StateStore::open(&restored).unwrap();
            assert_eq!(imported.head(), state.head());
            for reading in block.readings {
                let key = STANDARD.decode(reading.key).unwrap();
                let expected = reading.value.map(|value| STANDARD.decode(value).unwrap());
                assert_eq!(
                    state.value(&key).unwrap(),
                    expected,
                    "{}: physical key {} at {}",
                    scenario.name,
                    hex::encode(&key),
                    block.height,
                );
                assert_eq!(
                    imported.value_at(block.height, &key).unwrap(),
                    expected,
                    "{}: roundtrip key {} at {}",
                    scenario.name,
                    hex::encode(&key),
                    block.height,
                );
            }
        }
    }
    assert_eq!(count, names.len(), "missing conformance scenarios");
}

#[test]
fn matches_go_recreation_versions_owners_commissions_and_reimport() {
    replay(&[
        "recreation-versions-and-owner",
        "recreation-after-go-export-import",
        "recreation-gas-coins",
        "recreation-price-coin",
        "recreation-zero-commission",
        "recreation-after-physical-go-reopen",
        "recreation-transaction-network-2",
    ]);
}

#[test]
fn matches_go_recreation_validation_and_balance_priority() {
    replay(&[
        "recreation-token-validation",
        "recreation-coin-validation-order",
        "recreation-reserve-and-total-balance",
    ]);
}

#[test]
fn preserves_go_recreation_uint16_version_behavior() {
    replay(&["recreation-version-overflow"]);
}
