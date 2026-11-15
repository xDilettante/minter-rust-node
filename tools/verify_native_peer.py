"""Check the Rust block reactor against a real Tendermint 0.34 Go node."""

import argparse
import base64
import hashlib
import json
import platform
import socket
import subprocess
import time
import urllib.parse
from pathlib import Path

from verify_mainnet_empty import read_varint
from verify_send_devnet import (
    HIDE_WINDOW,
    HTTP,
    allocate_ports,
    logs,
    require,
    set_toml,
    start,
    stop,
    wait_for,
)


def rpc(port, method, **params):
    query = "?" + urllib.parse.urlencode(params) if params else ""
    with HTTP.open(f"http://127.0.0.1:{port}/{method}{query}", timeout=8) as response:
        body = json.load(response)
    require("result" in body, f"RPC {method} failed: {body}")
    return body["result"]


def cli_run(cli, *args):
    return subprocess.run(
        [cli, "peer", *args], capture_output=True, text=True,
        creationflags=HIDE_WINDOW, timeout=35,
    )


def success(process):
    require(process.returncode == 0, process.stderr)
    return json.loads(process.stdout)


def refused(process, message):
    require(process.returncode != 0 and message in process.stderr, process.stderr)
    return process.stderr.strip()


def handshake_deadline(cli, peer_id, chain):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(4)
        port = listener.getsockname()[1]
        before = time.monotonic()
        process = subprocess.Popen(
            [cli, "peer", "status", "--peer", f"{peer_id}@127.0.0.1:{port}",
             "--chain", chain, "--timeout-secs", "1"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            creationflags=HIDE_WINDOW,
        )
        try:
            connection, _ = listener.accept()
            with connection:
                _, error = process.communicate(timeout=5)
                elapsed = time.monotonic() - before
                require(process.returncode != 0 and "Secret Connection handshake" in error,
                        error)
                require(elapsed < 4, f"handshake deadline exceeded: {elapsed:.2f}s")
                return round(elapsed, 3)
        finally:
            stop(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("minterctl", type=lambda path: Path(path).resolve())
    parser.add_argument("tendermint", type=lambda path: Path(path).resolve())
    parser.add_argument("output", type=lambda path: Path(path).resolve())
    args = parser.parse_args()
    out, cli, tm = args.output, args.minterctl, args.tendermint
    out.mkdir()
    home = out / "go"
    with (out / "init.log").open("wb") as log:
        subprocess.run([tm, "init", "--home", home], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=15)
    genesis = json.loads((home / "config/genesis.json").read_bytes())
    chain = genesis["chain_id"]
    peer_id = subprocess.check_output(
        [tm, "show-node-id", "--home", home], text=True,
        creationflags=HIDE_WINDOW, timeout=15,
    ).strip()
    config = home / "config/config.toml"
    config.write_text(set_toml(logs(config), "consensus", {"timeout_commit": '"500ms"'}),
                      encoding="utf-8")
    rpc_port, peer_port = allocate_ports(2)
    endpoint = f"{peer_id}@127.0.0.1:{peer_port}"
    connection = ["--peer", endpoint, "--chain", chain]
    tm_log = out / "tendermint.log"
    process = start(
        [tm, "node", "--home", home, "--proxy_app", "kvstore",
         "--rpc.laddr", f"tcp://127.0.0.1:{rpc_port}",
         "--p2p.laddr", f"tcp://127.0.0.1:{peer_port}", "--p2p.pex=false"],
        tm_log,
    )
    try:
        wait_for(
            lambda: int(rpc(rpc_port, "status")["sync_info"]["latest_block_height"]) >= 2,
            "Go node", [(process, tm_log)],
        )
        tx = b"native-peer=" + b"v" * (70 * 1024)
        broadcast = rpc(rpc_port, "broadcast_tx_commit", tx="0x" + tx.hex())
        require(all(broadcast[phase].get("code", 0) == 0 for phase in ("check_tx", "deliver_tx")),
                f"Go rejected test transaction: {broadcast}")
        height = int(broadcast["height"])
        expected = rpc(rpc_port, "block", height=height)
        header = expected["block"]["header"]
        txs = [base64.b64decode(item, validate=True) for item in expected["block"]["data"]["txs"]]
        require(tx in txs, "Go test transaction missing")
        status = success(cli_run(cli, "status", *connection))
        require(status["peerId"] == peer_id and status["chainId"] == chain
                and status["base"] <= height <= status["height"], "unexpected peer status")
        captured = out / "block.pb"
        request = ["block", *connection, "--height", str(height), "--output", captured]
        fetched = success(cli_run(cli, *request))
        require(fetched["peer"]["peerId"] == peer_id and len(fetched["blocks"]) == 1,
                "unexpected block report")
        actual = fetched["blocks"][0]
        parts = expected["block_id"]["parts"]
        with captured.open("rb") as file:
            payload = file.read(read_varint(file))
            require(file.read(1) == b"", "multiple frames in block capture")
        require(actual == {
            "peerId": peer_id, "chainId": chain, "height": height,
            "blockHash": expected["block_id"]["hash"].lower(),
            "parts": int(parts["total"]), "partSetHash": parts["hash"].lower(),
            "appHash": header["app_hash"].lower(), "transactions": len(txs),
            "wireSha256": hashlib.sha256(payload).hexdigest(),
        } and len(payload) > len(tx) and int(parts["total"]) > 1,
                "multipart block differs from Go RPC")
        original = captured.read_bytes()
        refused(cli_run(cli, *request), "publish block response")
        require(captured.read_bytes() == original, "existing block capture changed")
        incomplete = out / "incomplete.pb"
        missing = str(status["height"] + 10000)
        errors = {
            "wrongId": refused(
                cli_run(cli, "status", "--peer", f"{'0' * 40}@127.0.0.1:{peer_port}", "--chain", chain),
                "public key does not match configured ID",
            ),
            "wrongChain": refused(
                cli_run(cli, "status", "--peer", endpoint, "--chain", "minter-wrong-network"),
                "unexpected peer chain",
            ),
            "noBlock": refused(
                cli_run(cli, "block", *connection, "--height", str(height),
                        "--height", missing, "--output", incomplete),
                "peer has no block at height",
            ),
        }
        require(not incomplete.exists(), "failed block batch published a file")
        timeout = handshake_deadline(cli, peer_id, chain)
    finally:
        stop(process)
    result = {
        "platform": platform.platform(),
        "engine": subprocess.check_output([tm, "version"], text=True, timeout=10,
                                          creationflags=HIDE_WINDOW).strip(),
        "peer": status,
        "block": actual,
        "payloadBytes": len(payload),
        "errors": errors,
        "handshakeTimeoutSeconds": timeout,
    }
    (out / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
