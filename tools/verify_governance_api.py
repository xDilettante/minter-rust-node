"""Compare Minter v2 governance reads while replaying the Go fixtures."""

import argparse
import base64
import json
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace

from verify_mainnet_empty import app
from verify_minter_api_v2 import listen, verify_queries
from verify_minter_events import go_events
from verify_send_devnet import HIDE_WINDOW, allocate_ports, require, stop
from verify_update_votes import exchange


def price(height=0, code=0):
    return {"Method": "PriceCommission", "Height": height, "Code": code}


def votes(method, target, height=0):
    return {"Method": method, "TargetVersion": target, "Height": height, "Code": 0}


def halts(height):
    return {"Method": "Halts", "Height": height, "Code": 0}


def events(height, code=0):
    return {"Method": "Events", "Height": height, "Code": code}


CASES = {
    "commission": {
        717: [price(), price(717), price(716, 5), votes("CommissionVotes", 718),
              votes("CommissionVotes", 719)],
        718: [price(), price(717), votes("CommissionVotes", 718),
              votes("CommissionVotes", 718, 717), events(718)],
        719: [price(), votes("CommissionVotes", 719), votes("CommissionVotes", 719, 718)],
        720: [price(), price(719), votes("CommissionVotes", 720), events(720),
              {"Method": "VersionNetwork", "Code": 0}],
        721: [price(), price(720), events(721)],
    },
    "halt": {
        717: [halts(718), halts(719), halts(722)],
        718: [halts(718), halts(719)],
        719: [halts(719)],
        720: [halts(720), halts(722)],
        721: [halts(722)],
    },
    "update": {
        717: [votes("UpdateVotes", 718), votes("UpdateVotes", 722)],
        718: [votes("UpdateVotes", 718), votes("UpdateVotes", 718, 717)],
        719: [votes("UpdateVotes", 723)],
        720: [votes("UpdateVotes", 720), votes("UpdateVotes", 723, 719)],
        722: [votes("UpdateVotes", 722)],
        723: [votes("UpdateVotes", 723)],
    },
}


def verify(args, mode, output):
    fixture = args.fixtures / f"go-{mode}-votes-v034.json"
    data = json.loads(fixture.read_bytes())
    checkpoint = data["pending"]["manifest"]
    output.mkdir()
    manifest, snapshot, db = (output / name for name in ("manifest.json", "snapshot.zlib", "rocksdb"))
    manifest.write_text(json.dumps(checkpoint), encoding="utf-8")
    snapshot.write_bytes(base64.b64decode(data["pending"]["snapshot"], validate=True))
    with (output / "import.log").open("wb") as log:
        subprocess.run([args.cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=120)
    go_abci, go_http, go_grpc, rust_http, rust_grpc = allocate_ports(5)
    go_ports = SimpleNamespace(http=go_http, grpc=go_grpc)
    rust_ports = SimpleNamespace(http=rust_http, grpc=rust_grpc)
    rust_flags = ("--api-listen", listen(rust_http), "--grpc-listen", listen(rust_grpc))
    query_args = SimpleNamespace(oracle=args.oracle, output=output)
    first = checkpoint["height"]
    blocks = [block for block in data["blocks"] if block["height"] > first]
    groups = {}

    def compare(cases, label):
        groups[label] = verify_queries(query_args, cases, go_ports, rust_ports, label)

    def replay(go, rust, block):
        height = block["height"]
        steps = block["exchanges"]
        exchange(go, [step for step in steps if not step["label"].startswith("CheckTx")], height)
        exchange(rust, steps, height)
        require(rust.info() == {"height": height, "appHash": block["appHash"]},
                f"Rust committed a different AppHash at {height}")
        if height in CASES[mode]:
            compare(CASES[mode][height], f"height-{height}")

    cut = next((index for index, block in enumerate(blocks) if block["height"] == 720),
               len(blocks)) if mode == "commission" else len(blocks)
    with go_events(args, output, snapshot, manifest, go_abci, go_ports) as go:
        with app(args.daemon, db, output / "rust.log", *rust_flags) as (rust, process):
            require(rust.info() == {"height": first, "appHash": checkpoint["appHash"]},
                    "Rust opened a different Go checkpoint")
            compare(CASES[mode][first], f"height-{first}")
            for block in blocks[:cut]:
                replay(go, rust, block)
            if cut < len(blocks):
                height = blocks[cut]["height"]
                before = rust.info()
                exchange(rust, blocks[cut]["exchanges"][:-1], height)
                require(rust.info() == before, "EndBlock published the fee schedule")
                pending = [price(), votes("CommissionVotes", height), events(height, 5)]
                compare(pending, "before-commit")
                stop(process)
                require(process.returncode != 0, "process did not stop before Commit")

        if cut < len(blocks):
            with app(args.daemon, db, output / "rust-after-abort.log", *rust_flags) as (rust, process):
                require(rust.info() == before, "restart published the uncommitted fee schedule")
                compare(pending, "after-abort")
                replay(go, rust, blocks[cut])
                committed = rust.info()
                stop(process)
                require(process.returncode != 0, "process did not stop after Commit")
            with app(args.daemon, db, output / "rust-after-commit.log", *rust_flags) as (rust, _):
                require(rust.info() == committed, "restart lost the committed fee schedule")
                compare(CASES[mode][height], "after-commit")
                for block in blocks[cut + 1:]:
                    replay(go, rust, block)

    result = {
        "checkpoint": first,
        "abcis": sum(len(block["exchanges"]) for block in blocks),
        "final": {"height": blocks[-1]["height"], "appHash": blocks[-1]["appHash"]},
        "queries": groups,
    }
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return {"final": result["final"], "abcis": result["abcis"],
            "queries": sum(len(cases) for cases in groups.values())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("daemon", "cli", "oracle", "fixtures", "output"):
        parser.add_argument(argument, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir()
    result = {"platform": platform.platform(), "cases": {
        mode: verify(args, mode, args.output / mode) for mode in CASES
    }}
    encoded = json.dumps(result, indent=2) + "\n"
    (args.output / "result.json").write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
