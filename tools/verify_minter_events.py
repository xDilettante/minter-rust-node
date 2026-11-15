"""Compare persisted Minter events to Go on authenticated blocks and in a mixed devnet."""

import argparse
import base64
from collections import Counter
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import platform
import socket
import subprocess
from types import SimpleNamespace

from verify_mainnet_empty import Abci, app, fields, load_inputs
from verify_mainnet_iavl import check_file
from verify_minter_api_v2 import get, grpc, listen, rpc, verify_queries
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, require, signed_root, start, stop, wait_for


def case(height, code=0, **fields):
    return {"Method": "Events", "Code": code, "Height": height, **fields}


def missing(port, height):
    result = get(port, f"/v2/events/{height}")
    require(result == {"status": 404, "body": {"error": {"code": "404",
            "message": f"version {height} doesn't exist yet", "data": {}}}}, result)
    return result


@contextmanager
def go_events(args, folder, snapshot, manifest, port, ports):
    log = folder / "go.log"
    process = start([args.oracle, "checkpoint-events", folder / "go", snapshot, manifest,
                     listen(port), listen(ports.grpc), listen(ports.http)], log)
    try:
        with wait_for(lambda: socket.create_connection(("127.0.0.1", port), timeout=1),
                      "Go ABCI listener", [(process, log)], timeout=180) as connection:
            connection.settimeout(90)
            with connection.makefile("rb") as reader:
                yield Abci(connection, reader, flush=True)
    finally:
        stop(process)


def compare_history(args, folder, go, rust, records, base):
    first, last = records[0]["height"], records[-1]["height"]
    queries = [case(height) for height in range(first, last + 1)]
    results = grpc(args.oracle, go.grpc, queries, folder / "discovery.json")
    require(len(results) == len(queries), "Go response count differs")
    cases = [case(base, 5), case(last + 1, 5)]
    per_block = []
    types = Counter()
    empty = False
    for query, result in zip(queries, results):
        require(result["code"] == 0, result)
        height = query["Height"]
        events = result["body"]["events"]
        if not events:
            if not empty:
                cases.append(query)
                empty = True
            continue
        kinds = Counter(event["type"].removeprefix("minter/") for event in events)
        types.update(kinds)
        cases.append(query)
        per_block.append({"height": height, "types": kinds})
        for kind in kinds:
            sample = next(event["value"] for event in events if event["type"] == f"minter/{kind}")
            if "address" in sample:
                cases.append(case(height, Search=["missing", sample["address"]]))
            if "validator_pub_key" in sample:
                cases.append(case(height, Search=[sample["validator_pub_key"]]))
                break
        cases.append(case(height, Search=["missing"]))
    require(bool(per_block), f"{folder.name} replay did not produce Minter events")
    # The Go API narrows the requested height to uint32 for the lookup.
    cases.append(case(per_block[0]["height"] + 2**32))
    comparisons = verify_queries(SimpleNamespace(oracle=args.oracle, output=folder),
                                 cases, go, rust, "events")
    # Preserve the full ordered responses, including the samples used for filtering.
    (folder / "events.json").write_text(json.dumps(comparisons, indent=2) + "\n", encoding="utf-8")
    return per_block, types, len(cases)


def load_corpus(corpus_dir):
    corpus = json.loads((corpus_dir / "corpus.json").read_bytes())
    files = {"go_checkpoint/snapshot.zlib": "snapshot.zlib",
             "go_checkpoint/manifest.json": "checkpoint.json",
             "abci-iavl.jsonl.gz": "abci-iavl.jsonl.gz", "blocks.csv.gz": "blocks.csv.gz"}
    paths = []
    for name, flat in files.items():
        path = corpus_dir / name
        if not path.is_file():
            path = corpus_dir / flat
        check_file(path, corpus["files"][name])
        paths.append(path)
    snapshot, manifest, trace, headers = paths
    checkpoint = json.loads(manifest.read_bytes())
    require(checkpoint["height"] == corpus["checkpoint"], "checkpoint height differs from corpus")
    records, _ = load_inputs(trace, headers, checkpoint, corpus["through"])
    return snapshot, manifest, checkpoint, records


def load_swap_fixture(folder):
    snapshot, manifest = (folder / "checkpoint" / name for name in ("snapshot.zlib", "manifest.json"))
    checkpoint = json.loads(manifest.read_bytes())
    fixture = json.loads((folder / "go-swap-v034.json").read_bytes())
    records = []
    record = None
    for exchange in fixture["exchanges"]:
        [(tag, request)] = fields(bytes.fromhex(exchange["request"])).items()
        if tag == 8:
            continue  # CheckTx
        response = fields(bytes.fromhex(exchange["response"]))[tag + 1]
        pair = {"request": base64.b64encode(request).decode(),
                "response": base64.b64encode(response).decode()}
        if tag == 7:
            require(record is None, "incomplete fixture block")
            record = {"height": int(exchange["label"].split("-")[-1]),
                      "begin_block": pair, "deliver_tx": []}
        elif tag == 9:
            record["deliver_tx"].append(pair)
        elif tag == 10:
            record["end_block"] = pair
        elif tag == 11:
            record.update({"commit_response": pair["response"], "app_hash": fields(response)[2].hex()})
            records.append(record)
            record = None
        else:
            raise RuntimeError(f"unexpected ABCI request in swap fixture: {tag}")
    require(record is None, "incomplete fixture block")
    return snapshot, manifest, checkpoint, records


def replay(args, input_dir, load):
    folder = args.output / input_dir.name
    folder.mkdir()
    snapshot, manifest, checkpoint, records = load(input_dir)
    base = checkpoint["height"]
    require(len(records) >= 3, "need three committed blocks to test event retention")
    db = folder / "rocksdb"
    with (folder / "import.log").open("wb") as log:
        subprocess.run([args.cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=300)

    go_abci, go_grpc, go_http, rust_grpc, rust_http = allocate_ports(5)
    go_ports = SimpleNamespace(grpc=go_grpc, http=go_http)
    rust_ports = SimpleNamespace(grpc=rust_grpc, http=rust_http)
    api_flags = ["--grpc-listen", listen(rust_grpc), "--api-listen", listen(rust_http)]
    initial = {"height": base, "appHash": checkpoint["appHash"]}
    first = records[0]
    last = records[-1]["height"]
    with go_events(args, folder, snapshot, manifest, go_abci, go_ports) as go:
        require(go.info() == initial, "Go did not open at the checkpoint")
        with app(args.daemon, db, folder / "before.log", *api_flags,
                 "--keep-iavl-versions", "1") as (rust, process):
            require(rust.info() == initial, "Rust did not open at the checkpoint")
            missing(rust_http, base)
            rust.stage(first)
            require(rust.info() == initial, "staging changed committed height or AppHash")
            missing(rust_http, first["height"])
            stop(process)

        with app(args.daemon, db, folder / "replay.log", *api_flags) as (rust, process):
            require(rust.info() == initial, "uncommitted block advanced height after restart")
            missing(rust_http, first["height"])
            for record in records:
                go.stage(record)
                rust.stage(record)
                require(go.commit(record) == rust.commit(record), "Go and Rust Commit differ")
            stop(process)

        with app(args.daemon, db, folder / "reopen.log", *api_flags) as (rust, _):
            head = rust.info()
            require(head == {"height": last, "appHash": records[-1]["app_hash"]},
                    "durable head differs after forced stop")
            events, types, cases = compare_history(args, folder, go_ports, rust_ports, records, base)
            require(rust.iavl_key(b"p", last - 1).get(1) == 1,
                    "IAVL history did not expire while event history survived")

        with app(args.daemon, db, folder / "prune.log", *api_flags,
                 "--keep-event-blocks", "2") as (rust, _):
            require(rust.info() == head, "event pruning changed AppHash")
            missing(rust_http, last - 2)
            for height in (last - 1, last):
                path = f"/v2/events/{height}"
                require(get(rust_http, path) == get(go_http, path), f"lost retained events at {height}")

        with app(args.daemon, db, folder / "pruned-reopen.log", *api_flags) as (rust, _):
            require(rust.info() == head, "event pruning changed reopened AppHash")
            missing(rust_http, last - 2)
    return {"corpus": input_dir.name, "checkpoint": initial, "head": head,
            "replayedBlocks": len(records), "comparedRequests": cases, "events": events,
            "types": types, "retainedEventBlocks": [last - 1, last]}


def devnet(args):
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    first = int(genesis["initial_height"])
    due = first + 12
    sender = genesis["app_state"]["accounts"][0]["address"]
    ports = allocate_ports(9)
    rust_ports = SimpleNamespace(rpc=ports[1], http=ports[3], grpc=ports[4])
    go_ports = SimpleNamespace(rpc=ports[5], p2p=ports[6], grpc=ports[7], http=ports[8])
    rust = Node(args.output / "rust", args.tendermint, args.daemon, args.fixture, ports[:3], False)
    log = args.output / "go.log"
    oracle = start([args.oracle, "serve", args.output / "go", args.fixture, listen(go_ports.rpc),
                    listen(go_ports.p2p), listen(go_ports.grpc), listen(go_ports.http), "2"], log)
    flags = ["--api-listen", listen(rust_ports.http), "--grpc-listen", listen(rust_ports.grpc),
             "--tendermint-rpc", listen(rust_ports.rpc), "--keep-iavl-versions", "2"]
    try:
        info = wait_for(lambda: rpc(go_ports.rpc, "status"), "Go Tendermint RPC", [(oracle, log)])
        peer = SimpleNamespace(p2p=go_ports.p2p, node_id=info["node_info"]["id"],
                               rpc=lambda method, **options: rpc(go_ports.rpc, method, **options))
        rust.launch(peer, "initial", use_rust=True, app_args=flags)

        def wait_height(height):
            wait_for(lambda: min(rust.height(), int(peer.rpc("status")["sync_info"]["latest_block_height"]))
                     >= height, f"both nodes at {height}", rust.processes + [(oracle, log)])

        wait_height(first + 1)
        raw = fixture["transactions"]["event-lock"]
        sent = get(rust_ports.http, "/v2/send_transaction/0x" + raw)
        require(sent["status"] == 200, sent)
        tx_hash = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
        receipt = wait_for(lambda: peer.rpc("tx", hash="0x" + tx_hash), "indexed Lock",
                           rust.processes + [(oracle, log)])
        require(receipt["tx_result"].get("code", 0) == 0 and int(receipt["height"]) < due, receipt)
        wait_height(due + 3)
        block = {"Method": "Block", "Code": 0, "Height": due, "Fields": [0], "Events": True}
        cases = [case(first), case(due), case(due, Search=[sender]), block, {**block, "Events": False}]
        results = verify_queries(args, cases, go_ports, rust_ports, "devnet")
        expected = {"address": sender, "coin": "0", "amount": str(10**18)}
        require(results[1]["rustGrpc"]["body"]["events"] ==
                [{"type": "minter/UnlockEvent", "value": expected}], results[1])
        require(results[2]["rustGrpc"]["body"]["events"] == [], "UnlockEvent matched a stake search")
        require(results[3]["rustGrpc"]["body"]["events"] ==
                [{"@type": "type.googleapis.com/api_pb.UnlockEvent", **expected}], results[3])
        require(results[4]["rustGrpc"]["body"]["events"] == [], "Block ignored events=false")
        root = signed_root(rust, peer, due + 1)
        rust.close()

        rust.launch(peer, "bounded", use_rust=True, app_args=[*flags, "--keep-event-blocks", "2"])
        wait_height(due + 4)
        pruned = missing(rust_ports.http, due)
        pruned_block = get(rust_ports.http, f"/v2/block/{due}?fields=0&events=true")
        require(pruned_block["status"] == 200 and pruned_block["body"]["events"] == [], pruned_block)
        # The newest blocks continue to get an event record after the retention change.
        head = rust.height()
        require(get(rust_ports.http, f"/v2/events/{head}")["status"] == 200,
                "latest block lacks an events record")
        return {"signedHeader": root, "comparisons": results, "prunedEvents": pruned,
                "prunedBlock": pruned_block, "latestEventHeight": head}
    finally:
        rust.close()
        stop(oracle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    devnet_args = modes.add_parser("devnet")
    replay_args = modes.add_parser("replay")
    for mode, paths in ((devnet_args, ("tendermint", "fixture")), (replay_args, ("cli",))):
        for name in ("daemon", "oracle", "output", *paths):
            mode.add_argument(name, type=Path)
    replay_args.add_argument("corpus", nargs="*", type=Path)
    replay_args.add_argument("--swap-fixture", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    if args.mode == "devnet":
        results = devnet(args)
    else:
        require(args.corpus or args.swap_fixture, "need a corpus or a Go swap fixture")
        results = {"corpora": [replay(args, folder, load_corpus) for folder in args.corpus]}
        if args.swap_fixture:
            results["swapFixture"] = replay(args, args.swap_fixture, load_swap_fixture)
    result = {"platform": platform.platform(), **results}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"platform": result["platform"], "mode": args.mode, "result": str(args.output)}))


if __name__ == "__main__":
    main()
