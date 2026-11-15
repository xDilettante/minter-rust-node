"""Join a local Tendermint 0.34 network using a snapshot served by Go Minter."""

import argparse
import hashlib
import io
import json
import platform
import zlib
from pathlib import Path

from verify_mainnet_empty import fields, read_varint
from verify_send_devnet import (
    Node, allocate_ports, logs, receipt, require, set_toml, signed_root, start, wait_for,
)


SNAPSHOT_HEIGHT = 105


def launch(node, peers, phase, checkpoint=None, bounded=False):
    abci = f"127.0.0.1:{node.abci}"
    if checkpoint:
        command = [node.app, "--db", node.root / "state", "--abci-listen", abci,
                   "--state-sync-checkpoint", checkpoint]
        if bounded:
            command += ["--keep-iavl-versions", "32"]
    else:
        command = [node.app, "serve", node.root / "state", f"tcp://{abci}", str(SNAPSHOT_HEIGHT)]
    app_log = node.root / f"app-{phase}.log"
    node.application_process = start(command, app_log)
    node.processes = [(node.application_process, app_log)]
    wait_for(lambda: "ABCI 0.34 listening on" in logs(app_log), "ABCI socket", node.processes)
    tm_log = node.root / f"tm-{phase}.log"
    command = [node.engine, "node", "--home", node.home, "--abci", "socket",
               "--proxy_app", f"tcp://{abci}", "--rpc.laddr", f"tcp://127.0.0.1:{node.rpc_port}",
               "--p2p.laddr", f"tcp://127.0.0.1:{node.p2p}", "--p2p.pex=false",
               "--p2p.persistent_peers", ",".join(f"{peer.node_id}@127.0.0.1:{peer.p2p}" for peer in peers)]
    if bounded:
        command += ["--keep_blocks", "3", "--keep_index_blocks", "36", "--log_level", "debug"]
    node.engine_process = start(command, tm_log)
    node.processes.append((node.engine_process, tm_log))


def count_nodes(snapshot):
    raw = zlib.decompress(snapshot)
    stream = io.BytesIO(raw)
    count = 0
    while stream.tell() < len(raw):
        size = read_varint(stream)
        frame = stream.read(size)
        require(len(frame) == size, "truncated snapshot item")
        count += 2 in fields(frame)
    return count


def verify(args):
    args.output.mkdir(parents=True)
    ports = allocate_ports(9)
    nodes = []
    try:
        for i, (name, binary) in enumerate((("go-source", args.go), ("go-witness", args.go),
                                            ("rust", args.rust))):
            node = Node(args.output / name, args.tendermint, binary, args.fixture,
                        ports[3 * i:3 * i + 3], i == 0)
            nodes.append(node)
            path = node.home / "config/genesis.json"
            genesis = json.loads(logs(path))
            genesis["chain_id"] = "minter-mainnet-5"
            genesis["consensus_params"]["version"] = {"app_version": "8"}
            if args.bounded_history:
                genesis["consensus_params"]["evidence"]["max_age_num_blocks"] = "5"
                genesis["consensus_params"]["evidence"]["max_age_duration"] = "4000000000"
            path.write_text(json.dumps(genesis), encoding="utf-8")
        source, witness, rust = nodes
        launch(source, [witness], "source")
        launch(witness, [source], "witness")
        children = source.processes + witness.processes
        wait_for(lambda: min(source.height(), witness.height()) >= SNAPSHOT_HEIGHT + 4,
                 "Go snapshot and signed successors", children, timeout=90)
        snapshot_path = Path(f"state/snapshots/{SNAPSHOT_HEIGHT}/1/0")
        source_bytes = (source.root / snapshot_path).read_bytes()
        require(source_bytes == (witness.root / snapshot_path).read_bytes(), "Go snapshots differ")
        signed = signed_root(source, witness, SNAPSHOT_HEIGHT)
        successor = signed_root(source, witness, SNAPSHOT_HEIGHT + 1)
        checkpoint = {"format": 1, "network": "minter-mainnet-5", "height": SNAPSHOT_HEIGHT,
                      "blockHash": signed["blockHash"], "appHash": successor["appHash"],
                      "snapshotSha256": hashlib.sha256(source_bytes).hexdigest(),
                      "bytes": len(source_bytes), "nodes": count_nodes(source_bytes)}
        require(0 < checkpoint["bytes"] < 10_000_000 and checkpoint["nodes"] > 0,
                "expected one Go snapshot chunk")
        manifest = args.output / "checkpoint.json"
        manifest.write_text(json.dumps(checkpoint, indent=2) + "\n", encoding="utf-8")
        config_path = rust.home / "config/config.toml"
        config = set_toml(logs(config_path), "statesync", {
            "enable": "true", "rpc_servers": json.dumps(
                ",".join(f"http://127.0.0.1:{node.rpc_port}" for node in (source, witness))
            ),
            "trust_height": str(SNAPSHOT_HEIGHT + 1),
            "trust_hash": json.dumps(successor["blockHash"]),
            "trust_period": '"1h0m0s"', "discovery_time": '"5s"',
        })
        config_path.write_text(config, encoding="utf-8")
        launch(rust, [source, witness], "state-sync", manifest, args.bounded_history)
        children += rust.processes

        def caught_up():
            sync = rust.rpc("status")["sync_info"]
            return sync if not sync["catching_up"] and int(sync["latest_block_height"]) > SNAPSHOT_HEIGHT else None

        sync = wait_for(caught_up, "Rust State Sync and block catch-up", children, timeout=100)
        require(int(sync["earliest_block_height"]) > SNAPSHOT_HEIGHT, "Rust downloaded pre-snapshot blocks")
        engine_log = logs(rust.root / "tm-state-sync.log")
        require("Verified ABCI app" in engine_log and "Snapshot restored" in engine_log,
                "Tendermint did not verify the restored Rust ABCI state")
        fixture = json.loads(logs(args.fixture / "go-abci-v034.json"))
        raw = fixture["transactions"]["transfer"]
        reply = rust.rpc("broadcast_tx_sync", tx="0x" + raw)
        require(reply.get("code", 0) == 0, "Rust rejected a transfer after State Sync")
        tx = receipt(rust, source, raw)
        accounts = [a["address"] for a in fixture["blocks"][0]["accounts"][:2]]
        balances = [rust.balance(address, tx["height"]) for address in accounts]
        require(balances == [{"balance": "29999999999999999968", "nonce": 1},
                             {"balance": "10000000000000000000", "nonce": 0}],
                "balances differ after State Sync")
        floor = None
        if args.bounded_history:
            first_after_snapshot = int(sync["earliest_block_height"])
            def pruned():
                info = rust.rpc("status")["sync_info"]
                return info if int(info["earliest_block_height"]) > first_after_snapshot else None
            floor = int(wait_for(pruned, "block pruning after State Sync", children)["earliest_block_height"])
            require("pruned block and ABCI history" in logs(rust.root / "tm-state-sync.log"),
                    "companion did not advance the block floor")
        before = rust.height()
        rust.close()
        launch(rust, [source, witness], "restarted", manifest, args.bounded_history)
        children = source.processes + witness.processes + rust.processes
        wait_for(lambda: rust.height() > before, "blocks after restart", children, timeout=75)
        require([rust.balance(address, tx["height"]) for address in accounts] == balances,
                "historical state changed after restart")
        observed_height = rust.height() if args.bounded_history else before
        result = {"os": platform.platform(), "checkpoint": checkpoint,
                  "firstAvailableBlock": int(sync["earliest_block_height"]),
                  "transaction": tx, "balances": balances,
                  "restartSignedHeader": signed_root(rust, source, observed_height)}
        if args.bounded_history:
            restart_floor = int(rust.rpc("status")["sync_info"]["earliest_block_height"])
            require(restart_floor >= floor, "block history floor moved back after restart")
            result["prunedAfterStateSync"] = floor
            result["restartBlockFloor"] = restart_floor
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result))
    finally:
        for node in reversed(nodes):
            node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("rust", "go", "tendermint", "fixture", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--bounded-history", action="store_true", help="use the Minter Tendermint retention companion")
    args = parser.parse_args()
    for name in ("rust", "go", "tendermint", "fixture", "output"):
        setattr(args, name, getattr(args, name).resolve())
    verify(args)
