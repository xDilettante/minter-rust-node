"""Check physical Tendermint history retention with a Rust ABCI app and Go peer."""

import argparse
from datetime import datetime, timedelta
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
from types import SimpleNamespace
from urllib.parse import urlencode

from verify_minter_api_v2 import get, grpc, http_request, listen, rpc
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, signed_root, start, stop, wait_for


BLOCKS = 3
INDEX = 36
EVIDENCE_BLOCKS = 5
EVIDENCE_TIME = 6


def probe(binary, node, height, tx_hash):
    return json.loads(subprocess.check_output(
        [binary, node.home, str(height), tx_hash[2:]], timeout=20, creationflags=HIDE_WINDOW))


def with_api(node, ports):
    return ["--api-listen", listen(ports.http), "--grpc-listen", listen(ports.grpc),
            "--tendermint-rpc", listen(node.rpc_port)]


def index_rpc_error(port, method, **args):
    answer = get(port, f"/{method}?{urlencode(args)}")
    error = answer["body"].get("error")
    require(error is not None and "needed for transaction proof is unavailable" in error.get("data", ""), answer)
    return error


def tx_hash(raw):
    return "Mt" + hashlib.sha256(bytes.fromhex(raw)).hexdigest()


def verify(args):
    args.output.mkdir()
    fixture = args.output / "fixture"
    fixture.mkdir()
    for name in ("priv_validator_key.json", "priv_validator_state.json"):
        shutil.copyfile(args.fixture / name, fixture / name)
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    evidence = genesis["consensus_params"]["evidence"]
    evidence["max_age_num_blocks"] = str(EVIDENCE_BLOCKS)
    evidence["max_age_duration"] = str(EVIDENCE_TIME * 1_000_000_000)
    (fixture / "genesis.json").write_text(json.dumps(genesis), encoding="utf-8")
    first = int(genesis["initial_height"])
    transactions = json.loads((args.fixture / "go-abci-v034.json").read_bytes())["transactions"]
    old_hash = tx_hash(transactions["transfer"])
    new_hash = tx_hash(transactions["self"])
    ports = allocate_ports(14)
    go = SimpleNamespace(rpc_port=ports[0], p2p=ports[1], grpc=ports[2], http=ports[3],
                         rpc=lambda method, **params: rpc(ports[0], method, **params))
    archive_api = SimpleNamespace(http=ports[7], grpc=ports[8])
    fresh_api = SimpleNamespace(http=ports[12], grpc=ports[13])
    archive = Node(args.output / "converted", args.tendermint, args.daemon, fixture, ports[4:7], False)
    fresh = Node(args.output / "fresh", args.tendermint, args.daemon, fixture, ports[9:12], False)
    oracle_log = args.output / "go.log"
    oracle = start([args.oracle, "serve", args.output / "go", fixture, listen(go.rpc_port),
                    listen(go.p2p), listen(go.grpc), listen(go.http), "0"], oracle_log)
    flags = ["--keep_blocks", str(BLOCKS), "--keep_index_blocks", str(INDEX), "--log_level", "debug"]

    def wait_node(node, minimum, *, synced=False):
        def ready():
            info = node.rpc("status")["sync_info"]
            if int(info["latest_block_height"]) < minimum or (synced and info["catching_up"]):
                return None
            return info
        return wait_for(ready, f"{node.root.name} reaches {minimum}", node.processes + [(oracle, oracle_log)])

    def wait_indexed(port, hash):
        return wait_for(lambda: rpc(port, "tx", hash="0x" + hash[2:]),
                        f"indexed transaction {hash}", archive.processes + fresh.processes + [(oracle, oracle_log)])

    def bounded(node, minimum):
        def reached():
            info = node.rpc("status")["sync_info"]
            height = int(info["latest_block_height"])
            base = int(info["earliest_block_height"])
            if info["catching_up"] or height < minimum:
                return None
            target = max(first, min(height - BLOCKS + 1, height - EVIDENCE_BLOCKS))
            # Query the same head's signed time to avoid wall-clock assumptions.
            header = go.rpc("block", height=height)["block"]["header"]
            cutoff = datetime.fromisoformat(header["time"]) - timedelta(seconds=EVIDENCE_TIME)
            lower, upper = first, target
            while lower < upper:
                mid = lower + (upper - lower) // 2
                observed = go.rpc("block", height=mid)["block"]["header"]["time"]
                if datetime.fromisoformat(observed) < cutoff:
                    lower = mid + 1
                else:
                    upper = mid
            if base != lower:
                return None
            return {"base": base, "head": height, "heightOnlyFloor": target,
                    "cutoff": cutoff.isoformat(), "headTime": header["time"]}
        return wait_for(reached, f"evidence-aware history of {node.root.name}",
                        node.processes + [(oracle, oracle_log)])

    def same_tx(port, hash, name):
        request = [{"Method": "Transaction", "Hash": hash}]
        expected, = grpc(args.oracle, go.grpc, request, args.output / f"go-{name}.json")
        actual, = grpc(args.oracle, port.grpc, request, args.output / f"rust-{name}.json")
        require(actual == expected and actual["code"] == 0, (expected, actual))
        require(get(port.http, "/v2/transaction/" + hash) == {"status": 200, "body": actual["body"]},
                "HTTP and gRPC transaction results differ")
        return actual["body"]

    try:
        status = wait_for(lambda: go.rpc("status"), "Go validator RPC", [(oracle, oracle_log)])
        go.node_id = status["node_info"]["id"]
        archive.launch(go, "archive", use_rust=True, app_args=with_api(archive, archive_api))
        wait_node(archive, first + 1, synced=True)
        old = go.rpc("broadcast_tx_commit", tx="0x" + transactions["transfer"])
        require(old["check_tx"].get("code", 0) == old["deliver_tx"].get("code", 0) == 0, old)
        old_height = int(old["height"])
        wait_node(archive, old_height + 14, synced=True)
        old_receipt = same_tx(archive_api, old_hash, "archive")
        require(get(archive_api.http, f"/v2/block/{old_height}")["status"] == 200, "archive lost old block")
        archive.close()
        before = probe(args.inspect, archive, old_height, old_hash)
        require(before["base"] == first and before["indexFloor"] == 0 and
                all(before[key] for key in ("blockAvailable", "resultsAvailable", "txAvailable", "blockIndexed")),
                before)

        # The fresh observer receives old blocks by fast sync. The other
        # observer converts its existing stock-format database at startup.
        fresh.launch(go, "bounded", use_rust=True, app_args=with_api(fresh, fresh_api), engine_args=flags)
        archive.launch(go, "converted", use_rust=True, app_args=with_api(archive, archive_api), engine_args=flags)
        sync = bounded(fresh, old_height + 14)
        converted = bounded(archive, old_height + 14)
        require(sync["base"] > old_height and converted["base"] > old_height and
                sync["base"] < sync["heightOnlyFloor"], (sync, converted))
        fast_log = logs(fresh.root / "tm-bounded.log")
        require("pruned block and ABCI history" in fast_log and "Time to switch to consensus reactor!" in fast_log and
                fast_log.index("pruned block and ABCI history") < fast_log.index("Time to switch to consensus reactor!"),
                "no prune observed before the fast-sync reactor switched to consensus")

        proof_errors = []
        for node, api in ((fresh, fresh_api), (archive, archive_api)):
            missing = get(api.http, f"/v2/block/{old_height}")
            require(missing["status"] == 404 and missing["body"]["error"]["message"] == "Block not found", missing)
            require(get(api.http, f"/v2/coin_info/BIP?height={old_height}")["status"] == 200,
                    "Tendermint retention changed the separate IAVL archive")
            require(same_tx(api, old_hash, node.root.name) == old_receipt, "index changed the retained receipt")
            proof_errors.append(index_rpc_error(node.rpc_port, "tx", hash="0x" + old_hash[2:], prove="true"))
            proof_errors.append(index_rpc_error(node.rpc_port, "tx_search",
                               query=json.dumps(f"tx.height = {old_height}"), prove="true"))
        for node, api in ((fresh, fresh_api), (archive, archive_api)):
            wait_node(node, old_height + INDEX + 2, synced=True)
            gone = get(api.http, "/v2/transaction/" + old_hash)
            require(gone["status"] == 400 and "not found" in gone["body"]["error"]["message"], gone)
            missing_txs = get(api.http, "/v2/transactions?" + urlencode(
                {"query": f"tx.height = {old_height}", "page": 1, "per_page": 10}))
            require(missing_txs == {"status": 200, "body": {"transactions": []}}, missing_txs)

        submitted = http_request(archive_api.http, "/v2/send_transaction", "POST",
                                 {"Content-Type": "application/json"},
                                 json.dumps({"tx": "0x" + transactions["self"]}).encode())
        require(submitted["status"] == 200 and submitted["body"]["hash"] == new_hash, submitted)
        new_height = int(wait_indexed(go.rpc_port, new_hash)["height"])
        for node, api in ((fresh, fresh_api), (archive, archive_api)):
            wait_indexed(node.rpc_port, new_hash)
            wait_node(node, new_height + 1, synced=True)
            same_tx(api, new_hash, f"{node.root.name}-after-prune")
        root = signed_root(archive, go, new_height + 1)
        archive.close()
        fresh.close()
        after = {}
        for node in (archive, fresh):
            old_disk = probe(args.inspect, node, old_height, old_hash)
            recent_disk = probe(args.inspect, node, new_height, new_hash)
            require(old_disk["base"] > old_height and old_disk["indexFloor"] > old_height and
                    not any(old_disk[key] for key in ("blockAvailable", "resultsAvailable", "txAvailable", "blockIndexed",
                                                     "txHeightKeys")) and
                    all(old_disk[key] for key in ("baseBlockAvailable", "baseResultsAvailable", "baseValidatorsAvailable")),
                    old_disk)
            require(all(recent_disk[key] for key in ("blockAvailable", "resultsAvailable", "txAvailable", "blockIndexed")),
                    recent_disk)
            after[node.root.name] = {"expired": old_disk, "recent": recent_disk}

        # Change the persisted TOML values and restart after a forced stop.
        path = archive.home / "config/config.toml"
        config = path.read_text(encoding="utf-8")
        require("keep_blocks = 0" in config and "keep_index_blocks = 0" in config, "companion config lacks history")
        config = config.replace("keep_blocks = 0", f"keep_blocks = {BLOCKS}", 1)
        path.write_text(config.replace("keep_index_blocks = 0", f"keep_index_blocks = {INDEX}", 1), encoding="utf-8")
        archive.launch(go, "restarted", use_rust=True, app_args=with_api(archive, archive_api))
        restart = bounded(archive, new_height + 2)
        require(restart["base"] >= after["converted"]["expired"]["base"], restart)
        require(get(archive_api.http, "/v2/transaction/" + old_hash)["status"] == 400, "old hash reappeared")
        same_tx(archive_api, new_hash, "after-restart")
        for log in (fresh.root / "tm-bounded.log", archive.root / "tm-converted.log",
                    archive.root / "tm-restarted.log"):
            require("could not prune" not in logs(log), f"history prune failed in {log}")
        result = {"os": platform.platform(), "archived": before, "fastSync": sync, "converted": converted,
                  "pruned": after, "restart": restart, "oldTxHeight": old_height, "recentTxHeight": new_height,
                  "signedRoot": root, "prunedProofErrors": proof_errors,
                  "keepBlocks": BLOCKS, "keepIndexBlocks": INDEX,
                  "evidenceBlocks": EVIDENCE_BLOCKS, "evidenceSeconds": EVIDENCE_TIME}
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({key: result[key] for key in ("os", "archived", "fastSync", "converted", "restart", "signedRoot")}))
    finally:
        archive.close()
        fresh.close()
        stop(oracle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("tendermint", "daemon", "oracle", "inspect", "fixture", "output"):
        parser.add_argument(name, type=Path)
    verify(parser.parse_args())
