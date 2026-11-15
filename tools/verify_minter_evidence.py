"""Replay injected Tendermint evidence from an authenticated Minter checkpoint."""

import argparse
import base64
from collections import Counter
import json
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace

from verify_mainnet_empty import app, fields, validator_updates
from verify_minter_api_v2 import listen, verify_queries
from verify_minter_events import case, go_events, load_corpus, missing
from verify_send_devnet import HIDE_WINDOW, allocate_ports, require, stop


def verify(args):
    args.output.mkdir(parents=True)
    snapshot, manifest, checkpoint, authentic = load_corpus(args.corpus)
    source = json.loads((args.corpus / "corpus.json").read_bytes())
    reference = json.loads(args.reference.read_bytes())
    require(reference["checkpoint"] == checkpoint and reference["sourceFiles"] == source["files"]
            and reference["goOracle"] == source["go_oracle_revision"], "Go oracle input differs")
    records = reference["blocks"]
    first, *remaining = records
    require(len(remaining) > 0 and first["height"] == checkpoint["height"] + 1,
            "need evidence and a follow-up block")
    require(4 in fields(base64.b64decode(first["begin_block"]["request"], validate=True))
            and 4 not in fields(base64.b64decode(authentic[0]["begin_block"]["request"], validate=True)),
            "expected synthetic evidence at the first height")

    db = args.output / "rocksdb"
    with (args.output / "import.log").open("wb") as log:
        subprocess.run([args.cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=300)

    go_abci, go_grpc, go_http, rust_grpc, rust_http = allocate_ports(5)
    go_ports = SimpleNamespace(grpc=go_grpc, http=go_http)
    rust_ports = SimpleNamespace(grpc=rust_grpc, http=rust_http)
    flags = ["--grpc-listen", listen(rust_grpc), "--api-listen", listen(rust_http),
             "--keep-iavl-versions", str(args.keep_iavl_versions)]
    initial = {"height": checkpoint["height"], "appHash": checkpoint["appHash"]}
    height = first["height"]
    last = records[-1]["height"]
    roots = []
    with go_events(args, args.output, snapshot, manifest, go_abci, go_ports) as go:
        require(go.info() == initial, "Go did not open at the checkpoint")
        with app(args.daemon, db, args.output / "before-commit.log", *flags) as (rust, process):
            require(rust.info() == initial, "Rust did not open at the checkpoint")
            rust.stage(first)
            require(rust.info() == initial, "staged evidence changed committed state")
            missing(rust_http, height)
            stop(process)

        with app(args.daemon, db, args.output / "first-commit.log", *flags) as (rust, process):
            require(rust.info() == initial, "the aborted penalty advanced the height")
            missing(rust_http, height)
            go.stage(first)
            rust.stage(first)
            head = rust.commit(first)
            require(go.commit(first) == head, "first AppHash differs from Go")
            roots.append(head)
            stop(process)

        with app(args.daemon, db, args.output / "after-commit.log", *flags) as (rust, _):
            require(rust.info() == head, "the committed penalty did not survive restart")
            for record in remaining:
                go.stage(record)
                rust.stage(record)
                head = rust.commit(record)
                require(go.commit(record) == head, "follow-up AppHash differs from Go")
                roots.append(head)

            expected = reference["events"][0]
            penalty = [event["value"] for event in expected if event["type"] == "minter/SlashEvent"]
            unbond = next(event["value"] for event in expected if event["type"] == "minter/UnbondEvent")
            coin_ids = sorted({int(event["coin"]) for event in penalty})
            cases = [case(checkpoint["height"], 5), *(case(record["height"]) for record in records),
                     case(height, Search=["missing", reference["validator"]]),
                     case(height, Search=[unbond["address"]]), case(last + 1, 5)]
            cases += [{"Method": "CoinInfoById", "ID": coin, "Height": height, "Code": 0}
                      for coin in coin_ids if coin != 0]
            cases.append({"Method": "Address", "Address": unbond["address"], "Height": height, "Code": 0})
            compared = verify_queries(args, cases, go_ports, rust_ports, "evidence")
            actual = [row["rustGrpc"]["body"]["events"] for row in compared[1:1 + len(records)]]
            require(actual == reference["events"], "Rust event sequence differs from the recorded Go result")
            require(rust.iavl_key(b"p", height).get(8), "no retained proof for the slashing height")
            if args.keep_iavl_versions == 2 and len(records) == 2:
                require(rust.iavl_key(b"p", checkpoint["height"]).get(1) == 1,
                        "checkpoint version should have expired")

    updates = validator_updates(base64.b64decode(first["end_block"]["response"], validate=True))
    require(any(row["key"] == reference["validator"][2:] and row["power"] == 0 for row in updates),
            "the punished validator was not removed")
    return {"platform": platform.platform(), "checkpoint": initial, "roots": roots,
            "eventCounts": [dict(Counter(event["type"] for event in batch)) for batch in actual],
            "penaltiesByCoin": dict(Counter(event["coin"] for event in penalty)),
            "validatorUpdates": updates, "comparisons": compared}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "oracle", "corpus", "reference", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--keep-iavl-versions", type=int, choices=(0, 2), default=2)
    args = parser.parse_args()
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: result[k] for k in ("platform", "roots", "eventCounts", "penaltiesByCoin")}))
