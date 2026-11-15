"""Verify light sync, witnessed headers and proved balances over Tendermint 0.34 RPC."""

import argparse
import base64
import json
import platform
import socket
import subprocess
import threading
import urllib.parse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from verify_send_devnet import (
    HIDE_WINDOW, Node, allocate_ports, logs, receipt, require, signed_root, wait_for,
)


@contextmanager
def proxy(node, mutate):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            request = urllib.parse.urlsplit(self.path)
            method = request.path.lstrip("/")
            params = dict(urllib.parse.parse_qsl(request.query))
            try:
                result = node.rpc(method, **params)
                mutate(method, result)
                data = json.dumps({"jsonrpc": "2.0", "id": -1, "result": result}).encode()
            except Exception as error:
                data = json.dumps({"jsonrpc": "2.0", "id": -1, "error": {
                    "code": -32603, "message": str(error)}}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}"
        finally:
            server.shutdown()
            thread.join()


def damage_signature(method, result):
    if method == "commit":
        signatures = result["signed_header"]["commit"]["signatures"]
        signature = next(s for s in signatures if s.get("signature"))
        raw = base64.b64decode(signature["signature"], validate=True)
        signature["signature"] = base64.b64encode(bytes([raw[0] ^ 1]) + raw[1:]).decode()


def damage_proof(field):
    def mutate(method, result):
        if method != "abci_query":
            return
        response = result["response"]
        if field == "height":
            response["height"] = str(int(response["height"]) + 1)
        elif field in ("key", "value"):
            response[field] = base64.b64encode(b"forged").decode()
        elif field == "missing":
            response.pop("proofOps")
        elif field == "bytes":
            op = response["proofOps"]["ops"][0]
            raw = base64.b64decode(op["data"], validate=True)
            op["data"] = base64.b64encode(raw[:-1] + bytes([raw[-1] ^ 1])).decode()
    return mutate


def damage_tx(field):
    def mutate(method, result):
        if method == "block_results" and field == "execution":
            result["txs_results"][0]["code"] = 99
        if method != "tx":
            return
        if field == "result":
            result["tx_result"]["code"] = 99
        elif field == "bytes":
            result["tx"] = base64.b64encode(b"forged").decode()
        elif field == "leaf":
            result["proof"]["proof"]["leaf_hash"] = base64.b64encode(bytes(32)).decode()
    return mutate


def verify(args):
    args.output.mkdir(parents=True)
    nodes = []
    ports = allocate_ports(9)
    config = json.loads(logs(args.fixture / "genesis.json"))
    config["consensus_params"]["version"] = {"app_version": "8"}
    fixture = json.loads(logs(args.fixture / "go-abci-v034.json"))
    db = args.output / "light.redb"

    def command(*argv, expect_error=None):
        call = subprocess.run([args.cli, "light", *argv], capture_output=True, text=True,
                              timeout=60, creationflags=HIDE_WINDOW)
        if expect_error:
            require(call.returncode != 0 and expect_error.lower() in call.stderr.lower(),
                    f"expected {expect_error!r}, received {call.stdout!r} {call.stderr!r}")
            return None
        require(call.returncode == 0, call.stderr)
        return json.loads(call.stdout)

    def url(node):
        return f"http://127.0.0.1:{node.rpc_port}"

    try:
        for index, (name, binary) in enumerate((("go-primary", args.go), ("go-witness", args.go),
                                                ("rust", args.rust))):
            node = Node(args.output / name, args.tendermint, binary, args.fixture,
                        ports[3 * index:3 * index + 3], index == 0)
            nodes.append(node)
            (node.home / "config/genesis.json").write_text(json.dumps(config), encoding="utf-8")
        source, witness, rust = nodes
        source.launch(witness, "initial", use_rust=False)
        witness.launch(source, "initial")
        rust.launch(source, "initial", use_rust=True)
        children = [item for node in nodes for item in node.processes]
        initial_height = int(config["initial_height"])
        anchor_height = initial_height + 3
        wait_for(lambda: min(node.height() for node in nodes) >= anchor_height + 5,
                 "signed starting header and validator sets", children, timeout=75)
        signed = signed_root(source, witness, anchor_height)
        anchor = {"chainId": config["chain_id"], "height": str(anchor_height),
                  "hash": signed["blockHash"].upper(), "initialHeight": str(initial_height),
                  "trustingPeriodSecs": 3600}
        anchor_path = args.output / "anchor.json"
        anchor_path.write_text(json.dumps(anchor, indent=2) + "\n", encoding="utf-8")
        peer_args = ["--primary", url(source), "--witness", url(witness)]
        init = ["init", "--db", db, "--anchor", anchor_path, *peer_args]
        bad = args.output / "invalid-anchor.json"
        bad.write_text(json.dumps({**anchor, "hash": "00" * 32}), encoding="utf-8")
        command("init", "--db", args.output / "bad-hash.redb", "--anchor", bad, *peer_args,
                expect_error="trusted header hash")
        bad.write_text(json.dumps({**anchor, "chainId": "different-chain"}), encoding="utf-8")
        command("init", "--db", args.output / "bad-chain.redb", "--anchor", bad, *peer_args,
                expect_error="chain ID")
        bad.write_text(json.dumps({**anchor, "trustingPeriodSecs": 1}), encoding="utf-8")
        command("init", "--db", args.output / "expired.redb", "--anchor", bad, *peer_args,
                expect_error="expired trusted header")
        command(*init[:-1], url(source), expect_error="distinct Tendermint nodes")
        with proxy(source, lambda *_: None) as endpoint:
            command(*init[:-1], endpoint, expect_error="distinct Tendermint nodes")
        start_status = command(*init)
        require(start_status["height"] == anchor_height and not start_status["stale"], "invalid bootstrap status")

        raw = fixture["transactions"]["transfer"]
        require(source.rpc("broadcast_tx_sync", tx="0x" + raw).get("code", 0) == 0, "Go rejected transfer")
        tx = receipt(rust, source, raw)
        height = tx["height"]
        target = height + 1
        wait_for(lambda: witness.height() > target, "witness successor validators", children)
        head = command("sync", "--db", db, *peer_args, "--height", str(target))
        require(head["height"] == target and head["appHash"] == tx["signedSuccessor"]["appHash"],
                "verified header differs from signed successor")
        require(command("status", "--db", db) == head, "verified head did not survive process exit")
        historical = command("sync", "--db", db, *peer_args, "--height", str(height))
        require(historical["height"] == height and command("status", "--db", db) == head,
                "adding a historical signed header changed the verified head")
        tx_query = ["tx", "--db", db, "--height", str(height), "--hash", tx["hash"], "--rpc"]
        inclusion = command(*tx_query, url(source))
        data_hash = source.rpc("block", height=height)["block"]["header"]["data_hash"].lower()
        require(inclusion == command(*tx_query, url(rust)) and inclusion["included"]
                and inclusion["dataHash"] == data_hash, "transaction inclusion differs from Go")
        require("execution" not in inclusion, "claimed execution with an inclusion proof only")
        raw_results = source.rpc("block_results", height=height)
        (args.output / "block-results-go.json").write_text(json.dumps(raw_results, indent=2) + "\n", encoding="utf-8")
        execution = command(*tx_query, url(source), "--result")
        require(execution == command(*tx_query, url(rust), "--result")
                and execution["execution"]["code"] == 0
                and execution["execution"]["gasUsed"] == 16,
                "verified execution differs from Go")
        successor = source.rpc("block", height=target)["block"]["header"]
        require(execution["execution"]["resultsHash"] == successor["last_results_hash"].lower(),
                "verified execution hash differs from the signed successor")
        with proxy(source, damage_tx("result")) as endpoint:
            require(command(*tx_query, endpoint) == inclusion, "used an unauthenticated execution result")
        with proxy(source, damage_tx("bytes")) as endpoint:
            command(*tx_query, endpoint, expect_error="transaction bytes differ")
        with proxy(source, damage_tx("leaf")) as endpoint:
            command(*tx_query, endpoint, expect_error="invalid Tendermint transaction leaf")
        with proxy(source, damage_tx("execution")) as endpoint:
            command(*tx_query, endpoint, "--result", expect_error="execution results root differs")

        sender, receiver = [a["address"] for a in fixture["blocks"][0]["accounts"][:2]]
        cases = [(sender, 0, "29999999999999999968", 1), (receiver, 0, "10000000000000000000", 0),
                 (sender, 4000, "0", 1), ("00" * 20, 0, "0", 0)]
        balances = []
        for address, coin, amount, nonce in cases:
            cmd = ["balance", "--db", db, "--height", str(height),
                   "--address", "Mx" + address, "--coin", str(coin), "--rpc"]
            go_value, rust_value = (command(*cmd, url(node)) for node in (source, rust))
            require(go_value == rust_value and go_value["balance"] == amount and go_value["nonce"] == nonce
                    and go_value["headerHeight"] == target, "wrong proved balance")
            balances.append(go_value)

        balance_query = ["balance", "--db", db, "--height", str(height), "--address", sender, "--rpc"]
        errors = {"height": "another proof height", "key": "another proof height",
                  "value": "differs from IAVL proof", "missing": "no IAVL proof", "bytes": "proof root differs"}
        for field, error in errors.items():
            with proxy(source, damage_proof(field)) as endpoint:
                command(*balance_query, endpoint, expect_error=error)

        next_target = target + 1
        wait_for(lambda: min(node.height() for node in nodes) > next_target, "new signed block", children)
        with proxy(witness, damage_signature) as endpoint:
            command("sync", "--db", db, "--primary", url(source), "--witness", endpoint,
                    "--height", str(next_target), expect_error="light verification failed")
        signed = source.rpc("commit", height=next_target)["signed_header"]
        alternate = subprocess.run([args.go, "alternate-header"], input=json.dumps(signed),
                                   capture_output=True, text=True, timeout=15, creationflags=HIDE_WINDOW)
        require(alternate.returncode == 0, alternate.stderr)
        alternate = json.loads(alternate.stdout)

        def switch_header(method, result):
            if method == "commit" and int(result["signed_header"]["header"]["height"]) == next_target:
                result["signed_header"] = alternate

        with proxy(witness, switch_header) as endpoint:
            command("sync", "--db", db, "--primary", url(source), "--witness", endpoint,
                    "--height", str(next_target), expect_error="different signed headers")
        with socket.socket() as unavailable:
            unavailable.bind(("127.0.0.1", 0))
            command("sync", "--db", db, "--primary", url(source), "--witness",
                    f"http://127.0.0.1:{unavailable.getsockname()[1]}", "--height", str(next_target),
                    expect_error="witness RPC")
        require(command("status", "--db", db) == head, "failed sync changed the trusted head")
        advanced = command("sync", "--db", db, *peer_args, "--height", str(next_target))
        require(advanced["height"] == next_target, "sync did not advance after witness recovered")
        require(command(*balance_query, url(source)) == balances[0], "historical proof failed after sync")
        result = {"os": platform.platform(), "initial": start_status, "verified": head,
                  "advanced": advanced, "balances": balances, "transactionInclusion": inclusion,
                  "transactionExecution": execution["execution"],
                  "proofAttacksRejected": list(errors), "transactionAttacksRejected": ["bytes", "leaf", "execution"],
                  "badSignatureRejected": True, "conflictingSignedHeaderRejected": True,
                  "unavailableWitnessRejected": True,
                  "expiredCheckpointRejected": True, "sameNodeWitnessRejected": True}
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result))
    finally:
        for node in reversed(nodes):
            node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("cli", "rust", "go", "tendermint", "fixture", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    for name in ("cli", "rust", "go", "tendermint", "fixture", "output"):
        setattr(args, name, getattr(args, name).resolve())
    verify(args)
