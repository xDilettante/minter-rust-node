"""Check Rust IAVL proof bytes and verification against the Go checkpoint oracle."""

import argparse
import json
import platform
import subprocess
from pathlib import Path


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(command):
    return subprocess.run(command, capture_output=True, text=True, timeout=45)


def verify(args):
    reference = json.loads(args.go_proofs.read_bytes())
    height, root = reference["height"], reference["appHash"]
    args.output.mkdir(parents=True)
    results = []
    for index, item in enumerate(reference["values"]):
        key = item["key"]
        want = {"height": height, "appHash": root, **item}
        generated = run([args.cli, "iavl", "prove", "--db", args.db, "--height", str(height), "--key", key])
        require(generated.returncode == 0, generated.stderr)
        require(json.loads(generated.stdout) == want, f"Go and Rust proof bytes differ for key {key}")
        source = args.output / f"proof-{index}.json"
        source.write_text(json.dumps(want) + "\n", encoding="utf-8")
        command = [args.cli, "iavl", "verify-proof", "--proof", source, "--height", str(height),
                   "--app-hash", root, "--key", key]
        checked = run(command)
        require(checked.returncode == 0, checked.stderr)
        require(json.loads(checked.stdout) == {"height": height, "key": key, "value": item["value"]},
                f"verified value differs from Go for key {key}")
        if index == 0:
            require(run(command[:-1] + [key + "00"]).returncode != 0, "accepted another key")
            bad_height = list(command)
            bad_height[6] = str(height + 1)
            require(run(bad_height).returncode != 0, "accepted another height")
            bad_root = list(command)
            bad_root[8] = "00" * 32
            require(run(bad_root).returncode != 0, "accepted another root")
            source.write_text(json.dumps({**want, "value": "ff"}) + "\n", encoding="utf-8")
            require(run(command).returncode != 0, "accepted a false value alongside the proof")
            source.write_text(json.dumps(want) + "\n", encoding="utf-8")
        results.append({"key": key, "present": item["value"] is not None,
                        "proofBytes": len(item["proof"]) // 2})
    result = {"os": platform.platform(), "height": height, "appHash": root,
              "proofs": results, "rejectedAlteredQuery": True}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("cli", "db", "go_proofs", "output"):
        parser.add_argument(name, type=Path)
    verify(parser.parse_args())
