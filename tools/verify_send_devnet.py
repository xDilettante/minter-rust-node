"""Run two local Tendermint 0.34.19 nodes with Rust and Go Minter ABCI apps."""

import argparse
import base64
import hashlib
import json
import platform
import re
import shutil
import socket
import subprocess
import time
import urllib.parse
import urllib.request
from pathlib import Path


HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))
HIDE_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def allocate_ports(count=6):
    sockets = [socket.socket() for _ in range(count)]
    try:
        for listener in sockets:
            listener.bind(("127.0.0.1", 0))
        return [listener.getsockname()[1] for listener in sockets]
    finally:
        for listener in sockets:
            listener.close()


def logs(path):
    return path.read_text(encoding="utf-8", errors="replace")


def start(command, path):
    with path.open("wb") as output:
        return subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT,
                                creationflags=HIDE_WINDOW)


def stop(process):
    if process and process.poll() is None:
        process.kill()
        process.wait(timeout=15)


def wait_for(predicate, label, processes, timeout=45):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        for process, path in processes:
            if process.poll() is not None:
                raise RuntimeError(f"{path.name} exited {process.returncode}\n{logs(path)[-4000:]}")
        try:
            result = predicate()
            if result:
                return result
        except (OSError, ValueError, RuntimeError) as error:
            last = error
        time.sleep(0.15)
    raise RuntimeError(f"Timed out waiting for {label}: {last}")


def set_toml(config, section, values):
    header = re.search(rf"(?m)^\[{re.escape(section)}\][ \t]*\n", config)
    require(header is not None, f"missing config section {section}")
    tail = config[header.end():]
    boundary = re.search(r"(?m)^\[", tail)
    end = boundary.start() if boundary else len(tail)
    body = tail[:end]
    for key, value in values.items():
        body, count = re.subn(rf"(?m)^{re.escape(key)}[ \t]*=.*$", f"{key} = {value}", body)
        require(count == 1, f"missing config key {section}.{key}")
    return config[:header.end()] + body + tail[end:]


class Node:
    def __init__(self, root, tendermint, app, fixture_dir, ports, validator):
        root.mkdir()
        self.root = root
        self.home = root / "tm"
        self.engine, self.app = tendermint, app
        self.abci, self.rpc_port, self.p2p = ports
        self.validator = validator
        self.application_process = self.engine_process = None
        self.processes = []
        with (root / "init.log").open("wb") as log:
            subprocess.run([self.engine, "init", "--home", self.home], check=True, timeout=20,
                           stdout=log, stderr=subprocess.STDOUT, creationflags=HIDE_WINDOW)
        shutil.copyfile(fixture_dir / "genesis.json", self.home / "config/genesis.json")
        if validator:
            for filename, directory in (("priv_validator_key.json", "config"),
                                        ("priv_validator_state.json", "data")):
                shutil.copyfile(fixture_dir / filename, self.home / directory / filename)
        self.node_id = subprocess.check_output(
            [self.engine, "show-node-id", "--home", self.home], text=True, timeout=15,
            creationflags=HIDE_WINDOW).strip()
        config_path = self.home / "config/config.toml"
        config = set_toml(logs(config_path), "p2p",
                          {"allow_duplicate_ip": "true", "addr_book_strict": "false"})
        config = set_toml(config, "consensus", {"timeout_commit": '"500ms"'})
        config_path.write_text(config, encoding="utf-8")

    def launch(self, peer, phase, *, use_rust=None, app_args=(), engine_args=()):
        abci = f"127.0.0.1:{self.abci}"
        use_rust = self.validator if use_rust is None else use_rust
        if use_rust:
            command = [self.app, "--db", self.root / "state", "--abci-listen", abci, *app_args]
        else:
            command = [self.app, "serve", self.root / "state", f"tcp://{abci}"]
        app_log = self.root / f"app-{phase}.log"
        self.application_process = start(command, app_log)
        self.processes = [(self.application_process, app_log)]
        wait_for(lambda: "ABCI 0.34 listening on" in logs(app_log), "ABCI listener", self.processes)
        tm_log = self.root / f"tm-{phase}.log"
        command = [self.engine, "node", "--home", self.home, "--abci", "socket",
                   "--proxy_app", f"tcp://{abci}",
                   "--rpc.laddr", f"tcp://127.0.0.1:{self.rpc_port}",
                   "--p2p.laddr", f"tcp://127.0.0.1:{self.p2p}",
                   "--p2p.pex=false"]
        if peer is not None:
            command += ["--p2p.persistent_peers", f"{peer.node_id}@127.0.0.1:{peer.p2p}"]
        command += engine_args
        self.engine_process = start(command, tm_log)
        self.processes.append((self.engine_process, tm_log))

    def close(self):
        stop(self.engine_process)
        stop(self.application_process)
        self.processes = []

    def rpc(self, method, **args):
        query = "?" + urllib.parse.urlencode(args) if args else ""
        with HTTP.open(f"http://127.0.0.1:{self.rpc_port}/{method}{query}", timeout=12) as response:
            body = json.load(response)
        if "error" in body:
            raise RuntimeError(body["error"])
        return body["result"]

    def height(self):
        return int(self.rpc("status")["sync_info"]["latest_block_height"])

    def balance(self, address, height, coin=0):
        data = address if coin == 0 else address + coin.to_bytes(4, "little").hex()
        reply = self.rpc("abci_query", path='"/balance"', data=f"0x{data}", height=height)["response"]
        require(reply.get("code", 0) == 0 and int(reply["height"]) == height, "balance query failed")
        return json.loads(base64.b64decode(reply["value"], validate=True))


def signed_root(rust, go, height):
    signed = [node.rpc("commit", height=height)["signed_header"] for node in (rust, go)]
    headers = [item["header"] for item in signed]
    require(headers[0] == headers[1] and signed[0]["commit"] == signed[1]["commit"],
            f"signed headers differ at {height}")
    require(int(headers[0]["height"]) == height and bool(headers[0]["app_hash"]),
            f"missing AppHash at {height}")
    require(any(sig["block_id_flag"] == 2 and sig.get("signature")
                for sig in signed[0]["commit"]["signatures"]), f"unsigned block at {height}")
    return {"height": height, "appHash": headers[0]["app_hash"].lower(),
            "blockHash": signed[0]["commit"]["block_id"]["hash"].lower()}


def receipt(rust, go, raw, gas=16):
    tx_hash = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
    children = rust.processes + go.processes
    receipts = [wait_for(lambda: node.rpc("tx", hash=f"0x{tx_hash}"), "indexed transfer", children)
                for node in (rust, go)]
    first, second = receipts
    require(first == second, "Go and Rust indexed different transaction results")
    require(first["tx_result"].get("code", 0) == 0 and int(first["tx_result"]["gas_used"]) == gas,
            "transfer failed or returned unexpected gas")
    height = int(first["height"])
    wait_for(lambda: min(rust.height(), go.height()) >= height + 2, "signed successor", children)
    return {"height": height, "hash": tx_hash, "result": first["tx_result"],
            "signedSuccessor": signed_root(rust, go, height + 1)}


def verify(args):
    args.output.mkdir(parents=True)
    version = subprocess.check_output([args.tendermint, "version"], text=True, timeout=15,
                                      creationflags=HIDE_WINDOW).strip()
    require(version == "0.34.19", f"expected Tendermint 0.34.19, found {version}")
    fixture = json.loads(logs(args.fixture / "go-abci-v034.json"))
    first_height = fixture["genesisHeight"] + 1
    transfers = fixture["transactions"]
    sender, receiver = (item["address"] for item in fixture["blocks"][0]["accounts"][:2])
    ports = allocate_ports()
    rust = Node(args.output / "rust", args.tendermint, args.rust, args.fixture, ports[:3], True)
    go = Node(args.output / "go", args.tendermint, args.go, args.fixture, ports[3:], False)
    try:
        rust.launch(go, "initial")
        go.launch(rust, "initial")
        children = rust.processes + go.processes
        wait_for(lambda: min(rust.height(), go.height()) >= first_height + 2,
                 "genesis and empty blocks on both nodes", children)
        require(int(rust.rpc("status")["validator_info"]["voting_power"]) == 100000000,
                "fixture validator is not active")
        require(int(go.rpc("status")["validator_info"]["voting_power"]) == 0,
                "Go observer unexpectedly signs")
        genesis_block = rust.rpc("block", height=first_height)["block"]
        require(not genesis_block["data"]["txs"], "first block was not empty")
        genesis_root = signed_root(rust, go, first_height + 1)
        require(genesis_root["appHash"] == fixture["blocks"][0]["appHash"],
                "first empty block differs from the deterministic Go fixture")

        broadcast = go.rpc("broadcast_tx_sync", tx="0x" + transfers["transfer"])
        require(broadcast.get("code", 0) == 0, "Go observer rejected transfer")
        first = receipt(rust, go, transfers["transfer"])
        first_balances = [rust.balance(address, first["height"]) for address in (sender, receiver)]
        require(first_balances == [
            {"balance": "29999999999999999968", "nonce": 1},
            {"balance": "10000000000000000000", "nonce": 0},
        ], "transfer balances differ")

        restart_height = rust.height()
        rust.close()
        rust.launch(go, "restarted")
        children = rust.processes + go.processes
        wait_for(lambda: min(rust.height(), go.height()) > restart_height,
                 "Rust resumed from the existing RocksDB", children)
        require([rust.balance(address, first["height"]) for address in (sender, receiver)]
                == first_balances, "historical balances changed after restart")
        accepted = rust.rpc("broadcast_tx_commit", tx="0x" + transfers["self"])
        require(accepted["check_tx"].get("code", 0) == accepted["deliver_tx"].get("code", 0) == 0,
                "Rust rejected post-restart transfer")
        second = receipt(rust, go, transfers["self"])
        sender_after = rust.balance(sender, second["height"])
        require(sender_after == {"balance": "29999999999999999956", "nonce": 2},
                "post-restart balance or nonce differs")
        result = {
            "os": platform.platform(), "tendermint": version, "chainId": fixture["chainId"],
            "firstEmptyBlock": genesis_root, "gossipedTransfer": first,
            "restartAtHeight": restart_height, "postRestartTransfer": second,
            "firstBalances": first_balances, "postRestartSender": sender_after,
        }
        last_height = second["height"]
        if "multisig-batch" in transfers:
            broadcast = go.rpc("broadcast_tx_sync", tx="0x" + transfers["multisig-batch"])
            require(broadcast.get("code", 0) == 0, "Go observer rejected the multisig batch")
            batch = receipt(rust, go, transfers["multisig-batch"], gas=57)
            last_height = batch["height"]
            expected = fixture["blocks"][-1]["accounts"][-1]
            balance = rust.balance(expected["address"], last_height)
            require(balance == {"balance": expected["balance"], "nonce": expected["nonce"]},
                    "multisig balance or nonce differs from Go")
            result.update({"gossipedMultisig": batch, "multisigBalance": balance})
        if "create-token" in transfers:
            broadcast = go.rpc("broadcast_tx_sync", tx="0x" + transfers["create-token"])
            require(broadcast.get("code", 0) == 0, "Go observer rejected token creation")
            created = receipt(rust, go, transfers["create-token"], gas=18)
            last_height = created["height"]
            attributes = created["result"]["events"][0]["attributes"]
            tags = {base64.b64decode(item["key"]).decode(): base64.b64decode(item["value"]).decode()
                    for item in attributes}
            issuer = tags["tx.from"]
            coin = int(tags["tx.coin_id"])
            require(tags["tx.coin_symbol"] == "ABCT" and tags["tx.burned_for_symbol"] == "194",
                    "ticker or ticker fee differs")
            initial_balance = rust.balance(issuer, last_height, coin)
            burned = rust.balance("00" * 20, last_height)
            require(initial_balance == {"balance": "5000000000000000000", "nonce": 1}
                    and burned == {"balance": "194", "nonce": 0},
                    "issued token balance or ticker charge differs")
            result.update({"gossipedCreation": created, "initialTokenBalance": initial_balance,
                           "tickerBurnAddressBalance": burned})
        roots = [signed_root(rust, go, height) for height in range(first_height + 1, last_height + 2)]
        result["signedRoots"] = roots
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"os": result["os"], "signedRoots": len(roots),
                          "transferHeight": first["height"], "postRestartHeight": second["height"],
                          "multisigHeight": result.get("gossipedMultisig", {}).get("height"),
                          "creationHeight": result.get("gossipedCreation", {}).get("height")}))
    finally:
        rust.close()
        go.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("rust", "go", "tendermint", "fixture", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    for name in ("rust", "go", "tendermint", "fixture"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    verify(args)


if __name__ == "__main__":
    main()
