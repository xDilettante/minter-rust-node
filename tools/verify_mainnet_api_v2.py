"""Compare Minter v2 state queries at an authenticated mainnet Go checkpoint."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace

from verify_mainnet_empty import app, fields, load_inputs
from verify_mainnet_iavl import check_file
from verify_minter_api_v2 import get, listen, verify_queries
from verify_send_devnet import HIDE_WINDOW, allocate_ports, require, start, stop, wait_for


def history(corpus_dir, checkpoint):
    trace = corpus_dir / "abci-iavl.jsonl.gz"
    headers = corpus_dir / "blocks.csv.gz"
    manifest = corpus_dir / "checkpoint.json"
    corpus = json.loads((corpus_dir / "corpus.json").read_bytes())
    for path, name in ((trace, trace.name), (headers, headers.name),
                       (manifest, "go_checkpoint/manifest.json")):
        check_file(path, corpus["files"][name])
    base = json.loads(manifest.read_bytes())
    blocks, verified = load_inputs(trace, headers, base, checkpoint["height"])
    require(blocks[-1]["app_hash"] == checkpoint["appHash"] and
            verified[checkpoint["height"]]["block_hash"] == checkpoint["blockHash"],
            "API checkpoint differs from the verified history")
    transactions = {}
    for block in blocks:
        for index, pair in enumerate(block["deliver_tx"] or ()):
            raw = fields(base64.b64decode(pair["request"], validate=True))[1]
            result = fields(base64.b64decode(pair["response"], validate=True))
            tx_hash = "Mt" + hashlib.sha256(raw).hexdigest()
            transactions[tx_hash] = {"height": str(block["height"]), "index": str(index),
                                     "raw_tx": raw.hex(), "code": str(result.get(1, 0))}
    return trace, base["height"], transactions


def verify(args):
    args.output.mkdir(parents=True)
    snapshot, manifest = args.corpus / "snapshot.zlib", args.corpus / "checkpoint.json"
    corpus = json.loads((args.corpus / "corpus.json").read_bytes())
    check_file(snapshot, corpus["files"]["go_checkpoint/snapshot.zlib"])
    check_file(manifest, corpus["files"]["go_checkpoint/manifest.json"])
    checkpoint = json.loads(manifest.read_bytes())
    require(checkpoint["network"] == "minter-mainnet-5", "unexpected checkpoint network")
    history_args, history_flags, transactions = [], [], {}
    if args.history_from:
        trace, base, transactions = history(args.history_from, checkpoint)
        history_rpc, = allocate_ports(1)
        history_args = [trace, listen(history_rpc)]
        history_flags = ["--tendermint-rpc", listen(history_rpc)]
    db = args.output / "rocksdb"
    imported = subprocess.run([args.cli, "checkpoint", "import", "--db", db,
                               "--snapshot", snapshot, "--manifest", manifest], capture_output=True,
                              text=True, timeout=300, creationflags=HIDE_WINDOW)
    (args.output / "import.log").write_text(imported.stdout + imported.stderr, encoding="utf-8")
    require(imported.returncode == 0, imported.stderr)
    rust_http, rust_grpc, go_http, go_grpc = allocate_ports(4)
    rust = SimpleNamespace(http=rust_http, grpc=rust_grpc)
    go = SimpleNamespace(http=go_http, grpc=go_grpc)
    sample_path = args.output / "sample.json"
    log = args.output / "go.log"
    oracle = start([args.oracle, "checkpoint", args.output / "go", snapshot, manifest,
                    listen(go_grpc), listen(go_http), sample_path, *history_args], log)
    try:
        wait_for(lambda: get(go_http, "/v2/coin_info/BIP")["status"] == 200,
                 "Go mainnet snapshot queries", [(oracle, log)], timeout=120)
        sample = json.loads(sample_path.read_bytes())
        height = checkpoint["height"]
        head = {"height": height, "appHash": checkpoint["appHash"]}
        require(sample["height"] == height and sample["appHash"] == head["appHash"], sample)
        cases = [
            {"Method": "Address", "Code": 0, "Address": sample["delegator"], "Height": height, "Delegated": True},
            {"Method": "Address", "Code": 0, "Address": sample["owner"], "Delegated": True},
            {"Method": "Address", "Code": 0, "Address": sample["delegator"]},
            {"Method": "CoinInfo", "Code": 0, "Symbol": sample["symbol"], "Height": height},
            {"Method": "CoinInfoById", "Code": 0, "ID": sample["coinId"]},
            {"Method": "CoinInfoById", "Code": 0, "ID": 3753, "Height": height},
            {"Method": "CoinInfoById", "Code": 0, "ID": 0},
            {"Method": "CoinInfoById", "Code": 5, "ID": 999999999},
            {"Method": "Address", "Code": 5, "Address": sample["owner"], "Height": height - 1},
            {"Method": "VersionNetwork", "Code": 0},
            {"Method": "PriceCommission", "Code": 0},
            {"Method": "PriceCommission", "Code": 0, "Height": height},
            {"Method": "PriceCommission", "Code": 5, "Height": height - 1},
            {"Method": "CommissionVotes", "Code": 0, "TargetVersion": height + 1},
            {"Method": "UpdateVotes", "Code": 0, "TargetVersion": height + 1, "Height": height},
            {"Method": "Halts", "Code": 0, "Height": height + 1},
        ]
        with app(args.daemon.resolve(), db, args.output / "rust.log",
                 "--api-listen", listen(rust_http), "--grpc-listen", listen(rust_grpc),
                 *history_flags) as (client, _):
            require(client.info() == head, "Rust opened a different checkpoint")
            queries = verify_queries(args, cases, go, rust, "mainnet")
            tx_results = verify_queries(args, [
                {"Method": "Transaction", "Hash": tx_hash, "Code": 0} for tx_hash in transactions
            ], go, rust, "mainnet-transactions") if transactions else []
        delegates = queries[0]["rustGrpc"]["body"]["delegated"]
        require(any(int(row["coin"]["id"]) == sample["coinId"] and int(row["bip_value"]) > 0
                    for row in delegates), "no reserve-coin stake with a BIP valuation")
        result = {"os": platform.platform(), "checkpoint": head, "sample": sample, "queries": queries}
        if transactions:
            for answer in tx_results:
                tx = answer["rustGrpc"]["body"]
                expected = transactions[answer["query"]["Hash"]]
                require({key: tx[key] for key in expected} == expected, tx)
            result["transactionHistory"] = {
                "from": base + 1, "through": height,
                "types": sorted({answer["rustGrpc"]["body"]["type_hex"] for answer in tx_results}),
                "queries": tx_results,
            }
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"os": result["os"], "checkpoint": head, "queries": len(queries),
                          "transactions": len(tx_results),
                          "reserveCoin": sample["coinId"]}))
    finally:
        stop(oracle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "oracle", "corpus", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--history-from", type=Path, help="verified corpus containing blocks before the checkpoint")
    verify(parser.parse_args())
