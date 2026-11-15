"""Check a Rust import against a Go readout and a verified Minter header index."""

import argparse
import csv
import gzip
import json
import platform
import subprocess
import time
from pathlib import Path


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def wait_for_staged_wal(process, db):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and process.poll() is None:
        for wal in db.glob("*.log"):
            try:
                size = wal.stat().st_size
            except FileNotFoundError:
                continue
            if size >= 128 * 1024:
                return size
        time.sleep(0.025)
    raise RuntimeError("Did not observe an active import with a staged IAVL batch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "snapshot", "manifest", "go_reference", "header_index", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    reference = json.loads(args.go_reference.read_text(encoding="utf-8"))
    height, app_hash = manifest["height"], manifest["appHash"]
    require(reference["height"] == height and reference["appHash"] == app_hash,
            "Go import differs from the trusted checkpoint")
    with gzip.open(args.header_index, "rt", encoding="utf-8", newline="") as stream:
        headers = csv.DictReader(stream)
        checkpoint = next((row for row in headers if int(row["height"]) == height), None)
        next_header = next(headers, None)
    require(checkpoint and checkpoint["block_hash"] == manifest["blockHash"],
            "Checkpoint block differs from the verified header index")
    require(next_header and int(next_header["height"]) == height + 1
            and next_header["header_app_hash"] == app_hash,
            "AppHash differs from the next verified header")

    args.output.mkdir(parents=True)
    db = args.output / "rocksdb"
    command = [str(args.binary.resolve(strict=True)), "checkpoint"]
    importer = command + ["import", "--db", str(db), "--snapshot", str(args.snapshot),
                          "--manifest", str(args.manifest)]
    status = command + ["status", "--db", str(db)]
    with (args.output / "interrupted-import.log").open("wb") as log:
        child = subprocess.Popen(importer, stdout=log, stderr=subprocess.STDOUT)
        try:
            wal_bytes = wait_for_staged_wal(child, db)
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=15)
    require(child.returncode != 0, "The import finished before the interruption")
    unfinished = subprocess.run(status, capture_output=True, text=True, timeout=15)
    require(unfinished.returncode != 0 and "no committed checkpoint" in unfinished.stderr,
            f"The interrupted state was not rejected: {unfinished.stderr}")

    completed = subprocess.run(importer, check=True, capture_output=True, text=True, timeout=300)
    (args.output / "restarted-import.log").write_text(completed.stdout + completed.stderr, encoding="utf-8")
    committed = json.loads(completed.stdout)
    reopened = json.loads(subprocess.check_output(status, text=True, timeout=15))
    require(reopened == committed and all(reopened[key] == manifest[key] for key in (
        "format", "network", "height", "appHash", "blockHash", "snapshotSha256", "bytes", "nodes",
    )), "Reopened state differs from the trusted checkpoint")

    for item in reference["values"]:
        actual = json.loads(subprocess.check_output(
            command + ["get", "--db", str(db), "--key", item["key"]], text=True, timeout=15,
        ))
        require(actual == {"height": height, **item}, f"Go and Rust differ at {item['key']}")
    result = {
        "os": platform.platform(),
        "network": manifest["network"], "height": height,
        "nodes": manifest["nodes"], "appHash": app_hash,
        "verified_header_height": height + 1,
        "snapshotSha256": manifest["snapshotSha256"],
        "matching_go_reads": len(reference["values"]),
        "interrupted_wal_bytes": wal_bytes,
        "interrupted_exit_code": child.returncode,
        "interrupted_open_rejected": True,
        "same_directory_retry_succeeded": True,
    }
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
