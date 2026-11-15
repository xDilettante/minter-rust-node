"""Stop native cleanup through the controller and standalone EOF on a cold mainnet fixture."""
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

from verify_send_devnet import HIDE_WINDOW, allocate_ports, logs, require, stop, wait_for


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify(args):
    args.output.mkdir(parents=True)
    require((args.seed / "CURRENT").is_file(), "expected a cold RocksDB fixture")
    require(args.seed != args.output and args.seed not in args.output.parents, "output overlaps source")
    names = [p.name for p in args.seed.iterdir() if p.is_file() and re.fullmatch(
        r"[0-9]+\.(sst|log)|MANIFEST-[0-9]+|OPTIONS-[0-9]+|CURRENT|IDENTITY|LOCK", p.name)]
    require(all(not (args.seed / name).is_symlink() for name in names), "linked fixture input")
    before = {name: digest(args.seed / name) for name in names}
    data = args.output / "Данные полной ноды"
    database = data / "state"
    database.mkdir(parents=True)
    for name in names:
        shutil.copyfile(args.seed / name, database / name)
    require(before == {name: digest(args.seed / name) for name in names}, "fixture changed while copying")

    def command(binary, label, *options):
        result = subprocess.run([binary, *options], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=600, creationflags=HIDE_WINDOW)
        (args.output / f"{label}.stdout.log").write_bytes(result.stdout)
        (args.output / f"{label}.stderr.log").write_bytes(result.stderr)
        require(result.returncode == 0, f"{label}: {result.stderr.decode(errors='replace')[-2000:]}")
        text = result.stdout.decode("utf-8")
        if not text.strip():
            return None
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            rows = []
            for line in text.splitlines():
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
            require(rows, f"{label}: no JSON result")
            return rows[-1]

    def status(label):
        return command(args.daemon, label, "native-history", "status", "--db", database)

    def proof(label):
        value = command(args.cli, label, "iavl", "prove", "--db", database,
                        "--height", str(initial["head"]["height"]), "--key", "00")
        require(value["appHash"] == initial["head"]["appHash"], value)
        command(args.cli, f"{label}-verified", "iavl", "verify-proof", "--proof",
                args.output / f"{label}.stdout.log", "--height", str(initial["head"]["height"]),
                "--app-hash", initial["head"]["appHash"], "--key", "00")
        return value

    initial = status("initial")
    require(initial["head"] == {
        "height": 40700741, "appHash": "fbc9b7b8bdfb064de75082b404bc863ccdfe9a78f2365340e957c2957dfdedf1"}, initial)
    base, head = initial["archiveFrom"], initial["head"]["height"]
    require(head - base + 1 >= 100_000 and initial["contextFrom"] == base, initial)
    expected_proof = proof("initial-proof")
    count = 15_000
    target = base + count
    keep = head - target + 1
    config = args.output / "Настройки.json"
    settings = command(args.cli, "init", "node", "init", "--config", config,
                       "--data-dir", data, "--mode", "full")
    api, p2p = allocate_ports(2)
    settings.update({"fullStart": "resume", "storageProfile": "custom", "apiPort": api, "p2pPort": p2p,
                     "keepVersions": initial["retention"]["keepIavlVersions"],
                     "keepEventBlocks": initial["retention"]["keepEventBlocks"], "keepNativeBlocks": keep,
                     "peers": []})
    config.write_text(json.dumps(settings, ensure_ascii=False), encoding="utf-8")
    process = None
    observations = []
    try:
        owner_log = args.output / "controller.log"
        node_log = data / "node.log"
        with owner_log.open("xb") as output:
            process = subprocess.Popen([args.cli, "node", "run", "--config", config,
                                        "--shutdown-on-stdin-close"], stdin=subprocess.PIPE,
                                       stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=os.name == "posix", creationflags=HIDE_WINDOW)
        wait_for(lambda: "Native maintenance: native-blocks 1000" in logs(node_log),
                 "controller cleanup progress", [(process, owner_log)], timeout=240)
        active = command(args.cli, "controller-status", "node", "status", "--config", config)
        require(active["phase"] == "starting" and active["controllable"] and active["node"] is None, active)
        started = time.monotonic()
        command(args.cli, "remote-stop", "node", "stop", "--config", config)
        require(process.wait(timeout=45) == 0, logs(owner_log))
        elapsed = time.monotonic() - started
        process.stdin.close()
        process = None
        require("Native startup cancelled" in logs(node_log) and "принудительно" not in logs(owner_log), logs(node_log)[-2000:])
        interrupted = status("after-controller-stop")
        require(interrupted["head"] == initial["head"] and base + 1000 <= interrupted["archiveFrom"] < target, interrupted)
        require(interrupted["contextFrom"] == interrupted["archiveFrom"] and interrupted["keepNativeBlocks"] == keep, interrupted)
        observations.append({"method": "controller IPC", "seconds": round(elapsed, 3), "status": interrupted})

        path = args.output / "standalone.log"
        with path.open("xb") as output:
            process = subprocess.Popen([args.daemon, "native-history", "maintain", "--db", database,
                                        "--shutdown-on-stdin-close"], stdin=subprocess.PIPE,
                                       stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=os.name == "posix", creationflags=HIDE_WINDOW)
        wait_for(lambda: "Native maintenance: native-blocks 1000" in logs(path),
                 "standalone cleanup progress", [(process, path)], timeout=240)
        started = time.monotonic()
        process.stdin.close()
        require(process.wait(timeout=45) == 0, logs(path))
        elapsed = time.monotonic() - started
        process = None
        require("Native startup cancelled" in logs(path), logs(path)[-2000:])
        second = status("after-standalone-stop")
        require(second["head"] == initial["head"]
                and interrupted["archiveFrom"] + 1000 <= second["archiveFrom"] < target, second)
        observations.append({"method": "standalone EOF", "seconds": round(elapsed, 3), "status": second})

        command(args.daemon, "finish-cleanup", "native-history", "maintain", "--db", database)
        final = status("final")
        require(final["head"] == initial["head"] and final["archiveFrom"] == final["contextFrom"] == target
                and final["archiveThrough"] == head and final["keepNativeBlocks"] == keep, final)
        require(proof("final-proof") == expected_proof, "IAVL proof changed during native cleanup")
        require(before == {name: digest(args.seed / name) for name in names}, "cold fixture changed during test")
        result = {"platform": platform.platform(), "source": str(args.seed), "sourceFiles": before,
                  "initial": initial, "cancellations": observations, "final": final,
                  "fullBlocksRetired": count, "proofUnchanged": True, "sourceUnchanged": True,
                  "noForcedTermination": True}
        (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        return result
    finally:
        if process is not None:
            if process.stdin and not process.stdin.closed:
                process.stdin.close()
            stop(process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "seed", "output"):
        parser.add_argument(name, type=lambda p: Path(p).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
