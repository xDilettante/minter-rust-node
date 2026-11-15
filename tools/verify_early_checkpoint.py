"""Replay a selected range authenticated by the Go genesis verifier."""

import argparse
import gzip
import json
from pathlib import Path
import platform
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("tools", "daemon", "cli", "reference", "checkpoint", "output"):
        parser.add_argument(name, type=lambda path: Path(path).resolve())
    parser.add_argument("--through", type=int, required=True)
    parser.add_argument("--vote-target", type=int, required=True)
    args = parser.parse_args()
    sys.path.insert(0, str(args.tools))
    from verify_mainnet_empty import app, fields, wire_fields
    from verify_minter_api_v2 import get, listen
    from verify_send_devnet import HIDE_WINDOW, allocate_ports, require, stop

    ref = json.loads((args.reference / "result.json").read_bytes())
    manifest = args.checkpoint / "manifest.json"
    snapshot = args.checkpoint / "snapshot.zlib"
    checkpoint = json.loads(manifest.read_bytes())
    start = {key: checkpoint[key] for key in ("height", "appHash")}
    first = start["height"] + 1
    require(ref["signedThrough"] >= args.through + 1 and first <= args.through,
            "selected range has no authenticated successor")
    require(ref["chain"] == checkpoint["network"] == "minter-mainnet-5", "wrong chain")
    if ref["from"] > start["height"]:
        keys = ("format", "network", "height", "appHash", "blockHash", "snapshotSha256", "bytes", "nodes")
        anchor = ref.get("startingCheckpoint", {})
        digest = ref.get("startingCheckpointManifestSha256", "")
        require(ref["from"] == first and ref.get("initialAppHash") == start["appHash"]
                and all(anchor.get(key) == checkpoint[key] for key in keys)
                and isinstance(digest, str) and len(digest) == 64
                and set(digest) <= set("0123456789abcdef"),
                "resumed Go trace differs from the selected checkpoint")
    records = []
    with gzip.open(args.reference / "abci.jsonl.gz", "rt", encoding="utf-8") as source:
        for line in source:
            record = json.loads(line)
            height = record["height"]
            if height == start["height"]:
                require(record["app_hash"] == start["appHash"], "checkpoint differs from verified Go replay")
            if first <= height <= args.through:
                require(height == first + len(records), "gap in verified Go replay")
                records.append(record)
    require(len(records) == args.through - first + 1, "verified Go range is incomplete")

    args.output.mkdir()
    db = args.output / "state"
    with (args.output / "import.log").open("wb") as log:
        subprocess.run([args.cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=300)

    port, = allocate_ports(1)
    flags = ("--api-listen", listen(port))
    roots, tx_codes, validator_changes, reads = [], [], [], {}
    votes_checked = 0

    def versions(height, label):
        expected = [{"name": row["Name"], "height": str(row["Height"])}
                    for row in ref["versions"] if row["Height"] <= height]
        result = get(port, "/v2/version_network")
        require(result == {"status": 200, "body": {"current": expected[-1]["name"], "versions": expected}},
                f"version history differs at {label}: {result}")
        proposals = get(port, f"/v2/update_votes/{args.vote_target}")
        require(proposals["status"] == 200, f"vote query failed at {label}: {proposals}")
        reads[label] = {"height": height, "versionNetwork": result["body"], "updateVotes": proposals["body"]}

    def commit(client, record):
        nonlocal votes_checked
        deliveries, updates = client.stage(record)
        head = client.commit(record)
        roots.append(head)
        if deliveries:
            tx_codes.append({"height": head["height"], "codes": [fields(tx).get(1, 0) for tx in deliveries]})
            for tx in deliveries:
                for tag, event in wire_fields(tx):
                    if tag != 7:
                        continue
                    for tag, attr in wire_fields(event):
                        if tag == 2 and fields(attr).get(1) == b"tx.type" and fields(attr).get(2) == b"21":
                            votes_checked += 1
        if updates:
            validator_changes.append({"height": head["height"], "updates": updates})
        return head

    with app(args.daemon, db, args.output / "staged.log", "--keep-iavl-versions", "2", *flags) as (client, process):
        require(client.info() == start, "wrong Rust checkpoint head")
        versions(start["height"], "checkpoint")
        client.stage(records[0])
        require(client.info() == start, "staged block appeared in Info")
        versions(start["height"], "staged")
        require(reads["staged"] == reads["checkpoint"], "staged votes or versions appeared in the API")
        stop(process)
        require(process.returncode != 0, "process did not stop before Commit")
    with app(args.daemon, db, args.output / "after-abort.log", *flags) as (client, process):
        require(client.info() == start, "restart changed the checkpoint")
        versions(start["height"], "after-abort")
        require(reads["after-abort"] == reads["checkpoint"], "interruption changed committed votes")
        head = commit(client, records[0])
        versions(head["height"], "first-commit")
        stop(process)
        require(process.returncode != 0, "process did not stop after Commit")
    with app(args.daemon, db, args.output / "after-commit.log", *flags) as (client, _):
        require(client.info() == head, "first block did not survive restart")
        versions(head["height"], "after-commit")
        require(reads["first-commit"] == reads["after-commit"], "committed votes or versions were lost")
        for record in records[1:]:
            head = commit(client, record)
        require(client.info() == head, "final head differs from Go")
        versions(head["height"], "final")
        if first <= args.vote_target <= head["height"]:
            current = reads["final"]["versionNetwork"]["current"]
            events = get(port, f"/v2/events/{args.vote_target}")
            event = {"type": "minter/UpdateNetworkEvent", "value": {"version": current}}
            require(events["status"] == 200 and event in events["body"]["events"],
                    f"missing committed upgrade event: {events}")
            reads["activationEvent"] = event

    result = {"platform": platform.platform(), "checkpoint": start, "final": head,
              "roots": roots, "transactionCodes": tx_codes, "validatorUpdates": validator_changes,
              "updateVoteResults": votes_checked, "api": reads}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "platform": result["platform"], "checkpoint": start, "final": head, "blocks": len(roots),
        "transactions": sum(len(row["codes"]) for row in tx_codes), "updateVoteResults": votes_checked,
        "validatorUpdates": sum(len(row["updates"]) for row in validator_changes),
    }))


if __name__ == "__main__":
    main()
