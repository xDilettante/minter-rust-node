"""Verify the current cancellable genesis core on the official mainnet file."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import signal
import subprocess
import threading
import time

HIDE_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
GENESIS_SHA = "906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5"
HEAD = {"height": 10196999, "appHash": "c88e583eecef3cb49965bb61fe35450c430cf6ae968fc09850d4ca670503620f"}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify(args):
    args.output.mkdir(parents=True)
    require(digest(args.genesis) == GENESIS_SHA and args.genesis.stat().st_size == 127581025,
            "official genesis differs")
    observations = []

    def command(label, binary, options, error=None):
        completed = subprocess.run([binary, *options], capture_output=True, timeout=120,
                                   creationflags=HIDE_WINDOW)
        (args.output / f"{label}.stdout.log").write_bytes(completed.stdout)
        (args.output / f"{label}.stderr.log").write_bytes(completed.stderr)
        if error:
            require(completed.returncode != 0 and error in completed.stderr.decode(errors="replace"),
                    f"{label}: {completed.stderr[-2000:]!r}")
            return None
        require(completed.returncode == 0, f"{label}: {completed.stderr[-2000:]!r}")
        return json.loads(completed.stdout)

    def empty(label, database):
        command(label, args.cli, ["iavl", "history", "--db", database],
                error="database has no committed Minter state")

    def launch(label, database, requested=None, with_genesis=True):
        database.parent.mkdir(parents=True, exist_ok=True)
        options = ["native-sync", "--db", database, "--p2p-listen", "127.0.0.1:0",
                   "--shutdown-on-stdin-close"]
        if with_genesis:
            options.extend(["--genesis", args.genesis])
        method = "SIGTERM" if os.name == "posix" and requested and requested[0] == "iavl-nodes" else "EOF"
        process = None
        started = time.monotonic()
        stopping = None
        ready = False
        last_progress = None
        try:
            process = subprocess.Popen([args.daemon, *options], stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       creationflags=HIDE_WINDOW, start_new_session=os.name == "posix")
            events = queue.Queue()

            def drain(source, stream):
                try:
                    for line in iter(stream.readline, b""):
                        events.put((source, line))
                finally:
                    events.put((source, None))

            readers = [threading.Thread(target=drain, args=(source, stream), daemon=True)
                       for source, stream in (("stdout", process.stdout), ("stderr", process.stderr))]
            for reader in readers:
                reader.start()
            deadline = started + 1800
            ended = set()
            with (args.output / f"{label}.stdout.log").open("xb") as stdout, \
                 (args.output / f"{label}.stderr.log").open("xb") as stderr:
                while len(ended) < 2:
                    remaining = deadline - time.monotonic()
                    require(remaining > 0, f"{label}: worker deadline exceeded")
                    try:
                        source, line = events.get(timeout=min(0.5, remaining))
                    except queue.Empty:
                        continue
                    if line is None:
                        ended.add(source)
                        continue
                    output = stdout if source == "stdout" else stderr
                    output.write(line)
                    output.flush()
                    text = line.decode(errors="replace").strip()
                    prefix = "Genesis preparation: "
                    if source == "stderr" and text.startswith(prefix):
                        phase, count = text[len(prefix):].rsplit(" ", 1)
                        last_progress = (phase, int(count))
                    if source == "stdout" and "Tendermint 0.34 P2P listening on " in text:
                        ready = True
                    should_stop = ready if requested is None else (
                        last_progress is not None and last_progress[0] == requested[0]
                        and last_progress[1] >= requested[1])
                    if stopping is None and should_stop:
                        stopping = time.monotonic()
                        deadline = stopping + 45
                        if method == "SIGTERM":
                            process.send_signal(signal.SIGTERM)
                        else:
                            process.stdin.close()
                require(stopping is not None, f"{label}: target stage was never observed")
                require(process.wait(timeout=max(0.1, deadline - time.monotonic())) == 0,
                        f"{label}: worker exited {process.returncode}")
            elapsed = time.monotonic() - stopping
            for reader in readers:
                reader.join(timeout=1)
            if not process.stdin.closed:
                process.stdin.close()
            process.stdout.close()
            process.stderr.close()
            process = None
            if requested:
                require(not ready, f"{label}: genesis was advertised before cancellation")
                text = (args.output / f"{label}.stderr.log").read_text(errors="replace")
                require("Native startup cancelled; database closed" in text, f"{label}: missing cancellation result")
                empty(label + "-empty", database)
            else:
                require(ready, f"{label}: node never became ready")
            result = {"label": label, "requestedPhase": requested[0] if requested else "ready",
                      "lastProgress": last_progress, "method": method, "stopSeconds": round(elapsed, 3),
                      "prepareSeconds": round(stopping - started, 3), "ready": ready,
                      "noForcedTermination": True}
            observations.append(result)
            print(json.dumps(result), flush=True)
            return result
        finally:
            if process is not None:
                if process.stdin and not process.stdin.closed:
                    process.stdin.close()
                try:
                    process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=15)
                for stream in (process.stdout, process.stderr):
                    if stream:
                        stream.close()

    cases = [("file-bytes", 65536), ("file-hash-bytes", 65536), ("envelope-bytes", 65536),
             ("app-state-bytes", 65536), ("canonical-hash-bytes", 65536),
             ("iavl-mutations", 1000), ("iavl-nodes", 1000)]
    databases = {}
    for phase, at in cases:
        database = args.output / "Данные genesis" / phase
        databases[phase] = database
        launch("cancel-" + phase, database, (phase, at))
    database = databases["iavl-nodes"]
    launch("completed-after-cancel", database)
    history = command("completed-history", args.cli, ["iavl", "history", "--db", database])
    require(history["head"] == HEAD, f"genesis root differs from Go: {history}")
    status = command("completed-native-status", args.daemon, ["native-history", "status", "--db", database])
    require(status["head"] == HEAD and status["archiveFrom"] == status["archiveThrough"] == 0,
            "native genesis cursor could not reopen independently")
    proof = command("completed-proof", args.cli, ["iavl", "prove", "--db", database,
                    "--height", str(HEAD["height"]), "--key", "00"])
    command("completed-proof-verified", args.cli, ["iavl", "verify-proof", "--proof",
            args.output / "completed-proof.stdout.log", "--height", str(HEAD["height"]),
            "--app-hash", HEAD["appHash"], "--key", "00"])
    launch("reopened-without-genesis", database, with_genesis=False)
    resumed = command("resumed-history", args.cli, ["iavl", "history", "--db", database])
    resumed_proof = command("resumed-proof", args.cli, ["iavl", "prove", "--db", database,
                            "--height", str(HEAD["height"]), "--key", "00"])
    require(resumed == history and resumed_proof == proof, "reopen changed genesis/IAVL")
    require(digest(args.genesis) == GENESIS_SHA, "official input changed")
    result = {"platform": platform.platform(), "genesisSha256": GENESIS_SHA, "head": HEAD,
              "cancellations": observations[:len(cases)], "completed": observations[len(cases):],
              "history": history, "nativeStatus": status, "proofUnchanged": True,
              "sourceUnchanged": True, "noForcedTermination": True, "featureComplete": False,
              "remaining": ["checkpoints in shared preparation loops", "ABCI shutdown wiring",
                            "complete CLI/ABCI/GUI acceptance"]}
    (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "genesis", "output"):
        parser.add_argument(name, type=lambda path: Path(path).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
