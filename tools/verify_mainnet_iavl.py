"""Replay the authenticated Minter corpus, interrupt it, and audit the recovered roots."""

import argparse
import gzip
import hashlib
import json
import platform
import re
import subprocess
import time
from pathlib import Path


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def check_file(path, expected):
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    require(path.stat().st_size == expected["bytes"] and digest == expected["sha256"],
            f"File differs from the trusted corpus manifest: {path.name}")


def run(command, log, timeout=1800):
    completed = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
    log.write_text(completed.stdout + completed.stderr, encoding="utf-8")
    if completed.returncode:
        raise RuntimeError(f"{command[1:3]} failed: {completed.stderr}")
    return json.loads(completed.stdout)


def interrupt(command, log_path, first_report):
    with log_path.open("wb") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 180
            while process.poll() is None and time.monotonic() < deadline:
                if f"committed {first_report}\n" in log_path.read_text(encoding="utf-8"):
                    process.kill()
                    break
                time.sleep(0.025)
            else:
                tail = log_path.read_text(encoding="utf-8")[-2000:]
                raise RuntimeError(f"Did not observe a committed height during active replay\n{tail}")
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=15)
    require(process.returncode != 0, "Replay completed before the forced interruption")
    reports = re.findall(r"^committed (\d+)$", log_path.read_text(encoding="utf-8"), re.M)
    return int(reports[-1]), process.returncode


def trace_reference(trace, heights):
    # Key 0x64 is updated by the Go application on the first block of this corpus.
    key = "64"
    values = {}
    total_operations = 0
    value = None
    with gzip.open(trace, "rt", encoding="utf-8") as stream:
        for line in stream:
            block = json.loads(line)
            height = block["height"]
            ops = block["iavl_operations"] or []
            total_operations += len(ops)
            for op in ops:
                if op["key_hex"] == key:
                    value = None if op.get("remove") else op.get("value_hex", "")
            if height in heights:
                values[height] = (value, total_operations)
            if height == heights[-1]:
                break
    require(all(height in values for height in heights), "Incomplete Go trace reference")
    return key, values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "corpus_manifest", "snapshot", "manifest", "trace", "headers", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    corpus = json.loads(args.corpus_manifest.read_text(encoding="utf-8"))
    files = corpus["files"]
    for path, name in (
        (args.snapshot, "go_checkpoint/snapshot.zlib"),
        (args.manifest, "go_checkpoint/manifest.json"),
        (args.trace, "abci-iavl.jsonl.gz"),
        (args.headers, "blocks.csv.gz"),
    ):
        check_file(path, files[name])
    checkpoint = corpus["benchmark_checkpoint"]
    start, through = checkpoint["height"], corpus["measured"]["to"]
    args.output.mkdir(parents=True)
    db = args.output / "rocksdb"
    cli = [str(args.binary.resolve(strict=True))]
    location = ["--db", str(db)]
    headers = ["--headers", str(args.headers)]
    replay = cli + ["iavl", "replay"] + location + headers
    verify = cli + ["iavl", "verify"] + location + headers
    status = cli + ["iavl", "status"] + location
    complete = replay + ["--trace", str(args.trace), "--through", str(through)]

    started = time.monotonic()
    imported = run(cli + ["checkpoint", "import"] + location + [
        "--snapshot", str(args.snapshot), "--manifest", str(args.manifest),
    ], args.output / "import.log")
    initial = {"height": imported["height"], "appHash": imported["appHash"]}
    require(initial == {"height": start, "appHash": checkpoint["appHash"]},
            "Imported state differs from the checkpoint")

    with gzip.open(args.trace, "rt", encoding="utf-8") as stream:
        changed = json.loads(next(stream))
    changed["iavl_operations"].append({"key_hex": "ff727573742d6961766c2d6661756c74", "value_hex": "01"})
    bad_trace = args.output / "divergent.jsonl.gz"
    with gzip.open(bad_trace, "wt", encoding="utf-8") as stream:
        stream.write(json.dumps(changed) + "\n")
    rejected = subprocess.run(replay + ["--trace", str(bad_trace), "--through", str(start + 1)],
                              capture_output=True, text=True, timeout=30)
    (args.output / "divergent.log").write_text(rejected.stdout + rejected.stderr, encoding="utf-8")
    require(rejected.returncode and f"IAVL AppHash mismatch at {start + 1}" in rejected.stderr,
            "Divergent mutation was not rejected")
    require(run(status, args.output / "after-rejection.log") == initial,
            "Divergent mutation changed the committed height")

    last_report, exit_code = interrupt(complete, args.output / "interrupted.log", start + 1000)
    recovered = run(verify, args.output / "recovered.log")
    resume_height = recovered["head"]["height"]
    require(last_report <= resume_height < through and recovered["roots"] == resume_height - start + 1,
            "Recovered height or root history differs from the committed prefix")

    resumed = run(complete, args.output / "replayed.log")
    final = run(verify, args.output / "verified.log")
    require(resumed["initial"] == recovered["head"] and resumed["blocks"] == through - resume_height,
            "Replay did not resume at the first missing height")
    require(final["head"] == resumed["head"] == {
        "height": through, "appHash": corpus["last_measured_app_hash"],
    }, "Final AppHash differs from the trusted corpus")
    require(final["roots"] == corpus["go_trace"]["count"] + 1, "Incomplete committed history")

    key, samples = trace_reference(args.trace, [start + 1, resume_height, through])
    total_operations = samples[through][1]
    require(total_operations == corpus["go_trace"]["iavl_operations"]
            and total_operations - samples[resume_height][1] == resumed["operations"],
            "Replayed operation count differs from the Go trace")
    queries = []
    for height, (value, _) in samples.items():
        actual = run(cli + ["iavl", "get"] + location + ["--key", key, "--height", str(height)],
                     args.output / f"get-{height}.log")
        require(actual == {"height": height, "key": key, "value": value},
                f"Historical read differs from Go at {height}")
        queries.append(actual)

    result = {
        "system": platform.platform(),
        "machine": platform.machine(),
        "chainId": corpus["chain_id"],
        "goOracleRevision": corpus["go_oracle_revision"],
        "checkpoint": initial,
        "rejectedHeight": start + 1,
        "interruptedAfter": last_report,
        "interruptedExitCode": exit_code,
        "recovered": recovered,
        "resumed": resumed,
        "verified": final,
        "operations": total_operations,
        "historyQueries": queries,
        "seconds": time.monotonic() - started,
    }
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
