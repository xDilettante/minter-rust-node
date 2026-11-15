"""Compare an offline native replay from a trusted checkpoint with signed Go blocks."""

import argparse
import json
from pathlib import Path
import platform
import subprocess

from verify_native_checkpoint import inputs
from verify_native_sync import check_trace, decode_trace
from verify_send_devnet import HIDE_WINDOW, require


def verify(args):
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    first = manifest["height"] + 1
    require(first <= args.through, "target must follow checkpoint")
    reference, responses = inputs(args.shard, args.replay, first, args.through)
    args.output.mkdir()
    log = args.output / "replay.log"
    with log.open("wb") as stream:
        result = subprocess.run([
            args.daemon, "native-sync", "--db", args.output / "state",
            "--checkpoint", args.checkpoint, "--checkpoint-sha256", args.checkpoint_sha256,
            "--blocks", args.shard, "--through", str(args.through), "--trace",
            "--keep-iavl-versions", "4",
        ], stdout=stream, stderr=subprocess.STDOUT, timeout=240, creationflags=HIDE_WINDOW)
    require(result.returncode == 0, log.read_text(encoding="utf-8", errors="replace")[-5000:])
    rows = [row for row in decode_trace(log) if "height" in row]
    require([row["height"] for row in rows] == list(range(first, args.through + 1)),
            "replay did not cover the selected range")
    check_trace(rows, reference, responses)
    return {"platform": platform.platform(), "from": first, "through": args.through,
            "blocks": len(rows), "transactions": sum(row["transactions"] for row in rows),
            "validatorUpdates": sum(row["validatorUpdates"] for row in rows),
            "final": {"height": rows[-1]["height"], "appHash": rows[-1]["appHash"]},
            "signedBlocksAndAbciMatchGo": True}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "checkpoint", "shard", "replay", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--checkpoint-sha256", required=True)
    parser.add_argument("--through", required=True, type=int)
    args = parser.parse_args()
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
