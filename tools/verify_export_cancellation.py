"""Cancel snapshot/history exports on an isolated cold mainnet database, then compare bytes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import re
import shutil
import signal
import subprocess
import threading
import time

HIDE_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
TRUSTED = "1537db3bd2ad7e914a1641ff9182fe94135cc4ce9a79cfabd3e3a0cddbbad114"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify(args):
    require(args.seed not in args.output.parents and args.output not in args.seed.parents
            and args.seed != args.output, "output overlaps cold source")
    args.output.mkdir(parents=True)
    require(digest(args.checkpoint / "manifest.json") == TRUSTED, "reference manifest differs")
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    require(manifest["height"] == 40700741 and manifest["historyContext"]["from"] == 40681533,
            "unexpected reference range")
    for name, key in (("state.pb", "consensusStateSha256"), ("blocks.pb", "blocksSha256")):
        require(digest(args.checkpoint / name) == manifest[key], f"reference {name} differs")
    names = [p.name for p in args.seed.iterdir() if p.is_file() and re.fullmatch(
        r"[0-9]+\.(sst|log)|MANIFEST-[0-9]+|OPTIONS-[0-9]+|CURRENT|IDENTITY|LOCK", p.name)]
    require("CURRENT" in names and all(not (args.seed / name).is_symlink() for name in names),
            "expected a closed ordinary RocksDB fixture")
    before = {name: digest(args.seed / name) for name in names}
    database = args.output / "Данные ноды/state"
    database.mkdir(parents=True)
    for name in names:
        shutil.copyfile(args.seed / name, database / name)
    require(before == {name: digest(args.seed / name) for name in names}, "source changed while copying")
    durations = {}

    def command(label, *options, binary=None, error=None):
        began = time.monotonic()
        result = subprocess.run([binary or args.daemon, *options], capture_output=True,
                                timeout=1800, creationflags=HIDE_WINDOW)
        durations[label] = round(time.monotonic() - began, 3)
        (args.output / f"{label}.stdout.log").write_bytes(result.stdout)
        (args.output / f"{label}.stderr.log").write_bytes(result.stderr)
        if error:
            require(result.returncode != 0 and error in result.stderr.decode(errors="replace"),
                    f"{label}: {result.stderr[-2000:]!r}")
            return None
        require(result.returncode == 0, f"{label}: {result.stderr[-2000:]!r}")
        return json.loads(result.stdout)

    def status(label):
        return command(label, "native-history", "status", "--db", database)

    def proof(label):
        return command(label, "iavl", "prove", "--db", database, "--height", "40700741",
                       "--key", "00", binary=args.cli)

    initial = status("initial")
    require(initial["head"] == {"height": manifest["height"], "appHash": manifest["appHash"]}, initial)
    initial_proof = proof("initial-proof")
    observations = []

    def cancel(label, target, options, phase=None, at=None, immediate=False):
        process = None
        began = None
        method = "SIGTERM" if os.name == "posix" and label == "history-records" else "EOF"
        try:
            with (args.output / f"{label}.stdout.log").open("xb") as stdout, \
                 (args.output / f"{label}.stderr.log").open("xb") as stderr:
                process = subprocess.Popen([args.daemon, *options, "--shutdown-on-stdin-close"],
                                           stdin=subprocess.PIPE, stdout=stdout, stderr=subprocess.PIPE,
                                           creationflags=HIDE_WINDOW, start_new_session=os.name == "posix")
                if immediate:
                    began = time.monotonic()
                    process.stdin.close()
                events = queue.Queue()

                def drain():
                    try:
                        for line in iter(process.stderr.readline, b""):
                            events.put(line)
                    finally:
                        events.put(None)

                reader = threading.Thread(target=drain, daemon=True)
                reader.start()
                deadline = (began + 45) if began is not None else time.monotonic() + 1800
                while True:
                    remaining = deadline - time.monotonic()
                    require(remaining > 0, f"{label}: timed out draining the worker")
                    try:
                        line = events.get(timeout=min(remaining, 0.5))
                    except queue.Empty:
                        continue
                    if line is None:
                        break
                    stderr.write(line)
                    stderr.flush()
                    if began is None and line.decode(errors="replace").strip() == f"Export progress: {phase} {at}":
                        began = time.monotonic()
                        if method == "SIGTERM":
                            process.send_signal(signal.SIGTERM)
                        else:
                            process.stdin.close()
                        deadline = began + 45
                require(began is not None, f"{label}: requested progress was never observed")
                require(process.wait(timeout=max(0.1, deadline - time.monotonic())) == 0,
                        f"{label}: exit {process.returncode}")
                reader.join(timeout=1)
                seconds = time.monotonic() - began
            if not process.stdin.closed:
                process.stdin.close()
            process.stderr.close()
            process = None
            text = (args.output / f"{label}.stderr.log").read_text(errors="replace")
            require("Native startup cancelled; database closed" in text and not target.exists(),
                    f"{label}: cancellation did not leave an unpublished destination")
            require(not (args.output / f"{label}.stdout.log").read_bytes().strip(),
                    f"{label}: cancelled export printed success JSON")
            require(status(f"{label}-status") == initial, f"{label}: application/ranges changed")
            require(not list(args.output.glob(".tmp*")), f"{label}: temporary files leaked")
            row = {"phase": phase or "early startup", "method": method, "seconds": round(seconds, 3),
                   "destinationAbsent": True, "stateUnchanged": True}
            observations.append(row)
            print(json.dumps({"cancelled": label, **row}), flush=True)
        finally:
            if process is not None:
                if process.stdin and not process.stdin.closed:
                    process.stdin.close()
                try:
                    process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=15)
                if process.stderr:
                    process.stderr.close()

    def snapshot(target, history=False):
        result = ["native-snapshot", "--db", database, "--output", target,
                  "--anchor-blocks", args.checkpoint / "blocks.pb"]
        return [*result, "--with-history-context"] if history else result

    def history(target):
        return ["native-history", "export", "--db", database, "--anchor", str(manifest["height"]),
                "--from", str(manifest["historyContext"]["from"]), "--output", target]

    for label, phase, count, with_history in (
        ("early", None, None, False),
        ("iavl-nodes", "snapshot-export-nodes", 1000, False),
        ("snapshot-hash", "snapshot-hash-bytes", 10000000, False),
        ("history-scan", "history-export-scan", 1000, True),
        ("history-hash", "history-hash-bytes", 10000000, True),
    ):
        target = args.output / f"cancelled-{label}"
        cancel(label, target, snapshot(target, with_history), phase, count, immediate=label == "early")
    target = args.output / "cancelled-history-records.pb"
    cancel("history-records", target, history(target), "history-export-records", 1000)

    existing = args.output / "existing"
    existing.mkdir()
    (existing / "sentinel").write_bytes(b"preserve this destination\n")
    command("existing-snapshot", *snapshot(existing), error="snapshot destination already exists")
    existing_file = args.output / "existing.pb"
    existing_file.write_bytes(b"preserve this file\n")
    command("existing-history", *history(existing_file), error="history destination already exists")
    require((existing / "sentinel").read_bytes() == b"preserve this destination\n"
            and list(existing.iterdir()) == [existing / "sentinel"]
            and existing_file.read_bytes() == b"preserve this file\n", "existing destination changed")

    package = args.output / "completed-package"
    exported = command("completed-export", *snapshot(package, True))
    actual = json.loads((package / "manifest.json").read_bytes())
    require(actual == manifest, "completed package manifest differs from the trusted previous export")
    expected = {"snapshot.zlib": manifest["snapshotSha256"], "state.pb": manifest["consensusStateSha256"],
                "blocks.pb": manifest["blocksSha256"], "history.pb": manifest["historyContext"]["sha256"]}
    for name, sha in expected.items():
        require(digest(package / name) == sha, f"completed {name} differs")
    require(exported["checkpointSha256"] == digest(package / "manifest.json")
            and exported["snapshotSha256"] == manifest["snapshotSha256"], "success report differs")
    history_output = args.output / "completed-history.pb"
    command("completed-history", *history(history_output))
    require(digest(history_output) == expected["history.pb"], "standalone history bytes differ")
    require(status("final") == initial and proof("final-proof") == initial_proof, "final state/proof changed")
    require(before == {name: digest(args.seed / name) for name in names}, "cold source changed")
    result = {"platform": platform.platform(), "checkpointSha256": TRUSTED, "sourceFiles": before,
              "initial": initial, "cancellations": observations, "completedFiles": expected,
              "durationsSeconds": durations, "proofUnchanged": True, "sourceUnchanged": True,
              "destinationsPreserved": True, "noForcedTermination": True,
              "latePublicationRace": "not scheduled deterministically; publication has no cancellation checks"}
    (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "seed", "checkpoint", "output"):
        parser.add_argument(name, type=lambda path: Path(path).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
