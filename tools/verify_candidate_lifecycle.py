"""Replay candidate removal over ABCI and compare current and historical Minter API views."""

import argparse
import base64
import json
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace

from verify_mainnet_empty import app, fields
from verify_minter_api_v2 import listen, verify_queries
from verify_minter_events import case, go_events, missing
from verify_send_devnet import HIDE_WINDOW, allocate_ports, require, stop


def exchange(engine, steps, height):
    for index, step in enumerate(steps):
        [(tag, request)] = fields(base64.b64decode(step["request"], validate=True)).items()
        response = fields(base64.b64decode(step["response"], validate=True))[tag + 1]
        require(engine.call(tag, request) == response, f"ABCI response differs at {height}, call {index}")


def verify(args):
    fixture = next(row for row in json.loads(args.fixture.read_bytes())
                   if row["name"] == "declaration-and-pruning")
    manifest = args.output / "manifest.json"
    snapshot = args.output / "snapshot.zlib"
    db = args.output / "rocksdb"
    manifest.write_text(json.dumps(fixture["manifest"]), encoding="utf-8")
    snapshot.write_bytes(base64.b64decode(fixture["snapshot"], validate=True))
    with (args.output / "import.log").open("wb") as log:
        subprocess.run([args.cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=120)

    go_abci, go_grpc, go_http, rust_grpc, rust_http = allocate_ports(5)
    go_ports = SimpleNamespace(grpc=go_grpc, http=go_http)
    rust_ports = SimpleNamespace(grpc=rust_grpc, http=rust_http)
    flags = ["--grpc-listen", listen(rust_grpc), "--api-listen", listen(rust_http),
             "--keep-iavl-versions", "2"]
    *earlier, removal, later = fixture["blocks"]
    height = removal["height"]
    before_commit, commit = removal["exchanges"][:-1], removal["exchanges"][-1:]
    with go_events(args, args.output, snapshot, manifest, go_abci, go_ports) as go:
        with app(args.daemon, db, args.output / "before-commit.log", *flags) as (rust, process):
            require(rust.info() == go.info(), "checkpoint heads differ")
            for block in earlier:
                for node in (go, rust):
                    exchange(node, block["exchanges"], block["height"])
            prior = go.info()
            exchange(rust, before_commit, height)
            require(rust.info() == prior, "staged removal changed the committed head")
            missing(rust_http, height)
            stop(process)

        with app(args.daemon, db, args.output / "commit.log", *flags) as (rust, process):
            require(rust.info() == prior, "aborted removal advanced the head")
            missing(rust_http, height)
            for node in (go, rust):
                exchange(node, before_commit, height)
                exchange(node, commit, height)
            pruned = go.info()
            require(rust.info() == pruned, "removal heads differ")
            stop(process)

        with app(args.daemon, db, args.output / "after-commit.log", *flags) as (rust, _):
            require(rust.info() == pruned, "committed removal did not survive restart")
            removed = next(event["value"]["candidate_pub_key"] for event in removal["events"]
                           if event["type"] == "minter/RemoveCandidateEvent")
            owner = "Mx" + (94).to_bytes(20, "little").hex()
            current = {"Method": "Address", "Address": owner, "Delegated": True, "Height": 0, "Code": 0}
            historical = {**current, "Height": height}
            compared = verify_queries(args, [case(height), case(height, Search=[removed]),
                                              current, historical], go_ports, rust_ports, "pruning")
            require(compared[2]["rustGrpc"]["body"]["delegated"], "live stake row was lost")
            require(not compared[3]["rustGrpc"]["body"]["delegated"], "IAVL has a removed stake row")
            require(all(event["type"] == "minter/UnbondEvent"
                        for event in compared[1]["rustGrpc"]["body"]["events"]), "unexpected filtered event")
            for node in (go, rust):
                exchange(node, later["exchanges"], later["height"])
            head = go.info()
            require(rust.info() == head, "heads differ after reopening Rust")
    return {"platform": platform.platform(), "removedAt": pruned, "afterRestart": head, "queries": compared}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "oracle", "fixture", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({name: result[name] for name in ("platform", "removedAt", "afterRestart")}))
