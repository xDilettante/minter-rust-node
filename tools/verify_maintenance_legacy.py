"""Cancel and resume the legacy history backfill in an isolated cold fixture."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time

from verify_send_devnet import HIDE_WINDOW, logs, require, stop, wait_for


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify(args):
    args.output.mkdir(parents=True)
    database = args.output / "state"
    database.mkdir()
    files = [p for p in args.seed.iterdir() if p.is_file() and re.fullmatch(
        r"[0-9]+\.(sst|log)|MANIFEST-[0-9]+|OPTIONS-[0-9]+|CURRENT|IDENTITY|LOCK", p.name)]
    before = {p.name: sha(p) for p in files}
    for p in files:
        require(not p.is_symlink(), "linked fixture input")
        shutil.copyfile(p, database / p.name)
    (args.output / "fixture-intent.txt").write_bytes(b"isolated-cold-maintenance-fixture-v1\n")

    def run(binary, label, *options):
        result = subprocess.run([binary, *options], capture_output=True, timeout=1800, creationflags=HIDE_WINDOW)
        (args.output / f"{label}.stdout.log").write_bytes(result.stdout)
        (args.output / f"{label}.stderr.log").write_bytes(result.stderr)
        require(result.returncode == 0, f"{label}: {result.stderr.decode(errors='replace')[-2000:]}")
        return result.stdout.decode("utf-8")

    def history(label):
        return json.loads(run(args.cli, label, "iavl", "history", "--db", database))

    initial = history("initial")
    require(initial["head"] == {
        "height": 40700741, "appHash": "fbc9b7b8bdfb064de75082b404bc863ccdfe9a78f2365340e957c2957dfdedf1"}, initial)
    run(args.fixture_helper, "prepare-legacy", database)
    empty = json.loads(run(args.fixture_helper, "initial-marker", database, "status"))
    require(not empty["ready"] and not empty["validatorPointsPresent"] and not empty["committedMarksPresent"], empty)
    trusted = sha(args.checkpoint / "manifest.json")
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    require(manifest["height"] == 40559287, manifest)
    command = [args.daemon, "native-history", "maintain", "--db", database,
               "--checkpoint", args.checkpoint, "--checkpoint-sha256", trusted]
    path = args.output / "interrupted.log"
    process = None
    try:
        with path.open("xb") as output:
            process = subprocess.Popen([*command, "--shutdown-on-stdin-close"], stdin=subprocess.PIPE,
                                       stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=os.name == "posix", creationflags=HIDE_WINDOW)
        wait_for(lambda: "Native maintenance: native-index-blocks 1000" in logs(path),
                 "legacy backfill progress", [(process, path)], timeout=600)
        began = time.monotonic()
        process.stdin.close()
        require(process.wait(timeout=45) == 0, logs(path)[-2000:])
        elapsed = time.monotonic() - began
        process = None
        require("Native startup cancelled" in logs(path), "legacy backfill completed before cancellation")
        partial = json.loads(run(args.fixture_helper, "interrupted-marker", database, "status"))
        require(not partial["ready"] and partial["validatorPointsPresent"], "partial backfill did not preserve unready durable points")
        require(history("interrupted-state") == initial, "cancelled backfill changed application state")
        refused = subprocess.run([args.daemon, "native-history", "status", "--db", database],
                                 capture_output=True, timeout=60, creationflags=HIDE_WINDOW)
        (args.output / "partial-without-anchor.log").write_bytes(refused.stdout + refused.stderr)
        require(refused.returncode != 0 and b"upgrade this database once with --checkpoint" in refused.stderr,
                "incomplete native index was exposed without its anchor")
        run(command[0], "resumed", *command[1:])
        require(json.loads(run(args.fixture_helper, "completed-marker", database, "status"))["ready"],
                "completed backfill is not ready")
        require(history("final-state") == initial, "backfill changed application state")
        reopened = run(args.daemon, "reopened", "native-history", "status", "--db", database)
        require("Restored Tendermint" not in reopened, "completed backfill ran again")
        final = json.loads(reopened)
        require(final["head"] == initial["head"] and final["archiveFrom"] == final["contextFrom"] == manifest["height"], final)
        require(before == {p.name: sha(p) for p in files}, "cold source changed")
        result = {"platform": platform.platform(), "checkpointSha256": trusted, "initial": initial,
                  "final": final, "cancelSeconds": round(elapsed, 3), "sourceFiles": before,
                  "partialBackfillNotPublished": True, "durablePartialPoints": True, "resumedReady": True,
                  "reopenedWithoutPackage": True, "blocksReexecuted": 0,
                  "sourceUnchanged": True, "noForcedTermination": True}
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        return result
    finally:
        if process is not None:
            if process.stdin and not process.stdin.closed:
                process.stdin.close()
            stop(process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "fixture_helper", "seed", "checkpoint", "output"):
        parser.add_argument(name, type=lambda p: Path(p).resolve())
    print(json.dumps(verify(parser.parse_args())))
