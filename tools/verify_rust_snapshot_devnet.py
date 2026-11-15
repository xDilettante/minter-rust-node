"""Restore a Go observer over Tendermint State Sync from a Rust-exported snapshot."""

import argparse
import base64
import hashlib
import json
import platform
import re
import shutil
import subprocess
from pathlib import Path
from types import SimpleNamespace

from verify_native_block_server import compare_go
from verify_native_peer import cli_run, success
from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, set_toml, signed_root, start, stop, wait_for


def verify(args):
    args.output.mkdir()
    ports = allocate_ports(10)
    source, witness, observer = [
        Node(args.output / name, args.tendermint, args.go_app, args.fixture, ports[i:i + 3], i == 0)
        for name, i in (("source", 0), ("witness", 3), ("observer", 6))
    ]
    genesis_path = args.output / "genesis.json"
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    genesis["consensus_params"]["version"] = {"app_version": "8"}
    genesis_path.write_text(json.dumps(genesis), encoding="utf-8")
    for node in (source, witness, observer):
        shutil.copyfile(genesis_path, node.home / "config/genesis.json")
    chain = genesis["chain_id"]
    first = int(genesis["initial_height"])
    snapshot_height = first + 9
    database = args.output / "rust-state"
    snapshot = args.output / "snapshot"
    replay = [args.daemon, "native-sync", "--db", database, "--trace",
              "--peer", f"{source.node_id}@127.0.0.1:{source.p2p}"]
    server = None
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())

    def run(command, name, error=None):
        path = args.output / f"{name}.log"
        with path.open("wb") as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                    creationflags=HIDE_WINDOW, timeout=120)
        if error is None:
            require(result.returncode == 0, f"{name} failed: {logs(path)[-2500:]}")
        else:
            require(result.returncode != 0 and error in logs(path), f"{name} accepted: {logs(path)[-2500:]}")
        return path

    try:
        source.launch(witness, "rpc-source", use_rust=False)
        witness.launch(source, "rpc-witness", use_rust=False)
        both = source.processes + witness.processes
        wait_for(lambda: min(source.height(), witness.height()) >= snapshot_height + 3,
                 "signed snapshot successors", both)
        run([*replay, "--genesis", genesis_path, "--through", str(snapshot_height),
             "--keep-iavl-versions", "4"], "rust-initial")
        export = [args.daemon, "native-snapshot", "--db", database, "--output", snapshot]
        exported = json.loads(logs(run(export, "export")))
        signed = signed_root(source, witness, snapshot_height)
        successor = signed_root(source, witness, snapshot_height + 1)
        require(exported == json.loads((snapshot / "manifest.json").read_bytes())
                and exported["blockHash"] == signed["blockHash"]
                and exported["appHash"] == successor["appHash"]
                and exported["height"] == snapshot_height and exported["nodes"] > 0,
                "Rust export differs from the signed checkpoint")
        run(export, "existing-output", error="create snapshot destination")
        with (snapshot / "snapshot.zlib").open("rb") as stream:
            require(hashlib.file_digest(stream, "sha256").hexdigest() == exported["snapshotSha256"],
                    "snapshot changed after refused export")
        if args.with_history_context:
            # This custom-genesis fixture uses a basic Go snapshot, not a native
            # mainnet checkpoint. Add a real authenticated context to exercise
            # both State Sync formats on the same Rust peer.
            history = snapshot / "history.pb"
            run([args.daemon, "native-history", "export", "--db", database,
                 "--anchor", str(snapshot_height), "--from", str(first), "--output", history],
                "export-history")
            exported["historyContext"] = {"from": first, "bytes": history.stat().st_size,
                                           "sha256": hashlib.sha256(history.read_bytes()).hexdigest()}
            (snapshot / "manifest.json").write_text(json.dumps(exported), encoding="utf-8")

        tx = source.rpc("broadcast_tx_commit", tx="0x" + fixture["transactions"]["transfer"])
        require(tx["check_tx"].get("code", 0) == tx["deliver_tx"].get("code", 0) == 0,
                "Go source rejected the test transfer")
        tx_height = int(tx["height"])
        tip = tx_height + 5
        wait_for(lambda: min(source.height(), witness.height()) >= tip + 2, "blocks after snapshot", both)
        replay_log = run([*replay, "--through", str(tip)], "rust-resumed")
        listen = [args.daemon, "native-sync", "--db", database, "--p2p-listen", f"127.0.0.1:{ports[-1]}"]
        damaged = args.output / "damaged"
        shutil.copytree(snapshot, damaged)
        altered = bytearray((damaged / "snapshot.zlib").read_bytes())
        altered[-1] ^= 1
        (damaged / "snapshot.zlib").write_bytes(altered)
        run([*listen, "--serve-snapshot", damaged], "damaged-snapshot",
            error="served snapshot differs from its manifest")
        wrong = {**exported, "appHash": "00" * 32}
        (damaged / "manifest.json").write_text(json.dumps(wrong), encoding="utf-8")
        run([*listen, "--serve-snapshot", damaged], "wrong-anchor",
            error="snapshot AppHash differs from the committed successor")

        rust_log = args.output / "rust-server.log"
        server = start([*listen, "--serve-snapshot", snapshot], rust_log)
        ready = wait_for(lambda: re.search(r"Tendermint 0\.34 P2P listening on ([0-9a-f]{40})@", logs(rust_log)),
                         "Rust snapshot listener", [(server, rust_log)])
        node_id = ready.group(1)
        endpoint = f"{node_id}@127.0.0.1:{ports[-1]}"
        status = success(cli_run(args.cli, "status", "--chain", chain, "--peer", endpoint))
        require(status["base"] == first and status["height"] == tip, "Rust did not retain the block archive")

        config = observer.home / "config/config.toml"
        config.write_text(set_toml(logs(config), "statesync", {
            "enable": "true",
            "rpc_servers": json.dumps(",".join(f"http://127.0.0.1:{node.rpc_port}" for node in (source, witness))),
            "trust_height": str(snapshot_height + 1),
            "trust_hash": json.dumps(successor["blockHash"]),
            "trust_period": '"1h0m0s"',
            "discovery_time": '"5s"',
        }), encoding="utf-8")
        go_log = observer.root / "app-state-sync.log"
        observer.application_process = start(
            [args.go_app, "serve", observer.root / "state", f"tcp://127.0.0.1:{observer.abci}", "1000000"], go_log)
        observer.processes = [(observer.application_process, go_log)]
        wait_for(lambda: "ABCI 0.34 listening on" in logs(go_log), "Go restore application", observer.processes)
        tm_log = observer.root / "tm-state-sync.log"
        observer.engine_process = start(
            [observer.engine, "node", "--home", observer.home, "--abci", "socket",
             "--proxy_app", f"tcp://127.0.0.1:{observer.abci}",
             "--rpc.laddr", f"tcp://127.0.0.1:{observer.rpc_port}", "--p2p.laddr", f"tcp://127.0.0.1:{observer.p2p}",
             "--p2p.pex=false", "--p2p.persistent_peers", endpoint], tm_log)
        observer.processes.append((observer.engine_process, tm_log))
        wait_for(lambda: "Time to switch to consensus reactor!" in logs(tm_log) and observer.height() >= tip - 2,
                 "Go snapshot restore and block sync", both + observer.processes + [(server, rust_log)], timeout=125)
        require("Verified ABCI app" in logs(tm_log) and "Snapshot restored" in logs(tm_log),
                "Go Tendermint did not restore and verify the Rust snapshot")
        if args.with_history_context:
            require(re.search(r"Snapshot format rejected[^\n]*format=1296258136(?:\D|$)", logs(tm_log))
                    and re.search(r"Snapshot restored[^\n]*format=1(?:\D|$)", logs(tm_log)),
                    "Go did not reject the extra format and restore format 1")
        go_status = observer.rpc("status")["sync_info"]
        require(int(go_status["earliest_block_height"]) == snapshot_height + 1, "Go downloaded pre-snapshot blocks")
        require([peer["node_info"]["id"] for peer in observer.rpc("net_info")["peers"]] == [node_id],
                "Go observer used another P2P peer")
        height = observer.height()
        receipts = {row["height"]: row for row in decode_trace(replay_log) if "height" in row}
        rows = [row for block, row in receipts.items() if snapshot_height < block <= height]
        require([row["height"] for row in rows] == list(range(snapshot_height + 1, height + 1)),
                "missing Rust receipts after snapshot")
        observed = compare_go(source, observer, rows, [tx_height])
        observer.close()
        config.write_text(set_toml(logs(config), "statesync", {"enable": "false"}), encoding="utf-8")
        observer.launch(SimpleNamespace(node_id=node_id, p2p=ports[-1]), "reopened", use_rust=False)
        wait_for(lambda: observer.height() >= height, "Go reopen after State Sync",
                 observer.processes + [(server, rust_log)])
        reopened = observer.rpc("status")["sync_info"]
        info = observer.rpc("abci_info")["response"]
        reopened_height = int(info["last_block_height"])
        require(int(reopened["earliest_block_height"]) == snapshot_height + 1
                and height <= reopened_height <= tip
                and base64.b64decode(info["last_block_app_hash"], validate=True).hex()
                == receipts[reopened_height]["appHash"],
                "Go state changed after restart")
        report = {"platform": platform.platform(), "snapshot": exported, "goHead": observed,
                  "firstGoBlock": snapshot_height + 1, "goReopenedHeight": reopened_height,
                  "rustTip": tip, "transactionHeight": tx_height,
                  "rustPeerId": node_id, "verifiedGoStateSync": True, "goReopened": True,
                  "damagedSnapshotRejected": True, "wrongAppHashRejected": True,
                  "extraContextFormatRejected": args.with_history_context}
        (args.output / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report))
    finally:
        observer.close()
        stop(server)
        witness.close()
        source.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "tendermint", "go_app", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--with-history-context", action="store_true")
    verify(parser.parse_args())


if __name__ == "__main__":
    main()
