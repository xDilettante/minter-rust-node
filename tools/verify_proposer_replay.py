"""Compare native Tendermint proposer state before and after reopening RocksDB."""

import argparse
import hashlib
import json
from pathlib import Path

from audit_native_replay import receipts
from verify_send_devnet import require


def verify(reference, manifest, results):
    checkpoint = json.loads(manifest.read_bytes())
    require(checkpoint.get("proposerPriorities") is True, "Go checkpoint lacks proposer priorities")
    first, early_last = 10_197_000, 10_197_365
    start, last = checkpoint["height"], 10_523_520
    genesis_root = "c88e583eecef3cb49965bb61fe35450c430cf6ae968fc09850d4ca670503620f"
    go = {}
    height = first - 1
    with reference.open("rb") as stream:
        for height, raw in enumerate(stream, first):
            row = json.loads(raw)
            require(row["height"] == height, f"Go trace is missing {height}")
            if height <= early_last or start <= height <= last:
                go[height] = row
    require(height == last and go[start]["appHash"] == checkpoint["appHash"],
            "Go trace or checkpoint ends at an unexpected state")
    output = {}
    for directory in results:
        runs = [
            ("genesis", first - 1, early_last, genesis_root,
             ("genesis-first.log", "genesis-second.log")),
            ("checkpoint", start, last, checkpoint["appHash"],
             ("checkpoint-first.log", "checkpoint-second.log")),
        ]
        rows = {}
        for name, base, end, root, paths in runs:
            total, updates = 0, 0
            for height, row in enumerate(receipts((directory / name for name in paths), base, root),
                                         base + 1):
                require({key: row[key] for key in go[height]} == go[height],
                        f"Rust and Go differ at {height} in {directory.name}")
                updates += row["validatorUpdates"]
                total += 1
            require(total == end - base, f"{name} checked an incomplete range")
            rows[name] = {"from": base + 1, "through": end, "blocks": total, "updates": updates,
                          "appHash": go[end]["appHash"],
                          "proposerStateSha256": go[end]["proposerStateSha256"]}

        legacy = list(receipts([directory / "checkpoint-legacy.log"], start, checkpoint["appHash"]))
        require(len(legacy) == last - start
                and all(row["appHash"] == go[row["height"]]["appHash"]
                        and "proposerStateSha256" not in row for row in legacy),
                "legacy checkpoint did not preserve observer replay")
        rows["legacyCheckpoint"] = {"blocks": len(legacy), "appHash": legacy[-1]["appHash"]}
        reopened = [json.loads(line) for line in
                    (directory / "legacy-db-reopen.log").read_text().splitlines()]
        require(reopened == [{"through": last, "appHash": go[last]["appHash"], "blocks": 0}],
                "reopening a pre-priority database changed its state")
        exported = json.loads((directory / "reexported/manifest.json").read_bytes())
        require(exported.get("proposerPriorities") is True and exported["height"] == 10_523_506
                and exported["appHash"] == go[exported["height"]]["appHash"],
                "exported Rust checkpoint lost proposer state")
        reimported = list(receipts([directory / "reimport.log"], exported["height"], exported["appHash"]))
        require(len(reimported) == last - exported["height"]
                and all({key: row[key] for key in go[row["height"]]} == go[row["height"]]
                        for row in reimported), "exported Rust checkpoint differs from Go")
        final = list(receipts([directory / "final-roundtrip.log"], exported["height"], exported["appHash"]))
        require(final == reimported, "final build differed when restoring the exported checkpoint")
        rows["reexported"] = {"from": exported["height"], "blocks": len(reimported),
                              "checkpointSha256": hashlib.sha256(
                                  (directory / "reexported/manifest.json").read_bytes()).hexdigest()}
        output[directory.name] = rows
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("go_trace", type=Path)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("results", type=Path, nargs="+")
    args = parser.parse_args()
    print(json.dumps(verify(args.go_trace, args.manifest, args.results), indent=2))
