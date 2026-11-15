use minter_app::Application;
use minter_state::{Checkpoint, StateStore};
use prost::Message;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use tempfile::TempDir;
use tendermint_proto::v0_34::abci::{
    Request, RequestApplySnapshotChunk, RequestOfferSnapshot, RequestQuery, Response, Snapshot,
    request, response, response_apply_snapshot_chunk::Result as Apply,
    response_offer_snapshot::Result as Offer,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    genesis_height: u64,
    genesis_root: String,
    exchanges: Vec<Exchange>,
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
struct Exchange {
    label: String,
    request: String,
    response: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Block {
    height: u64,
    app_hash: String,
    accounts: Vec<Account>,
}

#[derive(Deserialize)]
struct Account {
    address: String,
    balance: String,
    nonce: u64,
    #[serde(default)]
    coin: u32,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/go-abci-v034.json")).unwrap()
}

fn exchange(app: &mut Application, input: &Exchange) {
    let request = Request::decode(hex::decode(&input.request).unwrap().as_slice()).unwrap();
    let mut expected = Response::decode(hex::decode(&input.response).unwrap().as_slice()).unwrap();
    let actual = app
        .handle(request)
        .unwrap_or_else(|err| panic!("{}: {err:#}", input.label));
    if let (Some(response::Value::Info(actual)), Some(response::Value::Info(expected))) =
        (&actual.value, &mut expected.value)
    {
        assert!(actual.version.starts_with("minter-rust/"));
        expected.version.clone_from(&actual.version);
    }
    assert_eq!(actual, expected, "{}", input.label);
}

fn open(path: &Path) -> Application {
    Application::open(path, 1).unwrap()
}

#[test]
fn executes_genesis_and_all_go_abci_blocks_with_historical_balances() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("rocksdb");
    let mut app = open(&db);
    let fixture = fixture();
    let mut blocks = fixture.blocks.iter();
    for item in &fixture.exchanges {
        exchange(&mut app, item);
        if item.label == "init" {
            let head = app.head().unwrap();
            assert_eq!(head.height, fixture.genesis_height);
            assert_eq!(hex::encode(head.app_hash), fixture.genesis_root);
            drop(app);
            app = open(&db);
            exchange(&mut app, item);
        }
        if item.label.starts_with("commit-") {
            let expected = blocks.next().unwrap();
            let head = app.head().unwrap();
            assert_eq!(head.height, expected.height);
            assert_eq!(hex::encode(head.app_hash), expected.app_hash);
        }
        if item.label == "commit-103" || item.label == "commit-108" {
            drop(app);
            app = open(&db);
        }
    }
    assert!(blocks.next().is_none());
    for block in &fixture.blocks {
        for account in &block.accounts {
            let mut data = hex::decode(&account.address).unwrap();
            if account.coin != 0 {
                data.extend(account.coin.to_le_bytes());
            }
            let result = app
                .handle(Request {
                    value: Some(request::Value::Query(RequestQuery {
                        data: data.into(),
                        path: "/balance".into(),
                        height: block.height as i64,
                        ..Default::default()
                    })),
                })
                .unwrap();
            let Some(response::Value::Query(query)) = result.value else {
                panic!("missing query response");
            };
            assert_eq!(query.code, 0);
            assert_eq!(query.height, block.height as i64);
            let value: serde_json::Value = serde_json::from_slice(&query.value).unwrap();
            assert_eq!(
                value,
                serde_json::json!({"balance": account.balance, "nonce": account.nonce})
            );
        }
    }
}

#[test]
fn a_delivered_but_uncommitted_transfer_is_replayed_after_reopen() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("rocksdb");
    let mut app = open(&db);
    let fixture = fixture();
    for item in &fixture.exchanges {
        exchange(&mut app, item);
        if item.label == "deliver-transfer" {
            break;
        }
    }
    let head = app.head().unwrap();
    assert_eq!(head.height, 101);
    drop(app);
    let mut app = open(&db);
    assert_eq!(app.head(), Some(head));
    for item in fixture
        .exchanges
        .iter()
        .skip_while(|item| item.label != "begin-102")
    {
        exchange(&mut app, item);
        if item.label == "info-102" {
            break;
        }
    }
    assert_eq!(app.head().unwrap().height, 102);
}

#[test]
fn matches_go_for_trading_and_matured_funds() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("rocksdb");
    let path = temp.path().join("snapshot.zlib");
    std::fs::write(&path, include_bytes!("fixtures/go-swap-v034.zlib")).unwrap();
    let checkpoint: Checkpoint =
        serde_json::from_str(include_str!("fixtures/go-swap-checkpoint.json")).unwrap();
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/go-swap-v034.json")).unwrap();
    StateStore::import(&db, &path, &checkpoint).unwrap();
    let mut app = open(&db);
    let head = app.head().unwrap();
    assert_eq!(head.height, fixture.genesis_height);
    assert_eq!(hex::encode(head.app_hash), fixture.genesis_root);
    let mut blocks = fixture.blocks.iter();
    for item in &fixture.exchanges {
        exchange(&mut app, item);
        if item.label.starts_with("commit-") {
            let expected = blocks.next().unwrap();
            let head = app.head().unwrap();
            assert_eq!(head.height, expected.height);
            assert_eq!(hex::encode(head.app_hash), expected.app_hash);
            drop(app);
            app = open(&db);
            assert_eq!(app.head(), Some(head));
        }
    }
    assert!(blocks.next().is_none());
}

#[test]
fn authenticates_go_snapshot_chunks_and_continues_execution() {
    let temp = TempDir::new().unwrap();
    let db = temp.path().join("rocksdb");
    let snapshot = include_bytes!("fixtures/go-swap-v034.zlib");
    let checkpoint: Checkpoint =
        serde_json::from_str(include_str!("fixtures/go-swap-checkpoint.json")).unwrap();
    let open_sync = || Application::open_with_state_sync(&db, 1, checkpoint.clone()).unwrap();
    let offer = |app: &mut Application, format, app_hash, chunk: &[u8]| {
        let mut metadata = vec![0x0a, 32];
        metadata.extend(Sha256::digest(chunk));
        let result = app
            .handle(Request {
                value: Some(request::Value::OfferSnapshot(RequestOfferSnapshot {
                    snapshot: Some(Snapshot {
                        height: checkpoint.height,
                        format,
                        chunks: 1,
                        hash: checkpoint.snapshot_sha256.to_vec().into(),
                        metadata: metadata.into(),
                    }),
                    app_hash,
                })),
            })
            .unwrap();
        let Some(response::Value::OfferSnapshot(reply)) = result.value else {
            panic!("missing OfferSnapshot response");
        };
        reply.result()
    };
    let apply = |app: &mut Application, chunk: &[u8]| {
        let result = app
            .handle(Request {
                value: Some(request::Value::ApplySnapshotChunk(
                    RequestApplySnapshotChunk {
                        index: 0,
                        chunk: chunk.to_vec().into(),
                        sender: "source".into(),
                    },
                )),
            })
            .unwrap();
        let Some(response::Value::ApplySnapshotChunk(reply)) = result.value else {
            panic!("missing ApplySnapshotChunk response");
        };
        reply
    };
    let mut app = open_sync();
    assert_eq!(
        offer(&mut app, 2, checkpoint.app_hash.to_vec().into(), snapshot),
        Offer::RejectFormat
    );
    assert_eq!(
        offer(&mut app, 1, vec![0; 32].into(), snapshot),
        Offer::Reject
    );
    assert_eq!(
        offer(&mut app, 1, checkpoint.app_hash.to_vec().into(), snapshot),
        Offer::Accept
    );
    let mut corrupt = snapshot.to_vec();
    corrupt[0] ^= 1;
    let retry = apply(&mut app, &corrupt);
    assert_eq!(retry.result(), Apply::Retry);
    assert_eq!(retry.refetch_chunks, vec![0]);
    assert_eq!(retry.reject_senders, vec!["source"]);
    assert_eq!(app.head(), None);
    drop(app);
    let mut app = open_sync();
    assert_eq!(app.head(), None);
    assert_eq!(apply(&mut app, snapshot).result(), Apply::Abort);
    assert_eq!(
        offer(&mut app, 1, checkpoint.app_hash.to_vec().into(), &corrupt),
        Offer::Accept
    );
    assert_eq!(apply(&mut app, &corrupt).result(), Apply::RejectSnapshot);
    assert_eq!(app.head(), None);
    assert_eq!(
        offer(&mut app, 1, checkpoint.app_hash.to_vec().into(), snapshot),
        Offer::Accept
    );
    assert_eq!(apply(&mut app, snapshot).result(), Apply::Accept);
    let head = app.head().unwrap();
    assert_eq!(
        (head.height, head.app_hash),
        (checkpoint.height, checkpoint.app_hash)
    );
    assert_eq!(
        offer(&mut app, 1, checkpoint.app_hash.to_vec().into(), snapshot),
        Offer::Reject
    );
    drop(app);
    let mut app = open(&db);
    assert_eq!(app.head(), Some(head));
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/go-swap-v034.json")).unwrap();
    for item in &fixture.exchanges {
        exchange(&mut app, item);
        if item.label.starts_with("commit-") {
            break;
        }
    }
    assert_eq!(
        hex::encode(app.head().unwrap().app_hash),
        fixture.blocks[0].app_hash
    );
}
