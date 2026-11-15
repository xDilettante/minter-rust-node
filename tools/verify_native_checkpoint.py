"""Exercise a native Go checkpoint import, recovery and Tendermint peer failover."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import threading
import time

from verify_mainnet_empty import app, read_varint
from verify_minter_api_v2 import get, listen
from verify_native_sync import check_trace, decode_trace, header
from verify_send_devnet import HIDE_WINDOW, allocate_ports, logs, require, start, stop, wait_for


def inputs(shard, replay, start_height, last_height):
    reference, responses = {}, {}
    with gzip.open(replay, "rt", encoding="utf-8") as stream:
        for line in stream:
            row = json.loads(line)
            height = row["height"]
            if start_height <= height <= last_height:
                reference[height] = row
            if height >= last_height:
                break
    with shard.open("rb") as stream:
        while stream.peek(1):
            size = read_varint(stream)
            payload = stream.read(size)
            require(len(payload) == size, "truncated block shard")
            height = header(payload)[3]
            if start_height <= height <= last_height + 1:
                responses[height] = payload
            if height == last_height + 1:
                break
    require(sorted(reference) == list(range(start_height, last_height + 1))
            and sorted(responses) == list(range(start_height, last_height + 2)), "incomplete Go corpus")
    return reference, responses


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "checkpoint", "shard", "replay", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--checkpoint-sha256", required=True)
    parser.add_argument("--peer", required=True)
    parser.add_argument("--through", required=True, type=int)
    args = parser.parse_args()
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    height, last = manifest["height"], args.through
    require(height < last, "target must follow the snapshot")
    root = manifest["appHash"]
    expected = {"height": height, "appHash": root}
    reference, responses = inputs(args.shard, args.replay, height + 1, last)
    out = args.output
    out.mkdir()
    database = out / "state"
    archive = ["--blocks", args.shard]
    checkpoint = ["--checkpoint", args.checkpoint, "--checkpoint-sha256", args.checkpoint_sha256]
    command = [args.daemon, "native-sync", "--db", database, "--trace"]

    def run(name, *options, error=None):
        path = out / (name + ".log")
        began = time.monotonic()
        with path.open("wb") as stream:
            status = subprocess.run(
                [*command, *options], stdout=stream, stderr=subprocess.STDOUT,
                creationflags=HIDE_WINDOW, timeout=240,
            )
        if error is None:
            require(status.returncode == 0, f"{name} failed: {logs(path)[-2500:]}")
        else:
            require(status.returncode != 0 and error in logs(path),
                    f"{name} did not refuse the input: {logs(path)[-2500:]}")
        return decode_trace(path), round(time.monotonic() - began, 3)

    def history():
        return json.loads(subprocess.check_output(
            [args.cli, "iavl", "history", "--db", database],
            creationflags=HIDE_WINDOW, timeout=20,
        ))

    run("wrong-manifest", "--checkpoint", args.checkpoint, "--checkpoint-sha256", "00" * 32,
        *archive, "--through", str(height), error="manifest SHA-256")
    require(not database.exists(), "untrusted manifest created a database")

    damaged = out / "damaged"
    damaged.mkdir()
    for name in ("manifest.json", "snapshot.zlib", "state.pb", "blocks.pb"):
        shutil.copyfile(args.checkpoint / name, damaged / name)
    bad = ["--checkpoint", damaged, "--checkpoint-sha256", args.checkpoint_sha256]

    def corrupt(name, error):
        path = damaged / name
        data = bytearray(path.read_bytes())
        data[len(data) // 2] ^= 1
        path.write_bytes(data)
        run(f"bad-{name}", *bad, *archive, "--through", str(height), error=error)
        shutil.copyfile(args.checkpoint / name, path)

    corrupt("state.pb", "state SHA-256")
    corrupt("blocks.pb", "blocks SHA-256")
    corrupt("snapshot.zlib", "snapshot SHA-256")
    # Even a package with matching file digests must contain a valid signed state.
    wrong_state = bytearray((damaged / "state.pb").read_bytes())
    anchor_root = bytes.fromhex(root)
    index = wrong_state.rfind(anchor_root)
    require(index != -1, "cannot locate AppHash in Tendermint State")
    wrong_state[index] ^= 1
    (damaged / "state.pb").write_bytes(wrong_state)
    altered_manifest = {**manifest, "consensusStateSha256": hashlib.sha256(wrong_state).hexdigest()}
    raw_manifest = json.dumps(altered_manifest).encode()
    (damaged / "manifest.json").write_bytes(raw_manifest)
    run("inconsistent-state", "--checkpoint", damaged, "--checkpoint-sha256",
        hashlib.sha256(raw_manifest).hexdigest(), *archive, "--through", str(height),
        error="Tendermint state differs from the trusted checkpoint")

    cut_log = out / "import-cut.log"
    process = start([*command, *checkpoint, *archive, "--through", str(height),
                     "--keep-iavl-versions", "4"], cut_log)
    try:
        wait_for(lambda: any(path.stat().st_size > 1_000_000 for path in database.glob("*.log")),
                 "staged IAVL import", [(process, cut_log)], timeout=40)
        require(process.poll() is None, "import completed before the forced stop")
        stop(process)
        require(process.returncode != 0, "snapshot import was not interrupted")
    finally:
        stop(process)
    run("unpublished", *archive, "--through", str(height), error="resume the interrupted import")
    imported, import_seconds = run("imported", *checkpoint, *archive, "--through", str(height))
    require(imported == [{"through": height, "appHash": root, "blocks": 0}], "imported the wrong state")
    require(history() == {"head": expected, "earliestHeight": height, "keepIavlVersions": 4},
            "restored root or retention differs from the manifest")
    run("reopened", *archive, "--through", str(height))
    print(f"Imported and reopened checkpoint {height}", flush=True)

    # The first endpoint stalls during handshake. The same process must advance on the second.
    with socket.socket() as stalled:
        stalled.bind(("127.0.0.1", 0))
        stalled.listen(1)
        stalled.settimeout(10)
        accepted = []

        def hold():
            with stalled.accept()[0] as connection:
                accepted.append(True)
                connection.settimeout(10)
                while connection.recv(4096):
                    pass

        thread = threading.Thread(target=hold, daemon=True)
        thread.start()
        live, _ = run(
            "peer-fallback", "--peer", f"{'a' * 40}@127.0.0.1:{stalled.getsockname()[1]}",
            "--peer", args.peer, "--timeout-secs", "3", "--through", str(height + 2), "--follow",
        )
        thread.join(timeout=5)
        require(accepted and not thread.is_alive(), "did not close the timed-out peer")
    require("Secret Connection handshake" in logs(out / "peer-fallback.log"),
            "missing failover to the archive peer")
    resumed, _ = run("resumed", *archive, "--through", str(last))
    rows = [row for row in live + resumed if "height" in row]
    require([row["height"] for row in rows] == list(range(height + 1, last + 1)),
            "incomplete native replay after snapshot")
    check_trace(rows, reference, responses)
    final = {"height": last, "appHash": reference[last]["app_hash"]}
    retained = history()
    require(retained == {"head": final, "earliestHeight": last - 3, "keepIavlVersions": 4},
            "final AppHash or IAVL window differs")

    port, = allocate_ports(1)
    with app(args.daemon, database, out / "api.log", "--api-listen", listen(port)) as (client, _):
        require(client.info() == final, "native RocksDB head changed on reopening")
        versions = get(port, "/v2/version_network")
        events = get(port, f"/v2/events/{height + 1}")
        activation = {"type": "minter/UpdateNetworkEvent", "value": {"version": "v330"}}
        require(versions["status"] == 200 and versions["body"]["current"] == "v330"
                and events["status"] == 200 and activation in events["body"]["events"],
                "v330 activation missing from the reopened database")
        proof = client.iavl_key(b"p", last - 3)
        require(proof.get(1, 0) == 0 and proof[9] == last - 3 and proof[7] and proof[8],
                "oldest retained proof missing")
        require(client.iavl_key(b"p", height).get(1, 0) != 0, "snapshot root should be pruned")

    report = {
        "platform": platform.platform(), "checkpoint": expected, "manifestSha256": args.checkpoint_sha256,
        "importSeconds": import_seconds, "interruptedImportRecovered": True,
        "rejected": ["manifest", "state.pb", "blocks.pb", "snapshot.zlib", "inconsistent state"],
        "peerFailover": True, "liveThrough": height + 2, "blocks": len(rows),
        "transactions": sum(row["transactions"] for row in rows),
        "validatorUpdates": sum(row["validatorUpdates"] for row in rows),
        "final": final, "retention": retained, "versionNetwork": versions["body"],
        "activationEvent": activation,
    }
    (out / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
