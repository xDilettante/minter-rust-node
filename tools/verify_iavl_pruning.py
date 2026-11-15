"""Verify retained roots and resume a legacy archive conversion on a copy."""

import argparse
import json
from pathlib import Path
import platform
import shutil
import subprocess
from time import perf_counter

from verify_send_devnet import require, start, stop, wait_for


def files_size(db):
    return sum(path.stat().st_size for path in db.rglob("*") if path.is_file())


def run(*command, timeout=300):
    result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
    require(result.returncode == 0, result.stderr)
    return json.loads(result.stdout)


def check_window(cli, db, headers, head, first, keep):
    history = run(cli, "iavl", "history", "--db", db)
    require(history == {"earliestHeight": first, "head": head, "keepIavlVersions": keep},
            f"unexpected retention policy or range: {history}")
    roots = run(cli, "iavl", "verify", "--db", db, "--headers", headers)
    require(roots == {"fromHeight": first, "head": head, "roots": head["height"] - first + 1},
            f"unexpected retained root index: {roots}")
    expired = subprocess.run([cli, "iavl", "get", "--db", db, "--height", str(first - 1),
                              "--key", "70"], capture_output=True, text=True, timeout=60)
    require(expired.returncode != 0 and "outside the stored range" in expired.stderr,
            f"historical read accepted discarded height {first - 1}")
    return roots


def staged_index_wal(db):
    marker = b"\x00minter/iavl-indexed-v1"
    for path in db.glob("*.log"):
        with path.open("rb") as wal:
            if marker in wal.read(8 * 1024 * 1024):
                return path.stat().st_size
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("cli", "db", "archive_db", "headers", "replay_result", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    replay = json.loads(args.replay_result.read_bytes())
    head, keep = replay["final"], replay["keepIavlVersions"]
    first = replay["finalWindow"]["first"]
    initial = replay["checkpoint"]["height"]
    require(keep > 0 and first > initial, "need a replay with expired IAVL versions")
    cli = args.cli.resolve(strict=True)
    db, archive = (path.resolve(strict=True) for path in (args.db, args.archive_db))
    require(db != archive and (archive / "CURRENT").is_file(), "need a separate archive RocksDB")
    args.output.mkdir(parents=True)
    copy = args.output / "legacy-rocksdb"

    fresh = check_window(cli, db, args.headers, head, first, keep)
    fresh_before = files_size(db)
    run(cli, "iavl", "retain", "--db", db, "--versions", str(keep), "--compact", timeout=900)
    fresh_bytes = files_size(db)

    shutil.copytree(archive, copy)
    full = run(cli, "iavl", "retain", "--db", copy, "--versions", "0", "--compact", timeout=900)
    require(full == {"earliestHeight": initial, "head": head, "keepIavlVersions": 0},
            f"legacy archive did not start at the expected range: {full}")
    archive_bytes = files_size(copy)

    command = [cli, "iavl", "retain", "--db", copy, "--versions", str(keep), "--compact"]
    log = args.output / "interrupted-conversion.log"
    process = start(command, log)
    try:
        wal_bytes = wait_for(lambda: staged_index_wal(copy), "an IAVL index WAL entry",
                             [(process, log)], timeout=90)
    finally:
        stop(process)
    require(process.returncode != 0, "the conversion ran to completion before interruption")
    before = run(cli, "iavl", "history", "--db", copy)
    require(before == full, f"interrupted indexing exposed a new history floor: {before}")
    checkpoint = run(cli, "iavl", "get", "--db", copy, "--height", str(initial), "--key", "70")
    require(checkpoint["height"] == initial and checkpoint["value"] is not None,
            "interrupted indexing made the checkpoint unreadable")

    started = perf_counter()
    run(*command, timeout=900)
    resumed_seconds = perf_counter() - started
    converted = check_window(cli, copy, args.headers, head, first, keep)
    converted_bytes = files_size(copy)
    result = {
        "os": platform.platform(),
        "fresh": fresh,
        "converted": converted,
        "archiveCompactedBytes": archive_bytes,
        "freshBeforeCompactionBytes": fresh_before,
        "freshCompactedBytes": fresh_bytes,
        "convertedCompactedBytes": converted_bytes,
        "interruptedIndexWalBytes": wal_bytes,
        "historyAfterInterruptedIndex": before,
        "resumedConversionAndCompactionSeconds": resumed_seconds,
    }
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
