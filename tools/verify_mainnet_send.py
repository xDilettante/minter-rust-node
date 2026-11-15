"""Run real Minter blocks with BIP Send transactions across a forced restart."""

import argparse
import csv
import json
from pathlib import Path
import platform
import subprocess

from verify_mainnet_empty import app, fields
from verify_mainnet_iavl import check_file, require
from verify_send_devnet import logs, stop


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "fixture", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    root = args.fixture
    manifest, snapshot = root / "manifest.json", root / "snapshot.zlib"
    checkpoint = json.loads(logs(manifest))
    reference = json.loads(logs(root / "reference.json"))
    require(all(checkpoint[k] == v for k, v in reference["checkpoint"].items()),
            "checkpoint differs from Go reference")
    check_file(snapshot, {"bytes": checkpoint["bytes"], "sha256": checkpoint["snapshotSha256"]})
    height = reference["targetHeight"]
    first = json.loads(logs(root / f"block-{height}.abci.json"))
    trace = root / "blocks.abci.jsonl"
    records = [json.loads(line) for line in logs(trace).splitlines()] if trace.exists() else [first]
    with (root / "verified-headers.csv").open(encoding="utf-8", newline="") as stream:
        headers = {int(row["height"]): row for row in csv.DictReader(stream)}
    require(records and records[0] == first and checkpoint["height"] + 1 == height == first["height"]
            and checkpoint["blockHash"] == headers[height - 1]["block_hash"]
            and checkpoint["appHash"] == headers[height]["header_app_hash"]
            and first["app_hash"] == reference["targetAppHash"]
            and len(first["deliver_tx"]) == 1, "fixture differs from verified headers")
    for expected, record in enumerate(records, height):
        require(record["height"] == expected and record["block_hash"] == headers[expected]["block_hash"]
                and record["app_hash"] == headers[expected + 1]["header_app_hash"],
                f"ABCI inputs differ from verified headers at height {expected}")

    args.output.mkdir(parents=True)
    db = args.output / "rocksdb"
    cli, daemon = args.cli.resolve(strict=True), args.daemon.resolve(strict=True)
    imported = subprocess.run(
        [cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot, "--manifest", manifest],
        capture_output=True, text=True, timeout=300,
    )
    (args.output / "import.log").write_text(imported.stdout + imported.stderr, encoding="utf-8")
    require(imported.returncode == 0, f"checkpoint import failed: {imported.stderr}")
    initial = {"height": checkpoint["height"], "appHash": checkpoint["appHash"]}
    with app(daemon, db, args.output / "initial.log") as (client, process):
        require(client.info() == initial, "ABCI did not open at the verified Go checkpoint")
        delivery = client.stage(first)[0]
        require(fields(delivery).get(1, 0) == 0, "BIP Send was rejected")
        require(client.info() == initial, "EndBlock changed committed state")
        stop(process)
        require(process.returncode != 0, "Rust process did not stop at the cut")
    with app(daemon, db, args.output / "resumed.log") as (client, _):
        reopened = client.info()
        require(reopened == initial, "Rust exposed the uncommitted Send")
        require(client.stage(first) == [delivery], "Send result differs after restart")
        roots, transactions = [client.commit(first)], [height]
        for record in records[1:]:
            deliveries = client.stage(record)
            require(all(fields(delivered).get(1, 0) == 0 for delivered in deliveries),
                    f"unexpected transaction rejection at {record['height']}")
            transactions.extend([record["height"]] * len(deliveries))
            roots.append(client.commit(record))
        require(client.info() == roots[-1], "committed ABCI head differs")
    with app(daemon, db, args.output / "final.log") as (client, _):
        require(client.info() == roots[-1], "committed state did not survive reopen")
    result = {"os": platform.platform(), "goOracleRevision": reference["goOracleRevision"],
              "chainId": checkpoint["network"], "checkpoint": initial, "roots": roots,
              "final": roots[-1], "transactionHeights": transactions,
              "commissionPriceCoin": reference["commissionCoin"],
              "commissionInBip": reference["commissionInBip"], "recovered": reopened,
              "deliverTxMatchesGo": True}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"os": result["os"], "blocks": len(roots), "final": roots[-1],
                      "transactionHeights": transactions}))


if __name__ == "__main__":
    main()
