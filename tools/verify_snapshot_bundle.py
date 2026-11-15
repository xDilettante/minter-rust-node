"""Verify automatic mainnet context delivery, durable readiness and peer fallback."""
import argparse
import hashlib
import io
import json
import platform
import re
import shutil
import subprocess
import time
from pathlib import Path

from verify_mainnet_empty import read_varint
from verify_native_sync import decode_trace, header
from verify_send_devnet import HIDE_WINDOW, allocate_ports, logs, require, start, stop, wait_for
from verify_snapshot_history import MAGIC, frames


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    required = manifest["historyContext"]
    anchor, floor = manifest["height"], required["from"]
    data = (args.checkpoint / "history.pb").read_bytes()
    chunks, _ = frames(data)
    require(len(data) == required["bytes"] and hashlib.sha256(data).hexdigest() == required["sha256"]
            and len(chunks) == anchor-floor+1, "bundle descriptor differs")
    durations = {}

    def command(name, options, error=None, timeout=900):
        path = output / f"{name}.log"
        before = time.monotonic()
        with path.open("xb") as log:
            result = subprocess.run([args.daemon, *options], stdout=log, stderr=subprocess.STDOUT,
                                    timeout=timeout, creationflags=HIDE_WINDOW)
        durations[name] = round(time.monotonic()-before, 3)
        text = logs(path)
        if error:
            require(result.returncode != 0 and error in text, f"{name}: {text[-3000:]}")
            return text
        require(result.returncode == 0, f"{name}: {text[-3000:]}")
        return decode_trace(path)[-1]

    def status(name, db):
        return command(name, ["native-history", "status", "--db", db])

    def check(current, at, ready):
        require(current["head"] == {"height": anchor, "appHash": manifest["appHash"]}
                and current["archiveFrom"] == current["archiveThrough"] == anchor
                and current["contextFrom"] == at and current["requiredHistoryReady"] == ready, current)

    def package(name, descriptor=None, snapshot=True):
        destination = output / name
        destination.mkdir()
        for filename in ("state.pb", "blocks.pb", "snapshot.zlib"):
            if filename != "snapshot.zlib" or snapshot:
                shutil.copyfile(args.checkpoint / filename, destination / filename)
        if descriptor is None:
            shutil.copyfile(args.checkpoint / "manifest.json", destination / "manifest.json")
        else:
            altered = {**manifest, "historyContext": descriptor}
            (destination / "manifest.json").write_text(json.dumps(altered), encoding="utf-8")
        return destination

    def restore(name, db, pkg, *, missing=False, peers=()):
        trusted = hashlib.sha256((pkg / "manifest.json").read_bytes()).hexdigest()
        options = ["native-sync", "--db", db, "--checkpoint", pkg, "--checkpoint-sha256", trusted,
                   "--blocks", args.continuation, "--through", str(anchor), "--keep-iavl-versions", "4"]
        for peer in peers:
            options.extend(["--snapshot-peer", peer])
        return command(name, options, "snapshot requires history.pb" if missing else None)

    def blocked(name, db):
        before = status(name+"-before-gates", db)
        command(name+"-consensus", ["native-sync", "--db", db, "--p2p-listen", "127.0.0.1:0",
                                   "--consensus"], "trusted snapshot history is incomplete", timeout=30)
        command(name+"-apply", ["native-sync", "--db", db, "--blocks", args.continuation,
                               "--through", str(anchor+1)], "trusted snapshot history is incomplete", timeout=30)
        require(status(name+"-after-gates", db) == before, "refused work changed native state")

    def imported(name, db, path, error=None):
        return command(name, ["native-history", "import", "--db", db, "--input", path], error)

    metadata = package("offline-metadata")
    seed = output / "required-state"
    restore("missing-context", seed, metadata, missing=True)
    original = status("initial-status", seed)
    check(original, anchor, False)
    blocked("missing-context", seed)
    retention = json.loads(subprocess.check_output([args.cli, "iavl", "history", "--db", seed],
                                                   timeout=30, creationflags=HIDE_WINDOW))
    go_path = output / "go-reference.json"
    with go_path.open("xb") as log:
        subprocess.run([args.go_verifier, args.checkpoint / "history.pb",
                        args.checkpoint / "blocks.pb", args.checkpoint / "state.pb"],
                       stdout=log, stderr=subprocess.STDOUT, timeout=900,
                       creationflags=HIDE_WINDOW, check=True)
    go = json.loads(go_path.read_bytes())
    require(go["firstExpiredBoundaryVerified"] and go["records"] == anchor-floor
            and go["sha256"] == required["sha256"], go)

    partial = output / "partial-state"
    shutil.copytree(seed, partial)
    truncated = output / "first-record.pb"
    truncated.write_bytes(MAGIC + b"".join(chunks[:2]))
    imported("boundary-truncated", partial, truncated, "truncated context package")
    check(status("after-boundary", partial), anchor-1, False)
    blocked("after-boundary", partial)
    restore("resumed-checkpoint", partial, args.checkpoint)
    check(status("after-resume", partial), floor, True)
    # A ready marker survives reopen without either local context file or peers.
    restore("ready-without-context-file", partial, metadata)
    check(status("after-ready-reopen", partial), floor, True)
    after = json.loads(subprocess.check_output([args.cli, "iavl", "history", "--db", partial],
                                               timeout=30, creationflags=HIDE_WINDOW))
    require(after == retention, "context import changed retained IAVL")
    reexport = output / "automatic-reexport"
    exported = command("automatic-reexport", ["native-snapshot", "--db", partial, "--output", reexport,
                                              "--anchor-blocks", args.checkpoint / "blocks.pb",
                                              "--with-history-context"])
    require(exported["historyContext"] == required and exported["snapshotSha256"] == manifest["snapshotSha256"]
            and (reexport / "history.pb").read_bytes() == data, "automatic re-export changed the bundle")

    trailing = output / "trailing.pb"
    trailing.write_bytes(data + b"\x01")
    trailing_db = output / "trailing-state"
    shutil.copytree(seed, trailing_db)
    imported("trailing-import", trailing_db, trailing, "trailing context package data")
    check(status("after-trailing", trailing_db), floor, False)
    blocked("after-trailing", trailing_db)
    repeated = imported("trailing-retry", trailing_db, args.checkpoint / "history.pb")
    require(repeated["appended"] == 0, repeated)
    check(status("after-trailing-retry", trailing_db), floor, True)

    # Even a completely durable authenticated prefix cannot substitute for the
    # exact bytes committed to by the trusted manifest.
    for name, descriptor in (("wrong-hash", {**required, "sha256": "00" * 32}),
                             ("wrong-size", {**required, "bytes": required["bytes"]+1})):
        pkg = package(name+"-package", descriptor)
        db = output / (name+"-state")
        restore(name+"-bootstrap", db, pkg, missing=True)
        imported(name+"-import", db, args.checkpoint / "history.pb", "required context size or SHA-256 differs")
        check(status(name+"-status", db), floor, False)
        blocked(name, db)

    short_data = args.short_context.read_bytes()
    from verify_mainnet_empty import fields
    short_chunks, short_payloads = frames(short_data)
    short_floor = fields(short_payloads[0])[3]
    require(len(short_chunks) == anchor-short_floor+1 and short_floor > floor, "short fixture differs")
    short_descriptor = {"from": short_floor, "bytes": len(short_data),
                        "sha256": hashlib.sha256(short_data).hexdigest()}
    short_package = package("short-window-package", short_descriptor)
    shutil.copyfile(args.short_context, short_package / "history.pb")
    short_db = output / "short-window-state"
    trusted = hashlib.sha256((short_package / "manifest.json").read_bytes()).hexdigest()
    command("short-window", ["native-sync", "--db", short_db, "--checkpoint", short_package,
                             "--checkpoint-sha256", trusted, "--blocks", args.continuation,
                             "--through", str(anchor)], "snapshot context does not cover both age limits")
    check(status("short-window-status", short_db), short_floor, False)
    blocked("short-window", short_db)

    good = bad = None
    try:
        ports = allocate_ports(2)
        endpoints = []
        for name, port in zip(("bad", "good"), ports):
            db = output / (name+"-server-state")
            shutil.copytree(partial, db)
            pkg = args.checkpoint
            if name == "bad":
                pkg = package("bad-server-package")
                shutil.copyfile(args.checkpoint / "history.pb", pkg / "history.pb")
            log = output / (name+"-server.log")
            process = start([args.daemon, "native-sync", "--db", db, "--p2p-listen", f"127.0.0.1:{port}",
                             "--node-key", output / (name+"_key.json"), "--serve-snapshot", pkg], log)
            if name == "bad":
                bad = process
            else:
                good = process
            ready = wait_for(lambda: re.search(r"P2P listening on ([0-9a-f]{40})@", logs(log)),
                             name+" snapshot server", [(process, log)], timeout=45)
            endpoints.append(f"{ready.group(1)}@127.0.0.1:{port}")
            if name == "bad":
                # Advertise the verified original, then alter a chunk on disk.
                with (pkg / "history.pb").open("r+b") as file:
                    file.write(bytes([data[0] ^ 1]))
        receiver = output / "network-state"
        network_package = package("network-metadata", snapshot=False)
        restore("network-restore", receiver, network_package, peers=endpoints)
        network_log = logs(output / "network-restore.log")
        require("State Sync downloaded" in network_log and f"History peer {endpoints[0]}:" in network_log
                and "History State Sync downloaded" in network_log
                and endpoints[1].split("@")[0] in network_log, "context peer fallback not observed")
        check(status("network-status", receiver), floor, True)
        require(not list(receiver.glob("history-*.pb")) and not list(receiver.glob(".history-download-*")),
                "completed context left a download cache")
    finally:
        stop(good)
        stop(bad)

    continuation = io.BytesIO(args.continuation.read_bytes())
    blocks = [continuation.read(read_varint(continuation)) for _ in range(4)]
    expected_root = header(blocks[2])[11].hex()
    finals = []
    for name, db in (("resumed", partial), ("network", receiver)):
        continued = command(name+"-continued", ["native-sync", "--db", db, "--blocks", args.continuation,
                                                "--through", str(anchor+1), "--trace"])
        require(continued["through"] == anchor+1 and continued["appHash"] == expected_root, continued)
        final = status(name+"-final-status", db)
        require(final["requiredHistoryReady"] and final["contextFrom"] == floor
                and final["archiveFrom"] == anchor and final["archiveThrough"] == anchor+1, final)
        finals.append(final)
    return {"platform": platform.platform(), "snapshot": manifest, "go": go, "original": original,
            "finals": finals, "retentionUnchanged": True, "partialPrefixBlocksExecution": True,
            "resumedAfterOneRecord": True, "readyReopenNeedsNoContextFile": True,
            "wrongDigestAndSizeNeverReady": True, "trailingFileNeverReady": True,
            "completePrefixRetryAppended": repeated["appended"], "shortWindowNeverReady": True,
            "contextPeerFallbackVerified": True, "stateAndContextDownloaded": True,
            "automaticReexportByteEqual": True,
            "durationSeconds": durations}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "go_verifier", "checkpoint", "short_context", "continuation", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
