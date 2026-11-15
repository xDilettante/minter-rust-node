"""Upgrade a copied legacy checkpoint database without re-executing its blocks."""

import argparse
import json
from pathlib import Path
import platform
import shutil
import subprocess
import time

from verify_send_devnet import HIDE_WINDOW, require


def verify(args):
    args.output.mkdir()
    database = args.output / "state"
    shutil.copytree(args.database, database)

    def history():
        return json.loads(subprocess.check_output(
            [args.cli, "iavl", "history", "--db", database],
            creationflags=HIDE_WINDOW, timeout=30))

    original = history()
    head = original["head"]
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    command = [args.daemon, "native-sync", "--db", database,
               "--blocks", args.checkpoint / "blocks.pb", "--through", str(head["height"])]

    def run(name, *options, error=None):
        path = args.output / f"{name}.log"
        began = time.monotonic()
        with path.open("wb") as log:
            result = subprocess.run([*command, *options], stdout=log, stderr=subprocess.STDOUT,
                                    timeout=240, creationflags=HIDE_WINDOW)
        text = path.read_text(encoding="utf-8", errors="replace")
        if error:
            require(result.returncode != 0 and error in text, text)
        else:
            require(result.returncode == 0, text)
            rows = [json.loads(line) for line in text.splitlines() if line.startswith("{")]
            require(rows == [{"through": head["height"], "appHash": head["appHash"], "blocks": 0}], rows)
        require(history() == original, "upgrade changed the application root or retained history")
        return text, round(time.monotonic() - began, 3)

    run("missing-anchor", error="upgrade this database once with --checkpoint")
    run("wrong-manifest", "--checkpoint", args.checkpoint, "--checkpoint-sha256", "00" * 32,
        error="manifest SHA-256")
    restored, seconds = run("restored", "--checkpoint", args.checkpoint,
                            "--checkpoint-sha256", args.checkpoint_sha256)
    marker = f"Restored Tendermint validator history from {manifest['height']} through {head['height']}"
    require(marker in restored, "history scan did not cover the checkpoint through the chain head")
    reopened, _ = run("reopened")
    require(marker not in reopened, "completed history migration ran again")
    return {"platform": platform.platform(), "checkpointHeight": manifest["height"],
            "final": head, "retention": original, "migrationSeconds": seconds,
            "blocksReexecuted": 0, "missingAnchorRefused": True,
            "wrongManifestRefused": True, "reopenedWithoutPackage": True}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "database", "checkpoint", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--checkpoint-sha256", required=True)
    args = parser.parse_args()
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
