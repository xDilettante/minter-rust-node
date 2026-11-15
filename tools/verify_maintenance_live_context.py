"""Cancel a complete live-reference scan and prove its memberships survive cleanup."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import time

from verify_send_devnet import HIDE_WINDOW, allocate_ports, logs, require, stop, wait_for


def verify(args):
    args.output.mkdir(parents=True)
    previous = json.loads((args.runtime / "result.json").read_bytes())
    require(previous["sourceUnchanged"] and previous["noForcedTermination"], "complete the cold fixture case first")
    database = args.runtime / "Данные полной ноды/state"

    def run(label, *options):
        result = subprocess.run([args.daemon, *options], capture_output=True, timeout=600, creationflags=HIDE_WINDOW)
        (args.output / f"{label}.stdout.log").write_bytes(result.stdout)
        (args.output / f"{label}.stderr.log").write_bytes(result.stderr)
        require(result.returncode == 0, result.stderr.decode(errors="replace")[-2000:])
        return json.loads(result.stdout)

    def status(label):
        return run(label, "native-history", "status", "--db", database)

    initial = status("initial")
    require(initial == previous["final"], "runtime fixture was changed after the first case")
    path = args.output / "interrupted-scan.log"
    process = None
    try:
        with path.open("xb") as output:
            process = subprocess.Popen([args.daemon, "native-sync", "--db", database,
                                        "--p2p-listen", f"127.0.0.1:{allocate_ports(1)[0]}",
                                        "--keep-native-blocks", "2", "--shutdown-on-stdin-close"],
                                       stdin=subprocess.PIPE, stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=os.name == "posix", creationflags=HIDE_WINDOW)
        wait_for(lambda: "Native maintenance: membership-records 1000" in logs(path),
                 "partial live-reference scan", [(process, path)], timeout=600)
        began = time.monotonic()
        if os.name == "posix":
            process.send_signal(signal.SIGTERM)
            method = "SIGTERM"
        else:
            process.stdin.close()
            method = "EOF"
        require(process.wait(timeout=45) == 0, logs(path)[-2000:])
        elapsed = time.monotonic() - began
        if not process.stdin.closed:
            process.stdin.close()
        process = None
        require("Native startup cancelled" in logs(path), "scan completed before the cancellation")
        interrupted = status("after-stop")
        head = initial["head"]["height"]
        require(interrupted["head"] == initial["head"] and interrupted["archiveFrom"] == head - 1
                and interrupted["keepNativeBlocks"] == 2 and interrupted["contextFrom"] < head - 1000, interrupted)

        def export(label):
            target = args.output / f"{label}.pb"
            run(label, "native-history", "export", "--db", database, "--anchor", str(head),
                "--from", str(interrupted["contextFrom"]), "--output", target)
            with target.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            require(digest == "f58ef8087736b1e3a8771495ed823a4231c903875b0f1c1ed25b4bb9b3520d20"
                    and target.stat().st_size == 53505303, "live history differs from the previously Go-verified corpus")
            return {"sha256": digest, "bytes": target.stat().st_size}

        before = export("history-before-gc")
        run("completed", "native-history", "maintain", "--db", database)
        final = status("final")
        require(final == interrupted, "cleanup changed published ranges or application state")
        require(export("history-after-gc") == before, "live memberships changed")
        result = {"platform": platform.platform(), "method": method, "cancelSeconds": round(elapsed, 3),
                  "initial": initial, "interrupted": interrupted, "final": final,
                  "verifiedHistory": before, "partialScanPreservedLiveMemberships": True,
                  "resumedCollection": True, "noForcedTermination": True}
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        return result
    finally:
        if process is not None:
            if process.stdin and not process.stdin.closed:
                process.stdin.close()
            stop(process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "runtime", "output"):
        parser.add_argument(name, type=lambda p: Path(p).resolve())
    print(json.dumps(verify(parser.parse_args())))
