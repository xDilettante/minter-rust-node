"""Download a historical Minter snapshot over Tendermint 0.34 and resume block sync."""

import argparse
from contextlib import ExitStack, contextmanager
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time

from verify_mainnet_empty import app, read_varint
from verify_minter_api_v2 import get, listen
from verify_native_checkpoint import inputs
from verify_native_peer import cli_run, refused, success
from verify_native_sync import check_trace, decode_trace
from verify_send_devnet import HIDE_WINDOW, allocate_ports, logs, require, start, stop, wait_for


@contextmanager
def peer(args, port, mode):
    address = listen(port)
    if mode == "valid" and args.rust_serve_db is not None:
        path = args.output / "rust-valid.log"
        command = [args.daemon, "native-sync", "--db", args.rust_serve_db, "--p2p-listen",
                   address, "--serve-snapshot", args.checkpoint]
        pattern = r"^Tendermint 0\.34 P2P listening on (\S+)$"
    else:
        path = args.output / f"go-{mode}.log"
        command = [args.go_peer, args.checkpoint, args.shard, address, mode]
        pattern = r"^ready (\S+)$"
    process = start(command, path)
    try:
        ready = wait_for(lambda: re.search(pattern, logs(path), re.MULTILINE),
                         f"{mode} snapshot peer", [(process, path)])
        yield ready.group(1), path
    finally:
        stop(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "go_peer", "checkpoint", "shard", "replay", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--checkpoint-sha256", required=True)
    parser.add_argument("--through", type=int, default=10523520)
    parser.add_argument("--check-block-serving", action="store_true")
    parser.add_argument("--rust-serve-db", type=lambda value: Path(value).resolve(),
                        help="serve the valid snapshot and subsequent blocks from this Rust archive")
    args = parser.parse_args()

    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    height, last = manifest["height"], args.through
    live_tip = height + 2
    require(live_tip < last, "target must extend beyond the live replay")
    reference, responses = inputs(args.shard, args.replay, height + 1, last)
    output = args.output
    metadata = output / "metadata"
    metadata.mkdir(parents=True)
    for name in ("manifest.json", "state.pb", "blocks.pb"):
        shutil.copyfile(args.checkpoint / name, metadata / name)
    database = output / "state"
    command = [args.daemon, "native-sync", "--db", database, "--timeout-secs", "15", "--trace"]
    checkpoint = ["--checkpoint", metadata, "--checkpoint-sha256", args.checkpoint_sha256]
    archive = ["--blocks", args.shard]
    history_command = [args.cli, "iavl", "history", "--db", database]

    def run(name, *options, error=None):
        path = output / f"{name}.log"
        began = time.monotonic()
        with path.open("wb") as stream:
            result = subprocess.run([*command, *options], stdout=stream, stderr=subprocess.STDOUT,
                                    creationflags=HIDE_WINDOW, timeout=240)
        if error is None:
            require(result.returncode == 0, f"{name} failed: {logs(path)[-2500:]}")
        else:
            require(result.returncode != 0 and error in logs(path),
                    f"{name} did not reject the invalid transfer: {logs(path)[-2500:]}")
        return decode_trace(path), round(time.monotonic() - began, 3)

    def require_unpublished(name):
        result = subprocess.run(history_command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                creationflags=HIDE_WINDOW, timeout=20)
        (output / f"{name}-history.log").write_bytes(result.stdout)
        require(result.returncode != 0 and b"database has no committed Minter state" in result.stdout,
                f"{name} changed the database head: {result.stdout!r}")

    slow_port, corrupt_port, valid_port, api_port = allocate_ports(4)
    with peer(args, slow_port, "slow") as (slow, go_log):
        cut_log = output / "interrupted-download.log"
        process = start([*command, *checkpoint, "--snapshot-peer", slow, *archive,
                         "--through", str(height)], cut_log)
        try:
            wait_for(lambda: "chunk 1 bytes 10000000" in logs(go_log),
                     "second snapshot chunk request", [(process, cut_log)])
            require(process.poll() is None, "download completed before the forced stop")
            stop(process)
            require(process.returncode != 0, "snapshot download was not interrupted")
        finally:
            stop(process)
    require_unpublished("interrupted-download")

    with ExitStack() as peers:
        corrupt, _ = peers.enter_context(peer(args, corrupt_port, "corrupt"))
        run("corrupt-chunk", *checkpoint, "--snapshot-peer", corrupt, *archive, "--through", str(height),
            error="no State Sync peer served the trusted snapshot")
        require("snapshot chunk 1 rejected: Retry" in logs(output / "corrupt-chunk.log"),
                "client did not check the corrupt chunk")
        require_unpublished("corrupt-chunk")

        valid, go_log = peers.enter_context(peer(args, valid_port, "valid"))
        live, sync_seconds = run("network", *checkpoint, "--snapshot-peer", corrupt,
                                 "--snapshot-peer", valid, "--peer", valid, "--through", str(live_tip),
                                 "--keep-iavl-versions", "4")
        chunk_sizes = [min(10_000_000, manifest["bytes"] - offset)
                       for offset in range(0, manifest["bytes"], 10_000_000)]
        if args.rust_serve_db is None:
            require(re.findall(r"^chunk (\d+) bytes (\d+)$", logs(go_log), re.MULTILINE) ==
                    [(str(index), str(size)) for index, size in enumerate(chunk_sizes)],
                    "Go peer did not serve the snapshot in order")
        network_log = logs(output / "network.log")
        require("snapshot chunk 1 rejected: Retry" in network_log
                and f"State Sync downloaded {manifest['bytes']} bytes in {len(chunk_sizes)} chunks" in network_log
                and not (metadata / "snapshot.zlib").exists(), "missing State Sync peer failover")
        require(live[0]["startingHeight"] == height and live[0]["startingAppHash"] == manifest["appHash"]
                and live[-1]["through"] == live_tip and live[-1]["blocks"] == 2,
                "native sync did not start at the trusted checkpoint and replay two live blocks")

    resumed, _ = run("reopened", *archive, "--through", str(last))
    require(resumed[0]["startingHeight"] == live_tip and resumed[0]["startingAppHash"] ==
            reference[live_tip]["app_hash"], "restart did not resume from the live height")
    rows = [row for row in live + resumed if "height" in row]
    require([row["height"] for row in rows] == list(range(height + 1, last + 1)),
            "incomplete replay after network State Sync")
    check_trace(rows, reference, responses)
    final = {"height": last, "appHash": reference[last]["app_hash"]}
    retention = json.loads(subprocess.check_output(history_command, creationflags=HIDE_WINDOW, timeout=20))
    require(retention == {"head": final, "earliestHeight": last - 3, "keepIavlVersions": 4},
            "final root or IAVL window differs from the Go corpus")

    with app(args.daemon, database, output / "api.log", "--api-listen", listen(api_port)) as (client, _):
        require(client.info() == final, "native RocksDB head changed on reopening")
        versions = get(api_port, "/v2/version_network")
        require(versions["status"] == 200 and versions["body"]["current"] == "v330",
                "v330 activation missing from the reopened database")

    report = {
        "platform": platform.platform(), "checkpoint": {"height": height, "appHash": manifest["appHash"]},
        "validPeer": "rust" if args.rust_serve_db is not None else "go",
        "manifestSha256": args.checkpoint_sha256, "chunkSizes": chunk_sizes, "networkSyncSeconds": sync_seconds,
        "interruptedDownloadLeftNoHead": True, "corruptChunkLeftNoHead": True, "snapshotPeerFailover": True,
        "liveThrough": live_tip, "blocks": len(rows), "transactions": sum(row["transactions"] for row in rows),
        "validatorUpdates": sum(row["validatorUpdates"] for row in rows), "final": final, "retention": retention,
        "versionNetwork": versions["body"],
    }
    if args.check_block_serving:
        serve_log = output / "p2p.log"
        process = start([*command, "--p2p-listen", "127.0.0.1:0"], serve_log)
        try:
            ready = wait_for(lambda: re.search(r"Tendermint 0\.34 P2P listening on (\S+)", logs(serve_log)),
                             "Rust block listener", [(process, serve_log)])
            conn = ["--peer", ready.group(1), "--chain", manifest["network"]]
            requested = [height, height + 1, last]
            capture = output / "served-blocks.pb"
            downloaded = success(cli_run(args.cli, "block", *conn, "--output", capture,
                                         *[option for h in requested for option in ("--height", str(h))]))
            require(downloaded["peer"]["base"] == height and downloaded["peer"]["height"] == last,
                    "native archive does not advertise the checkpoint floor")
            with (args.checkpoint / "blocks.pb").open("rb") as source:
                checkpoint_block = source.read(read_varint(source))
            with capture.open("rb") as stream:
                for h, expected in zip(requested, [checkpoint_block, responses[height + 1], responses[last]]):
                    require(stream.read(read_varint(stream)) == expected,
                            f"Rust served different Tendermint bytes for block {h}")
                require(not stream.read(1), "extra served blocks")
            for missing in (height - 1, last + 1):
                refused(cli_run(args.cli, "block", *conn, "--height", str(missing)), "peer has no block")
            report["servedBlocks"] = downloaded
        finally:
            stop(process)
    (output / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
