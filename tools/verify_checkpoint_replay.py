"""Reject malformed checkpoint replay inputs before creating output state."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

from verify_send_devnet import HIDE_WINDOW, require


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("verifier", "genesis", "blocks", "checkpoint", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--checkpoint-sha256", required=True)
    args = parser.parse_args()
    args.output.mkdir()
    names = ("manifest.json", "state.pb", "blocks.pb", "snapshot.zlib")
    originals = {n: hashlib.sha256((args.checkpoint/n).read_bytes()).hexdigest() for n in names}
    base = json.loads((args.checkpoint/"manifest.json").read_bytes())
    cases = {
        "wrong-manifest-sha": "checkpoint manifest SHA-256 differs",
        "bad-snapshot": "checkpoint snapshot SHA-256 differs",
        "bad-state": "checkpoint state SHA-256 differs",
        "bad-blocks": "checkpoint blocks SHA-256 differs",
        "network": "checkpoint format, network or height differs",
        "height": "checkpoint consensus state differs",
        "app-hash": "checkpoint consensus state differs",
        "block-hash": "checkpoint consensus state differs",
        "format": "checkpoint format, network or height differs",
        "missing-state-hash": "checkpoint state SHA-256 differs",
        "genesis-identity": "checkpoint genesis identity differs",
        "transaction-network": "checkpoint genesis identity differs",
        "overflow-height": "checkpoint format, network or height differs",
        "empty-range": "replay range is outside supported heights",
        "empty-package": "checkpoint package path is empty",
        "empty-blocks": "no P2P block shards",
    }
    results = []
    for name, expected in cases.items():
        folder = args.output/name
        folder.mkdir()
        package = folder/"checkpoint"
        shutil.copytree(args.checkpoint, package)
        manifest = json.loads((package/"manifest.json").read_bytes())
        trusted = args.checkpoint_sha256
        if name.startswith("bad-"):
            path = package/{"bad-snapshot":"snapshot.zlib", "bad-state":"state.pb", "bad-blocks":"blocks.pb"}[name]
            raw = path.read_bytes()
            path.write_bytes(bytes([raw[0]^1])+raw[1:])
        elif name in ("network", "height", "app-hash", "block-hash", "format",
                      "missing-state-hash", "genesis-identity", "transaction-network", "overflow-height"):
            if name == "missing-state-hash":
                del manifest["consensusStateSha256"]
            else:
                key, value = {
                    "network": ("network", "minter-mainnet-6"),
                    "height": ("height", base["height"]+1),
                    "app-hash": ("appHash", "00"*32),
                    "block-hash": ("blockHash", "00"*32),
                    "format": ("format", 2),
                    "genesis-identity": ("genesisSha256", "00"*32),
                    "transaction-network": ("transactionChainId", 2),
                    "overflow-height": ("height", (1<<63)-1),
                }[name]
                manifest[key] = value
            raw = (json.dumps(manifest)+"\n").encode()
            (package/"manifest.json").write_bytes(raw)
            trusted = hashlib.sha256(raw).hexdigest()
        elif name == "wrong-manifest-sha":
            trusted = "00"*32
        blocks = args.blocks
        if name == "empty-blocks":
            blocks = folder/"empty-blocks"
            blocks.mkdir()
        output = folder/"result"
        command = [str(args.verifier), "--checkpoint", "" if name == "empty-package" else str(package),
                   trusted, str(args.genesis), str(blocks), str(output),
                   str(base["height"] if name == "empty-range" else base["height"]+1)]
        result = subprocess.run(command, capture_output=True, timeout=30, creationflags=HIDE_WINDOW)
        (folder/"stdout.log").write_bytes(result.stdout)
        (folder/"stderr.log").write_bytes(result.stderr)
        require(result.returncode != 0 and expected in result.stderr.decode(errors="replace"),
                f"wrong rejection for {name}: {result.stderr.decode(errors='replace')}")
        require(not output.exists(), f"invalid input created replay output for {name}")
        results.append({"case":name,"exitCode":result.returncode,"rejectedBeforeOutput":True})
    require(originals == {n:hashlib.sha256((args.checkpoint/n).read_bytes()).hexdigest() for n in names},
            "source checkpoint changed")
    report = {"verifierSha256":hashlib.sha256(args.verifier.read_bytes()).hexdigest(),
              "sourceCheckpointSha256":originals,"results":results}
    (args.output/"result.json").write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    print(json.dumps({"rejected":len(results),"allBeforeOutput":True}),flush=True)


if __name__ == "__main__":
    main()
