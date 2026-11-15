"""Check CLI peer validation and shutdown while an outbound peer is silent."""
import argparse
import json
from pathlib import Path
import socket
import subprocess
import time

from verify_send_devnet import HIDE_WINDOW, logs, require, stop


def verify(args):
    args.output.mkdir(parents=True)
    common = [args.daemon, "native-sync", "--genesis", args.genesis, "--p2p-listen", "127.0.0.1:0"]
    peer = "09" * 20 + "@127.0.0.1:1"
    invalid = {
        "requires-consensus": (["--consensus-peer", peer], "--consensus"),
        "duplicate-id": (["--consensus", "--consensus-peer", peer, "--consensus-peer", peer], "duplicate consensus peer ID"),
        "too-many": (["--consensus"] + [word for _ in range(17) for word in ("--consensus-peer", peer)], "too many configured consensus peers"),
    }
    checks = {}
    for name, (extra, expected) in invalid.items():
        path = args.output / f"{name}.log"
        with path.open("wb") as output:
            reply = subprocess.run(common + ["--db", args.output / name, *extra], stdout=output,
                                   stderr=subprocess.STDOUT, timeout=15, creationflags=HIDE_WINDOW)
        require(reply.returncode != 0 and expected in logs(path), f"validation failed: {name}: {logs(path)}")
        checks[name] = True
    with socket.create_server(("127.0.0.1", 0)) as listener:
        listener.settimeout(15)
        endpoint = "09" * 20 + f"@127.0.0.1:{listener.getsockname()[1]}"
        path = args.output / "silent-peer.log"
        database = args.output / "silent-peer"
        with path.open("wb") as output:
            process = subprocess.Popen(common + ["--db", database, "--consensus", "--consensus-peer", endpoint,
                                       "--timeout-secs", "30", "--shutdown-on-stdin-close"],
                                       stdin=subprocess.PIPE, stdout=output, stderr=subprocess.STDOUT,
                                       creationflags=HIDE_WINDOW)
        try:
            with listener.accept()[0] as connection:
                connection.settimeout(5)
                require(connection.recv(64), "outbound handshake did not start")
                began = time.monotonic()
                process.stdin.close()
                require(process.wait(timeout=3) == 0, logs(path))
                elapsed = time.monotonic() - began
                require(elapsed < 3, "shutdown waited for the 30-second peer timeout")
                checks["silent-peer-shutdown-seconds"] = elapsed
        finally:
            stop(process)
        identity = next(line.split()[-1].split("@")[0] for line in logs(path).splitlines()
                        if line.startswith("Tendermint 0.34 P2P listening on"))
        path = args.output / "self-id.log"
        with path.open("wb") as output:
            reply = subprocess.run(common + ["--db", database, "--consensus", "--consensus-peer", f"{identity}@127.0.0.1:1"],
                                   stdout=output, stderr=subprocess.STDOUT, timeout=15, creationflags=HIDE_WINDOW)
        require(reply.returncode != 0 and "consensus peer is this node" in logs(path), logs(path))
        checks["self-id"] = True
    (args.output / "result.json").write_text(json.dumps(checks, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(checks))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "genesis", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    verify(parser.parse_args())
