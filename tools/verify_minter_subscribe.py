"""Compare live Minter v2 subscriptions on a Go validator and a Rust observer."""

import argparse
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import platform
import time
from types import SimpleNamespace
from urllib.parse import urlencode

from verify_minter_api_v2 import get, listen, rpc
from verify_send_devnet import Node, allocate_ports, logs, require, set_toml, signed_root, start, stop, wait_for

TRANSPORTS = ("grpc", "http", "ws")
BLOCKS = "tm.event = 'NewBlock'"
IDLE = "tm.event = 'UnusedEvent'"


@dataclass
class Probe:
    process: object
    log: Path
    transport: str

    def rows(self):
        content = logs(self.log).rpartition("\n")[0]
        return [json.loads(line) for line in content.splitlines()]

    def result(self):
        require(self.process.wait(timeout=85) == 0, logs(self.log))
        result = {"messages": []}
        for row in self.rows():
            if "status" in row:
                result["httpStatus"] = row["status"]
            if "body" in row:
                body = row["body"]
                if "error" in body:
                    result["error"] = body["error"]
                else:
                    message = body if self.transport == "grpc" else body["result"]
                    message["events"].sort(key=lambda event: event["key"])
                    result["messages"].append(message)
            elif row.get("code"):
                result["error"] = {"code": row["code"], "message": row["message"], "details": []}
        return result


class Probes:
    def __init__(self, oracle, output):
        self.oracle = oracle
        self.output = output
        self.running = []

    def start(self, label, transport, ports, query, count=1, *, params=None):
        if transport == "grpc":
            address = listen(ports.grpc)
        else:
            params = urlencode({"query": query}) if params is None else params
            scheme = "ws" if transport == "ws" else "http"
            address = f"{scheme}://{listen(ports.http)}/v2/subscribe?{params}"
        path = self.output / f"{label}-{transport}.log"
        process = start([self.oracle, "stream", transport, address, query, str(count)], path)
        self.running.append(process)
        return Probe(process, path, transport)

    def close(self):
        for process in self.running:
            stop(process)


def expect(result, code=None, status=None):
    require(result.get("error", {}).get("code") == code, result)
    if "httpStatus" in result:
        require(result["httpStatus"] == (status or 200), result)
    return result


def run(args):
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    first = int(genesis["initial_height"])
    ports = allocate_ports(9)
    rust_ports = SimpleNamespace(rpc=ports[1], http=ports[3], grpc=ports[4])
    go_ports = SimpleNamespace(rpc=ports[5], p2p=ports[6], grpc=ports[7], http=ports[8])
    rust = Node(args.output / "rust", args.tendermint, args.daemon, args.fixture, ports[:3], False)
    config = rust.home / "config/config.toml"
    # One Tendermint EventBus client belongs to the built-in transaction indexer.
    config.write_text(set_toml(logs(config), "rpc", {"max_subscription_clients": "4"}), encoding="utf-8")
    go_log = args.output / "go.log"
    oracle = start([args.oracle, "serve", args.output / "go", args.fixture, listen(go_ports.rpc),
                    listen(go_ports.p2p), listen(go_ports.grpc), listen(go_ports.http), "0"], go_log)
    flags = ["--api-listen", listen(rust_ports.http), "--grpc-listen", listen(rust_ports.grpc),
             "--tendermint-rpc", listen(rust_ports.rpc)]
    probes = Probes(args.oracle, args.output)
    node_ports = {"go": go_ports, "rust": rust_ports}

    def equivalent_query(query, mode):
        # Go's HTTP and WebSocket gateway share one gRPC connection.
        equality = {"grpc": " = ", "http": "=", "ws": "  =  "}[mode]
        return query.replace(" = ", equality, 1)

    def group(label, query, count=1, transports=TRANSPORTS, params=None, *, equivalent=False):
        return {(node, mode): probes.start(f"{label}-{node}", mode, port,
                                           equivalent_query(query, mode) if equivalent else query,
                                           count, params=params)
                for node, port in node_ports.items() for mode in transports}

    def compare_errors(label, query, code, status, transports=TRANSPORTS, params=None, *, equivalent=False):
        pending = group(label, query, transports=transports, params=params, equivalent=equivalent)
        outcomes = {f"{node}-{mode}": expect(probe.result(), code, status)
                    for (node, mode), probe in pending.items()}
        for mode in transports:
            require(outcomes[f"go-{mode}"] == outcomes[f"rust-{mode}"], outcomes)
        return outcomes

    def comparable_messages(pending, query):
        results = []
        for (_, mode), probe in pending.items():
            messages = expect(probe.result())["messages"]
            for message in messages:
                require(message["query"] == equivalent_query(query, mode), message)
                message["query"] = query
            results.append(messages)
        return results

    try:
        info = wait_for(lambda: rpc(go_ports.rpc, "status"), "Go Tendermint RPC", [(oracle, go_log)])
        peer = SimpleNamespace(p2p=go_ports.p2p, node_id=info["node_info"]["id"],
                               rpc=lambda method, **options: rpc(go_ports.rpc, method, **options))
        rust.launch(peer, "initial", use_rust=True, app_args=flags)
        children = rust.processes + [(oracle, go_log)]
        wait_for(lambda: rust.height() >= first + 1, "Rust synced to Go", children)

        errors = {
            "syntax": compare_errors("syntax", "invalid", 3, 400),
            "empty": compare_errors("empty", "", 3, 400),
            "duplicate": compare_errors("duplicate", "", "400", 400, ("http", "ws"), "query=x&query=y"),
        }
        block_checks = {}
        for event in ("NewBlock", "NewBlockHeader"):
            query = f"tm.event = '{event}'"
            pending = group(event, query, count=5, equivalent=True)
            results = comparable_messages(pending, query)
            by_height = []
            for messages in results:
                require(len(messages) == 5, messages)
                indexed = {}
                for message in messages:
                    data = message["data"]
                    header = data["block"]["header"] if event == "NewBlock" else data["header"]
                    require(isinstance(header["height"], (int, float)), message)
                    indexed[int(header["height"])] = message
                require(len(indexed) == 5, indexed)
                by_height.append(indexed)
            common = sorted(set(by_height[0]).intersection(*by_height[1:]))
            require(len(common) >= 2, f"few common {event} heights: {common}")
            for height in common:
                messages = [index[height] for index in by_height]
                require(messages.count(messages[0]) == len(messages), messages)
            block_checks[event] = {"commonHeights": common, "signedHeader": signed_root(rust, peer, common[-1]),
                                   "message": by_height[0][common[-1]]}

        raw = fixture["transactions"]["transfer"]
        tx_hash = hashlib.sha256(bytes.fromhex(raw)).hexdigest().upper()
        query = f"tm.event = 'Tx' AND tx.hash = '{tx_hash}' AND tags.tx.type = '01'"
        pending = group("transaction", query, equivalent=True)
        wait_for(lambda: all('"ready":true' in logs(probe.log) for probe in pending.values()),
                 "all transaction requests sent", children)
        time.sleep(0.5)
        sent = get(rust_ports.http, "/v2/send_transaction/0x" + raw)
        require(sent["status"] == 200, sent)
        transactions = comparable_messages(pending, query)
        require(len(transactions[0]) == 1 and transactions.count(transactions[0]) == 6, transactions)
        transaction = transactions[0][0]
        require(transaction["query"] == query
                and {"key": "tx.hash", "events": [tx_hash]} in transaction["events"]
                and isinstance(transaction["data"]["result"]["gas_used"], (int, float)), transaction)

        holds = {mode: probes.start("hold", mode, rust_ports, IDLE if mode == "http" else BLOCKS,
                                   count=100) for mode in TRANSPORTS}
        wait_for(lambda: all(any("ready" in row if mode == "http" else
                                 "body" in row and "error" not in row["body"] for row in probe.rows())
                             for mode, probe in holds.items()), "three active subscriptions", children)
        limit = expect(probes.start("exhausted", "grpc", rust_ports, BLOCKS).result(), 8)
        require(limit["error"]["message"] == "max_subscription_clients 4 reached", limit)
        for mode, probe in holds.items():
            stop(probe.process)
            for attempt in range(5):
                reopened = probes.start(f"released-{attempt}", mode, rust_ports, BLOCKS).result()
                if reopened.get("error", {}).get("code") != 8:
                    break
                time.sleep(0.15)
            expect(reopened)
            require(len(reopened["messages"]) == 1, reopened)

        started = time.monotonic()
        lifetime = compare_errors("expiry", IDLE, 4, 504, equivalent=True)
        elapsed = time.monotonic() - started
        require(58 < elapsed < 75, f"unexpected subscription lifetime: {elapsed}")

        active = {mode: probes.start("engine-loss", mode, rust_ports, BLOCKS, count=100)
                  for mode in TRANSPORTS}
        wait_for(lambda: all(any("body" in row and "error" not in row["body"] for row in probe.rows())
                             for probe in active.values()), "subscriptions before engine loss", children)
        stop(rust.engine_process)
        disconnected = {mode: expect(probe.result(), 14) for mode, probe in active.items()}
        require(all(value["messages"] for value in disconnected.values()), disconnected)
        rust.close()
        rust.launch(peer, "restarted", use_rust=True, app_args=flags)
        wait_for(lambda: rust.height() >= block_checks["NewBlock"]["commonHeights"][-1],
                 "restarted Tendermint RPC", rust.processes + [(oracle, go_log)])
        recovered = expect(probes.start("restart", "grpc", rust_ports, BLOCKS).result())
        require(len(recovered["messages"]) == 1, recovered)
        return {"platform": platform.platform(), "errors": errors, "blocks": block_checks,
                "transaction": transaction, "clientLimit": limit, "lifetime": lifetime,
                "lifetimeSeconds": round(elapsed, 3), "engineLoss": disconnected, "recovered": recovered}
    finally:
        probes.close()
        rust.close()
        stop(oracle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "oracle", "tendermint", "fixture", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = run(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"platform": result["platform"], "lifetimeSeconds": result["lifetimeSeconds"],
                      "verified": str(args.output)}))
