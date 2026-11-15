"""Relay signed Minter transfers between Go Tendermint and a native Rust observer."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import platform
import re
import subprocess
from types import SimpleNamespace

from verify_minter_api_v2 import get, grpc, listen
from verify_mainnet_empty import varint
from verify_native_peer import cli_run, success
from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, set_toml, signed_root, start, stop, wait_for


def verify(args):
    args.output.mkdir()
    require(subprocess.check_output([args.tendermint, "version"], text=True, timeout=15,
                                    creationflags=HIDE_WINDOW).strip() == "0.34.19",
            "unexpected Tendermint version")
    ports = allocate_ports(9)
    validator, ingress = [
        Node(args.output / name, args.tendermint, args.go_app, args.fixture, ports[i:i + 3], i == 0)
        for name, i in (("validator", 0), ("ingress", 3))
    ]
    p2p, http, grpc_port = ports[-3:]
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    txs = fixture["transactions"]
    chain = fixture["chainId"]
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    genesis["consensus_params"]["version"] = {"app_version": "8"}
    genesis_file = args.output / "genesis.json"
    genesis_file.write_text(json.dumps(genesis), encoding="utf-8")
    for go in (validator, ingress):
        (go.home / "config/genesis.json").write_bytes(genesis_file.read_bytes())
    if args.shared:
        config = validator.home / "config/config.toml"
        config.write_text(set_toml(logs(config), "p2p", {"allow_duplicate_ip": "false"}), encoding="utf-8")
    database = args.output / "rust-state"
    key_id = None
    rust = None

    def peer_status():
        return success(cli_run(args.cli, "status", "--chain", chain,
                               "--peer", f"{key_id}@{listen(p2p)}"))

    def status():
        reply = get(http, "/v2/status")
        require(reply["status"] == 200 and reply["body"]["network"] == chain, reply)
        return reply["body"]

    def submit_http(name, processes):
        def request():
            reply = get(http, "/v2/send_transaction/0x" + txs[name])
            return reply if reply["status"] != 503 else None
        return wait_for(request, f"native HTTP submission of {name}", processes)

    def only_rust_peers(outbound=False):
        for go in (validator, ingress):
            peers = go.rpc("net_info")["peers"]
            other = ingress if go is validator else validator
            require(all(peer["node_info"]["id"] != other.node_id for peer in peers),
                    f"{go.root.name} has a direct path to the other Go node")
            rust_links = [peer for peer in peers if peer["node_info"]["id"] == key_id]
            require(len(rust_links) == 1, f"{go.root.name} lacks the stable Rust link")
            if go is ingress:
                require(rust_links[0]["is_outbound"] != outbound,
                        "Go ingress connected in the wrong direction")
            if args.shared and go is validator:
                require(len(peers) == 1 and not rust_links[0]["is_outbound"],
                        "the Go validator did not use a single connection for blocks and mempool")
        return True

    def launch(name, *, initial, dial_ingress):
        nonlocal rust, key_id
        path = args.output / f"{name}.log"
        flags = ["--genesis", genesis_file, "--keep-iavl-versions", "4"] if initial else []
        if dial_ingress:
            flags += ["--mempool-peer", f"{ingress.node_id}@{listen(ingress.p2p)}"]
        if args.shared:
            flags += ["--mempool-peer", f"{validator.node_id}@{listen(validator.p2p)}"]
        rust = start([args.daemon, "native-sync", "--db", database, *flags,
                      "--p2p-listen", listen(p2p), "--api-listen", listen(http),
                      "--grpc-listen", listen(grpc_port), "--mempool", "--trace",
                      "--peer", f"{validator.node_id}@{listen(validator.p2p)}", "--follow",
                      "--timeout-secs", "5"], path)
        processes = validator.processes + [(rust, path)]
        ready = wait_for(lambda: re.search(r"Tendermint 0\.34 P2P listening on ([0-9a-f]{40})@", logs(path)),
                         "Rust native listener", processes)
        if key_id is not None:
            require(key_id == ready.group(1), "Rust changed its P2P identity after restart")
        key_id = ready.group(1)
        return path, processes

    def indexed(name, processes):
        hash_ = hashlib.sha256(bytes.fromhex(txs[name])).hexdigest()
        first = wait_for(lambda: validator.rpc("tx", hash="0x" + hash_),
                         f"{name} committed by the Go validator", processes)
        require(first["tx_result"].get("code", 0) == 0, f"Go rejected {name}: {first}")
        height = int(first["height"])
        wait_for(lambda: int(status()["latest_block_height"]) >= height,
                 "Rust committed the gossiped transaction", processes)
        signed = validator.rpc("commit", height=height + 1)["signed_header"]
        require(any(sig["block_id_flag"] == 2 for sig in signed["commit"]["signatures"]),
                "missing validator signature")
        result = first["tx_result"]
        encoded = bytearray()
        def payload(field, value):
            return varint(field << 3 | 2) + varint(len(value)) + value
        for field, value in ((1, result["code"]), (5, int(result["gas_wanted"])),
                             (6, int(result["gas_used"]))):
            if value:
                encoded += varint(field << 3) + varint(value)
        for event in result["events"]:
            item = payload(1, event["type"].encode())
            for attribute in event["attributes"]:
                tags = payload(1, base64.b64decode(attribute["key"])) + payload(2, base64.b64decode(attribute["value"]))
                if attribute["index"]:
                    tags += b"\x18\x01"
                item += payload(2, tags)
            encoded += payload(7, item)
        return {"height": height, "hash": hash_, "result": first["tx_result"],
                "deliverResponsesSha256": hashlib.sha256(varint(len(encoded)) + encoded).hexdigest(),
                "appHash": signed["header"]["app_hash"].lower()}

    def compare_trace(path, results):
        rows = {row["height"]: row for row in decode_trace(path) if "height" in row}
        for receipt in results:
            row = rows[receipt["height"]]
            require(row["appHash"] == receipt["appHash"] and row["transactions"] == 1
                    and row["deliverResponsesSha256"] == receipt["deliverResponsesSha256"],
                    f"Rust receipt differs from the signed Go result at {row['height']}")

    try:
        validator.launch(None, "validator", use_rust=False)
        wait_for(lambda: validator.height() >= fixture["genesisHeight"] + 6,
                 "signed Go blocks", validator.processes)
        first_log, processes = launch("rust-inbound", initial=True, dial_ingress=False)
        rust_peer = SimpleNamespace(node_id=key_id, p2p=p2p)
        wait_for(lambda: peer_status()["height"] >= fixture["genesisHeight"] + 6,
                 "Rust archive has blocks for the Go observer", processes)
        ingress.launch(rust_peer, "observer", use_rust=False)
        processes += ingress.processes
        wait_for(lambda: ingress.height() >= fixture["genesisHeight"] + 4 and not status()["catching_up"],
                 "Go observer synced through Rust", processes)
        if not args.shared:
            validator.close()
            validator.launch(rust_peer, "gossip", use_rust=False)
            processes = validator.processes + ingress.processes + [(rust, first_log)]
        wait_for(only_rust_peers, "Go nodes connected through Rust", processes)
        wait_for(lambda: not ingress.rpc("status")["sync_info"]["catching_up"],
                 "Go ingress switched to consensus", processes)

        rejected = submit_http("wrong-chain", processes)
        require(rejected["status"] == 400 and "Wrong chain id" in json.dumps(rejected["body"]), rejected)
        incoming = ingress.rpc("broadcast_tx_sync", tx="0x" + txs["transfer"])
        require(incoming.get("code", 0) == 0, f"Go ingress rejected transfer: {incoming}")
        go_to_go = indexed("transfer", processes)
        repeated = submit_http("transfer", processes)
        require(repeated["status"] == 409, "included transfer was admitted again")
        submitted = submit_http("self", processes)
        require(submitted["status"] == 200 and submitted["body"]["hash"] == "Mt" +
                hashlib.sha256(bytes.fromhex(txs["self"])).hexdigest(), submitted)
        native_http = indexed("self", processes)
        compare_trace(first_log, [go_to_go, native_http])
        before = peer_status()
        signed_root(validator, ingress, fixture["genesisHeight"] + 3)

        stop(rust)
        ingress.close()
        ingress.launch(None, "dialed-by-rust", use_rust=False)
        second_log, processes = launch("rust-reopened", initial=False, dial_ingress=True)
        processes += ingress.processes
        wait_for(lambda: only_rust_peers(outbound=True), "Rust dialed the Go ingress", processes)
        wait_for(lambda: int(status()["latest_block_height"]) > before["height"], "Rust resumed its archive", processes)
        expired = submit_http("self", processes)
        require(expired["status"] == 400 and "nonce" in json.dumps(expired["body"]).lower(),
                "restart accepted a transaction with a spent nonce")
        outbound = ingress.rpc("broadcast_tx_sync", tx="0x" + txs["multisig-batch"])
        require(outbound.get("code", 0) == 0, outbound)
        outbound_relay = indexed("multisig-batch", processes)
        if args.oracle:
            def submit_grpc():
                reply, = grpc(args.oracle, grpc_port,
                              [{"Method": "SendTransaction", "Tx": "0x" + txs["batch"]}],
                              args.output / "grpc-submit.json")
                return reply if reply["code"] != 14 else None
            reply = wait_for(submit_grpc, "native gRPC submission", processes)
            require(reply["code"] == 0 and reply["body"]["hash"] == "Mt" +
                    hashlib.sha256(bytes.fromhex(txs["batch"])).hexdigest(), reply)
        else:
            reply = submit_http("batch", processes)
            require(reply["status"] == 200, reply)
        after_restart = indexed("batch", processes)
        compare_trace(second_log, [outbound_relay, after_restart])
        result = {"platform": platform.platform(), "sharedBlockMempoolConnection": args.shared,
                  "goToRustToGo": go_to_go, "nativeHttp": native_http,
                  "outboundRelay": outbound_relay, "afterRestart": after_restart,
                  "usedGrpc": bool(args.oracle), "rustPeerId": key_id,
                  "rustReopenedAbove": before["height"], "wrongChainRejected": True,
                  "duplicateRejected": True, "spentNonceRejectedAfterRestart": True}
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result))
    finally:
        stop(rust)
        ingress.close()
        validator.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "tendermint", "go_app", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--oracle", type=lambda value: Path(value).resolve())
    parser.add_argument("--shared", action="store_true",
                        help="use one connection to the Go validator for block sync and transaction relay")
    verify(parser.parse_args())
