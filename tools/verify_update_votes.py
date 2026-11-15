"""Check Go update votes, recovery and Minter events over the minterd socket."""

import argparse
import base64
import json
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace

from verify_mainnet_empty import app, fields
from verify_minter_api_v2 import get, grpc, listen
from verify_minter_events import go_events
from verify_send_devnet import HIDE_WINDOW, allocate_ports, require, stop


def exchange(client, steps, height):
    for step in steps:
        [(tag, payload)] = fields(base64.b64decode(step["request"], validate=True)).items()
        expected = fields(base64.b64decode(step["response"], validate=True))[tag + 1]
        require(client.call(tag, payload) == expected, f"{step['label']} differs at {height}")


def check_events(port, block):
    result = get(port, f"/v2/events/{block['height']}")
    require(result == {"status": 200, "body": {"events": block["events"] or []}},
            f"Go events differ at {block['height']}: {result}")


def require_unpublished(port, height):
    result = get(port, f"/v2/events/{height}")
    require(result["status"] == 404 and result["body"]["error"]["message"] == f"version {height} doesn't exist yet",
            f"uncommitted events are visible at {height}: {result}")


def check_versions(ports, block, oracle, output, label):
    versions = block["versions"]
    expected = {
        "current": versions[-1]["name"],
        "versions": [{"name": row["name"], "height": str(row["height"])} for row in versions],
    }
    actual = get(ports.http, "/v2/version_network")
    require(actual == {"status": 200, "body": expected}, f"HTTP version history differs at {label}: {actual}")
    if oracle is not None:
        query = [{"Method": "VersionNetwork", "Code": 0}]
        actual = grpc(oracle, ports.grpc, query, output / f"versions-{label}.json")
        require(actual == [{"code": 0, "body": expected}], f"gRPC version history differs at {label}: {actual}")


def replay(client, port, records):
    for block in records:
        exchange(client, block["exchanges"], block["height"])
        check_events(port, block)
        require(client.info() == {"height": block["height"], "appHash": block["appHash"]},
                f"committed head differs at {block['height']}")


def verify(daemon, cli, fixture, output, oracle):
    data = json.loads(fixture.read_bytes())
    reference = data["pending"]
    checkpoint = reference["manifest"]
    output.mkdir()
    manifest, snapshot, db = (output / name for name in ("manifest.json", "snapshot.zlib", "rocksdb"))
    manifest.write_text(json.dumps(checkpoint), encoding="utf-8")
    snapshot.write_bytes(base64.b64decode(reference["snapshot"], validate=True))
    with (output / "import.log").open("wb") as log:
        subprocess.run([cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=120)

    saved = next(block for block in data["blocks"] if block["height"] == checkpoint["height"])
    records = [block for block in data["blocks"] if block["height"] > checkpoint["height"]]
    cut = next(index for index, block in enumerate(records) if block["height"] == 720)
    upgrade = records[cut]
    height = upgrade["height"]
    port, grpc_port = allocate_ports(2)
    rust = SimpleNamespace(http=port, grpc=grpc_port)
    flags = ("--api-listen", listen(port), "--grpc-listen", listen(grpc_port))
    with app(daemon, db, output / "before-commit.log", *flags) as (client, process):
        require(client.info() == {key: checkpoint[key] for key in ("height", "appHash")}, "checkpoint head differs")
        check_versions(rust, saved, oracle, output, "rust-checkpoint")
        replay(client, port, records[:cut])
        prior = client.info()
        exchange(client, upgrade["exchanges"][:-1], height)
        require(client.info() == prior, "EndBlock published a head")
        require_unpublished(port, height)
        check_versions(rust, records[cut - 1], oracle, output, "rust-staged")
        stop(process)

    with app(daemon, db, output / "after-abort.log", *flags) as (client, process):
        require(client.info() == prior, "interrupted vote changed RocksDB")
        require_unpublished(port, height)
        check_versions(rust, records[cut - 1], oracle, output, "rust-recovered-before")
        replay(client, port, [upgrade])
        published = client.info()
        stop(process)

    with app(daemon, db, output / "after-commit.log", *flags) as (client, _):
        require(client.info() == published, "committed vote was lost")
        check_events(port, upgrade)
        check_versions(rust, upgrade, oracle, output, "rust-recovered-after")
        replay(client, port, records[cut + 1:])
        final = client.info()
        [(tag, blocked)] = fields(base64.b64decode(data["blocked"], validate=True)).items()
        try:
            client.call(tag, blocked)
        except RuntimeError as error:
            require(str(error) == "network upgrade v999_local requires a compatible node at height 725",
                    f"unexpected response to unknown update: {error}")
        else:
            raise RuntimeError("minterd executed a block requiring an unknown update")
        require_unpublished(port, final["height"] + 1)

    with app(daemon, db, output / "after-unknown.log", *flags) as (client, _):
        require(client.info() == final, "unknown update altered the last committed head")
        check_versions(rust, records[-1], oracle, output, "rust-unknown")
        for block in records:
            check_events(port, block)
    if oracle is not None:
        go_abci, go_http, go_grpc = allocate_ports(3)
        go_ports = SimpleNamespace(http=go_http, grpc=go_grpc)
        with go_events(SimpleNamespace(oracle=oracle), output, snapshot, manifest, go_abci, go_ports) as go:
            check_versions(go_ports, saved, oracle, output, "go-checkpoint")
            for block in records:
                deliveries = [step for step in block["exchanges"] if not step["label"].startswith("CheckTx")]
                exchange(go, deliveries, block["height"])
                check_versions(go_ports, block, oracle, output, f"go-{block['height']}")
    return {
        "platform": platform.platform(), "checkpoint": checkpoint["height"], "final": final,
        "abcis": sum(len(block["exchanges"]) for block in records),
        "cutBeforeAndAfterCommit": height,
        "unknownUpgradeRejectedAt": final["height"] + 1,
        "versionNetworkHttpChecks": 5,
        "comparedGoVersionNetworkOverHttpAndGrpc": oracle is not None,
        "networkUpdates": [
            {"height": block["height"], **event["value"]}
            for block in records for event in block["events"] or []
            if event["type"] == "minter/UpdateNetworkEvent"
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("daemon", "cli", "fixture", "output"):
        parser.add_argument(argument, type=Path)
    parser.add_argument("--oracle", type=Path, help="Go Minter v2 oracle for the gRPC and HTTP comparison")
    args = parser.parse_args()
    oracle = None if args.oracle is None else args.oracle.resolve()
    result = verify(args.daemon.resolve(), args.cli.resolve(), args.fixture.resolve(), args.output.resolve(), oracle)
    encoded = json.dumps(result, indent=2) + "\n"
    (args.output / "result.json").write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
