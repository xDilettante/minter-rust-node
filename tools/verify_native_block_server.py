"""Synchronize a Go Minter observer from the verified Rust block archive."""

import argparse
import base64
import json
from pathlib import Path
import platform
import re
import subprocess
from types import SimpleNamespace

from verify_native_peer import cli_run, refused, success
from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, start, stop, wait_for


def compare_go(source, observer, rows, transfer_heights):
    start_height, last_height = rows[0]["height"], rows[-1]["height"]
    for row in rows:
        height = row["height"]
        first, second = (node.rpc("commit", height=height)["signed_header"] for node in (source, observer))
        require(first == second and int(first["header"]["height"]) == height
                and first["commit"]["block_id"]["hash"].lower() == row["blockHash"]
                and any(sig["block_id_flag"] == 2 and sig.get("signature")
                        for sig in first["commit"]["signatures"]),
                f"signed header or block hash differs at {height}")
        if height > start_height:
            require(first["header"]["app_hash"].lower() == previous_root,
                    f"Rust AppHash differs at {height - 1}")
        previous_root = row["appHash"]
        first_results, second_results = (node.rpc("block_results", height=height) for node in (source, observer))
        require(first_results == second_results
                and len(first_results["txs_results"] or []) == row["transactions"]
                and len(first_results["validator_updates"] or []) == row["validatorUpdates"],
                f"ABCI results differ at {height}")
    require(any(start_height <= height <= last_height for height in transfer_heights),
            "the tested Go range is missing its transaction")
    head = observer.rpc("abci_info")["response"]
    expected = source.rpc("commit", height=last_height + 1)["signed_header"]["header"]["app_hash"].lower()
    require(int(head["last_block_height"]) == last_height
            and base64.b64decode(head["last_block_app_hash"], validate=True).hex() == expected
            and rows[-1]["appHash"] == expected,
            "Go observer committed a different state")
    return {"height": last_height, "appHash": expected}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "tendermint", "go_app", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir()
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    chain = fixture["chainId"]
    first_height = fixture["genesisHeight"] + 1
    ports = allocate_ports(7)
    source = Node(args.output / "source", args.tendermint, args.go_app, args.fixture, ports[:3], True)
    observer = Node(args.output / "observer", args.tendermint, args.go_app, args.fixture, ports[3:6], False)
    version = subprocess.check_output([args.tendermint, "version"], text=True,
                                      creationflags=HIDE_WINDOW, timeout=15).strip()
    require(version == "0.34.19", f"unexpected Go Tendermint version {version}")
    p2p = ports[6]
    database = args.output / "state"
    key = args.output / "node_key.json"
    server = None
    transfers = []

    def rust(command, phase):
        log = args.output / f"rust-{phase}.log"
        server = start([args.daemon, "native-sync", "--db", database, "--node-key", key,
                        "--p2p-listen", f"127.0.0.1:{p2p}", "--trace", *command], log)
        try:
            children = [*source.processes, (server, log)]
            ready = wait_for(lambda: re.search(r"Tendermint 0\.34 P2P listening on ([0-9a-f]{40})@", logs(log)),
                             "Rust P2P listener", children)
            return server, log, ready.group(1)
        except BaseException:
            stop(server)
            raise

    def wait_for_range(node_id, tip, phase_log):
        peer = f"{node_id}@127.0.0.1:{p2p}"
        wait_for(lambda: any(row.get("through") == tip for row in decode_trace(phase_log)),
                 "Rust block replay", [*source.processes, (server, phase_log)], timeout=100)
        status = success(cli_run(args.cli, "status", "--chain", chain, "--peer", peer))
        require(status["peerId"] == node_id and status["chainId"] == chain
                and status["base"] == first_height and status["height"] == tip,
                f"unexpected Rust block range {status}")
        return peer

    def synchronize(node_id, tip, phase, start_height, rust_log):
        observer.launch(SimpleNamespace(node_id=node_id, p2p=p2p), phase)
        children = [*source.processes, *observer.processes]
        # Tendermint can leave block sync once the next requested height reaches tip - 1.
        # Wait for its fast-sync handoff before recording the committed height.
        phase_log = observer.root / f"tm-{phase}.log"
        wait_for(lambda: "Time to switch to consensus reactor!" in logs(phase_log),
                 "Go block sync from Rust", children, timeout=100)
        peer_ids = [peer["node_info"]["id"] for peer in observer.rpc("net_info")["peers"]]
        require(peer_ids == [node_id], f"Go observer obtained blocks from an unexpected peer: {peer_ids}")
        head = observer.height()
        require(tip - 2 <= head < tip
                and int(observer.rpc("status")["validator_info"]["voting_power"]) == 0,
                "unexpected observer height or validator power")
        paths = [rust_log] if phase == "initial" else [args.output / "rust-initial.log", rust_log]
        rows = [row for path in paths for row in decode_trace(path)
                if start_height <= row.get("height", 0) <= head]
        require([row["height"] for row in rows] == list(range(start_height, head + 1)),
                "missing Rust receipts for the Go sync range")
        return compare_go(source, observer, rows, transfers)

    def send_transaction(name):
        result = source.rpc("broadcast_tx_commit", tx="0x" + fixture["transactions"][name])
        require(result["check_tx"].get("code", 0) == result["deliver_tx"].get("code", 0) == 0,
                f"Go source rejected {name}")
        transfers.append(int(result["height"]))

    try:
        source.launch(None, "source", use_rust=False)
        wait_for(lambda: source.height() >= first_height + 4, "Go source producing blocks", source.processes)
        send_transaction("transfer")
        first_tip = transfers[0] + 8
        wait_for(lambda: source.height() >= first_tip + 2, "first signed Go range", source.processes)
        upstream = ["--peer", f"{source.node_id}@127.0.0.1:{source.p2p}"]
        server, phase_log, node_id = rust([*upstream, "--through", str(first_tip),
                                         "--genesis", args.fixture / "genesis.json",
                                         "--keep-iavl-versions", "4"], "initial")
        peer = wait_for_range(node_id, first_tip, phase_log)
        fetched = success(cli_run(args.cli, "block", "--chain", chain, "--peer", peer,
                                  "--height", str(first_height), "--height", str(transfers[0])))
        require([row["height"] for row in fetched["blocks"]] == [first_height, transfers[0]]
                and fetched["blocks"][1]["transactions"] == 1, "Rust did not serve the requested blocks")
        refused(cli_run(args.cli, "block", "--chain", chain, "--peer", peer, "--height", str(first_tip + 1)),
                "peer has no block")
        refused(cli_run(args.cli, "status", "--chain", "other-fixture", "--peer", peer), "unexpected peer chain")
        initial = synchronize(node_id, first_tip, "initial", first_height, phase_log)

        observer.close()
        stop(server)
        retained = json.loads(subprocess.check_output(
            [args.cli, "iavl", "history", "--db", database], creationflags=HIDE_WINDOW, timeout=20))
        require(retained["earliestHeight"] == first_tip - 3 and retained["head"]["height"] == first_tip,
                "IAVL retention did not remain independent of the native block range")
        send_transaction("self")
        second_tip = transfers[1] + 8
        wait_for(lambda: source.height() >= second_tip + 2, "extended signed Go range", source.processes)
        server, phase_log, reopened_id = rust([*upstream, "--through", str(second_tip)], "resumed")
        require(reopened_id == node_id, "Rust P2P ID changed on reopening")
        wait_for_range(reopened_id, second_tip, phase_log)
        recovered = synchronize(node_id, second_tip, "resumed", initial["height"] + 1, phase_log)

        report = {"platform": platform.platform(), "tendermint": version, "chainId": chain,
                  "rustPeerId": node_id, "base": first_height, "initialRustTip": first_tip,
                  "resumedRustTip": second_tip, "initialGoHead": initial, "resumedGoHead": recovered,
                  "transactions": transfers, "retentionBeforeRestart": retained,
                  "sameRustIdAfterRestart": True, "missingBlockRejected": True}
        (args.output / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(report))
    finally:
        observer.close()
        stop(server)
        source.close()


if __name__ == "__main__":
    main()
