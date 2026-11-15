"""Replay signed blocks from a Minter genesis or archive database."""

import argparse
import gzip
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("tools", "daemon", "cli", "starting_db", "reference", "output"):
        parser.add_argument(name, type=lambda path: Path(path).resolve())
    parser.add_argument("--through", type=int, help="last block to replay, defaults to the first block")
    parser.add_argument("--interrupt-before", type=int, help="stop after EndBlock at this height")
    parser.add_argument("--interrupt-after-commit", type=int, help="stop after Commit at this height")
    args = parser.parse_args()
    sys.path.insert(0, str(args.tools))
    from verify_mainnet_empty import app, fields
    from verify_send_devnet import HIDE_WINDOW, require, stop

    ref = json.loads((args.reference / "result.json").read_bytes())
    first = ref["from"]
    db = args.output / "state"
    genesis = {"height": first - 1, "appHash": ref["initialAppHash"]}
    roots = []
    transactions = validator_changes = 0

    def read_history(database):
        return json.loads(subprocess.check_output(
            [args.cli, "iavl", "history", "--db", database], creationflags=HIDE_WINDOW, timeout=20))

    def check_head(client, expected):
        if expected != genesis:
            require(client.info() == expected, f"unexpected committed head at {expected['height']}")
            return
        info = fields(client.call(3))
        require(info.get(4, 0) == 0, "uncommitted first block appeared in Info")
        proof = client.iavl_key(b"p", genesis["height"])
        require(proof.get(1, 0) == 0 and proof[9] == genesis["height"] and proof[7] and proof[8],
                "genesis proof unavailable")

    def stage(client, record):
        try:
            return client.stage(record)
        except RuntimeError as error:
            raise RuntimeError(f"block {record['height']}: {error}") from error

    def advance(client, record):
        nonlocal transactions, validator_changes
        deliveries, updates = stage(client, record)
        roots.append(client.commit(record))
        transactions += len(deliveries)
        validator_changes += len(updates)
        if record["height"] % 1000 == 0 or record["height"] == last:
            print(f"Verified through {record['height']}", flush=True)

    history = read_history(args.starting_db)
    initial = history["head"]
    start = initial["height"] + 1
    last = args.through if args.through is not None else start
    before = args.interrupt_before if args.interrupt_before is not None else start
    after = args.interrupt_after_commit if args.interrupt_after_commit is not None else before
    require(first <= start <= before <= after <= last < ref["signedThrough"] and last <= ref["through"],
            "interruption or replay range is outside the verified Go trace")
    require(history["earliestHeight"] == genesis["height"] and history["keepIavlVersions"] == 0,
            "starting database does not keep the archive from genesis")
    require(not args.output.is_relative_to(args.starting_db),
            "output must be outside the starting database")

    with gzip.open(args.reference / "abci.jsonl.gz", "rt", encoding="utf-8") as source:
        def read_record(height):
            line = source.readline()
            require(bool(line), f"Go trace ended before {height}")
            record = json.loads(line)
            require(record["height"] == height, f"Go trace is missing block {height}")
            return record

        def replay(client, start, end):
            for height in range(start, end + 1):
                advance(client, read_record(height))

        expected_root = genesis["appHash"]
        for height in range(first, start):
            expected_root = read_record(height)["app_hash"]
        require(initial["appHash"] == expected_root, "starting database differs from signed Go trace")
        args.output.mkdir()
        shutil.copytree(args.starting_db, db)

        with app(args.daemon, db, args.output / "staged.log") as (client, process):
            check_head(client, initial)
            replay(client, start, before - 1)
            prior = roots[-1] if roots else initial
            interrupted = read_record(before)
            stage(client, interrupted)
            check_head(client, prior)
            stop(process)
            require(process.returncode != 0, "process did not stop before Commit")

        require(read_history(db)["head"] == prior, "pre-Commit interruption changed the archive")
        with app(args.daemon, db, args.output / "recovered.log") as (client, process):
            check_head(client, prior)
            advance(client, interrupted)
            replay(client, before + 1, after)
            committed = roots[-1]
            stop(process)
            require(process.returncode != 0, "process did not stop after Commit")
        with app(args.daemon, db, args.output / "committed.log") as (client, _):
            check_head(client, committed)
            replay(client, after + 1, last)
            check_head(client, roots[-1])
            for height in {genesis["height"], first, initial["height"], last}:
                proof = client.iavl_key(b"p", height)
                require(proof.get(1, 0) == 0 and proof[9] == height and proof[7] and proof[8],
                        f"archive proof unavailable at {height}")
        if last == ref["through"]:
            require(not source.readline() and roots[-1]["appHash"] == ref["appHash"],
                    "final block differs from the signed Go summary")
    history = read_history(db)
    require(history["head"] == roots[-1] and history["earliestHeight"] == genesis["height"]
            and history["keepIavlVersions"] == 0, "archive window differs after replay")
    result = {"platform": platform.platform(), "initial": initial, "final": roots[-1],
              "transactions": transactions, "validatorUpdates": validator_changes,
              "roots": roots, "cutBeforeCommitAt": before, "cutAfterCommitAt": after,
              "recoveredBefore": prior, "recoveredAfter": committed, "archive": history}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({**result, "roots": len(roots)}))


if __name__ == "__main__":
    main()
