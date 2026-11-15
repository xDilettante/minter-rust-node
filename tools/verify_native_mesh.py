"""Run four Rust validators, then verify their completed chain with a Go observer."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import time

from tcp_link import TcpLink
from verify_minter_api_v2 import get, listen
from verify_native_consensus import delivery_digest
from verify_native_multinode import RustValidator
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, start, wait_for


def verify(args):
    fixture = args.output / "fixture"
    subprocess.run([args.fixture_tool, "multinode", args.fixture, fixture],
                   check=True, timeout=20, creationflags=HIDE_WINDOW)
    source = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    first = source["genesisHeight"] + 1
    ports = allocate_ports(11)
    rust = [RustValidator(args, fixture, prefix, f"rust{i}", ports[2*i:2*i+2])
            for i, prefix in enumerate(("priv_validator", "second_validator", "third_validator", "fourth_validator"))]
    links, phases, compared, transactions, rejoined = {}, [], {}, {}, []
    observer = None

    def processes():
        return [item for node in rust for item in node.processes()]

    def heights():
        return [max(node.rows(), default=first - 1) for node in rust]

    def record(phase, **data):
        event = {"phase": phase, **data}
        phases.append(event)
        with (args.output / "phases.jsonl").open("a", encoding="utf-8") as file:
            file.write(json.dumps(event) + "\n")
        print(json.dumps(event), flush=True)

    def compare_through(height):
        wait_for(lambda: min(heights()) > height, f"four Rust nodes passed {height}", processes(), timeout=90)
        rows = [node.rows() for node in rust]
        for number in range(first, height + 1):
            if number not in compared:
                values = [items[number] for items in rows]
                require(all(row == values[0] for row in values), f"Rust nodes differ at {number}")
                require(values[0]["validatorUpdates"] == 0,
                        f"unexpected validator change in the four-Rust scenario at {number}")
                compared[number] = values[0]
        record("rust-compared", through=height, heights=heights())

    def transfer(name):
        raw = source["transactions"][name]
        before = max(heights())
        response = get(rust[0].api, f"/v2/send_transaction/0x{raw}")
        require(response["status"] == 200, response)
        height = wait_for(lambda: next((h for h, row in rust[0].rows().items()
                                       if h > before and row["transactions"]), None),
                          f"Rust committed {name}", processes())
        compare_through(height + 1)
        digest = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
        transactions[height] = digest
        record("transaction", name=name, height=height, hash=digest)
        return height

    def serve_only(node):
        node.close()
        node.log = node.root / "archive-only.log"
        node.process = start([args.daemon, "native-sync", "--db", node.root / "state",
                              "--p2p-listen", listen(node.p2p)], node.log)
        wait_for(lambda: "Tendermint 0.34 P2P listening on" in logs(node.log),
                 "read-only block server", node.processes())

    try:
        for i, node in enumerate(rust):
            for j in range(i):
                link = TcpLink(rust[j].p2p)
                links[f"r{i}-r{j}"] = link
                node.outgoing.append(f"{rust[j].peer.split('@')[0]}@127.0.0.1:{link.port}")
            node.launch("initial")
        compare_through(first + 9)
        transfer("transfer")
        healthy_through = max(compared)
        wait_for(lambda: all(link.stats()["connections"] and link.stats()["bytes"] for link in links.values()),
                 "all six Rust outbound links exchanged data", processes())
        record("all-rust-healthy", through=healthy_through,
               links={name: link.stats() for name, link in links.items()})
        before = heights()
        rust[0].close()
        target = max(before) + 4
        wait_for(lambda: min(heights()[1:]) >= target, "three Rust validators continue", processes(), timeout=90)
        record("one-rust-stopped", before=before, online=heights()[1:])
        rust[0].launch("rejoined")
        compare_through(target)
        boundary = max(heights())
        compare_through(boundary + 10)
        rejoined.append({"after": boundary, "through": max(compared), "addresses": [rust[0].address]})
        record("rust-rejoined", **rejoined[-1])

        crossing = [f"r{i}-r{j}" for i in (2, 3) for j in (0, 1)]
        for name in crossing:
            links[name].set_enabled(False)
        wait_for(lambda: all(not links[name].stats()["connections"] and not links[name].stats()["pending"]
                             for name in crossing), "cross-group sockets closed", processes())
        internal = ["r1-r0", "r3-r2"]
        require(all(links[name].stats()["connections"] and links[name].stats()["bytes"] for name in internal),
                "both halves must keep their internal connection")
        last, stable_since = heights(), time.monotonic()

        def settled():
            nonlocal last, stable_since
            current = heights()
            if current != last:
                last, stable_since = current, time.monotonic()
            return current if time.monotonic() - stable_since >= 2 else None

        stopped_at = wait_for(settled, "partition settled", processes(), timeout=15)
        for _ in range(20):
            time.sleep(0.25)
            require(all(p.poll() is None for p, _ in processes()), "node exited during partition")
            require(heights() == stopped_at, "a 50% partition committed a new block")
            require(all(links[name].stats()["connections"] for name in internal),
                    "an internal link was lost: this is not a 2+2 partition")
        record("split-stalled", seconds=5, heights=stopped_at,
               links={name: link.stats() for name, link in links.items()})
        for name in crossing:
            links[name].set_enabled(True)
        compare_through(max(stopped_at) + 6)
        record("network-healed", through=max(compared))

        for node in rust:
            node.close()
        boundary = max(heights())
        record("all-rust-stopped", heights=heights())
        for node in rust:
            node.launch("cluster-restart")
        # More than one complete 24-block attendance window must pass after recovery.
        compare_through(boundary + 32)
        rejoined.append({"after": boundary, "through": max(compared), "addresses": [node.address for node in rust]})
        record("all-rust-restarted", **rejoined[-1])
        transfer("self")
        through = max(compared)
        wait_for(lambda: min(heights()) >= through + 3, "archive includes later commit witnesses", processes())
        for node in rust:
            node.close()
        for link in links.values():
            link.close()
        record("rust-validation-finished", through=through, goProcessesStarted=0)

        # No Go process participates in the Rust network. Only now replay its
        # completed blocks with an ordinary non-validator Go node.
        serve_only(rust[0])
        observer = Node(args.output / "go-observer", args.tendermint, args.go_app, fixture, ports[8:], False)
        observer.launch(None, "offline-check", use_rust=False,
                        engine_args=("--p2p.persistent_peers", rust[0].peer))
        wait_for(lambda: observer.height() > through, "Go replayed the completed Rust chain",
                 observer.processes + processes(), timeout=90)
        attestations = {}
        for height, row in compared.items():
            signed = observer.rpc("commit", height=height)["signed_header"]
            (args.output / f"commit-{height}.json").write_text(json.dumps(signed) + "\n", encoding="utf-8")
            following = observer.rpc("commit", height=height + 1)["signed_header"]["header"]
            header, commit = signed["header"], signed["commit"]
            result = observer.rpc("block_results", height=height)
            path = args.output / f"block-results-{height}.json"
            path.write_text(json.dumps(result) + "\n", encoding="utf-8")
            end = json.loads(subprocess.check_output([args.fixture_tool, "end-block", path],
                                                    timeout=15, creationflags=HIDE_WINDOW))
            require(row["appHash"] == following["app_hash"].lower()
                    and row["blockHash"] == commit["block_id"]["hash"].lower()
                    and row["partSetHash"] == commit["block_id"]["parts"]["hash"].lower()
                    and row["parts"] == int(commit["block_id"]["parts"]["total"])
                    and row["resultsHash"] == following["last_results_hash"].lower()
                    and row["validatorsHash"] == header["validators_hash"].lower()
                    and row["nextValidatorsHash"] == header["next_validators_hash"].lower()
                    and row["endResponseSha256"] == end["endResponseSha256"]
                    and row["validatorUpdates"] == end["validatorUpdates"], f"Go differs at {height}: {row}")
            attestations[height] = {"round": int(commit["round"]), "proposer": header["proposer_address"].upper(),
                                    "signers": [s["validator_address"].upper() for s in commit["signatures"]
                                                if s["block_id_flag"] == 2 and s["signature"]]}
        expected = {node.address for node in rust}
        require({a for h, item in attestations.items() if h <= healthy_through for a in item["signers"]} == expected,
                "not all four Rust validators signed the healthy chain")
        require({item["proposer"] for h, item in attestations.items() if h <= healthy_through} == expected,
                "not all four Rust validators proposed healthy commits")
        for phase in rejoined:
            for address in phase["addresses"]:
                signed = [h for h, item in attestations.items() if phase["after"] < h <= phase["through"] and address in item["signers"]]
                proposed = [h for h, item in attestations.items() if phase["after"] < h <= phase["through"] and item["proposer"] == address]
                require(signed and proposed, f"returned Rust validator {address} did not sign and propose new blocks")
                phase.setdefault("freshParticipation", {})[address] = {"signed": signed, "proposed": proposed}
        for height, digest in transactions.items():
            receipt = observer.rpc("tx", hash=f"0x{digest}")
            require(int(receipt["height"]) == height and receipt["tx_result"].get("code", 0) == 0, receipt)
            require(compared[height]["deliverResponsesSha256"] == delivery_digest(receipt["tx_result"]),
                    "Go transaction result differs")
        record("go-replay-matched", blocks=len(compared), through=through)
        return {"platform": platform.platform(), "rustValidators": 4, "goNodesDuringConsensus": 0,
                "compared": list(compared.values()), "transactions": transactions, "attestations": attestations,
                "phases": phases, "rejoined": rejoined, "maxCommitRound": max(item["round"] for item in attestations.values())}
    finally:
        if observer:
            observer.close()
        for node in rust:
            node.close()
        for link in links.values():
            link.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "tendermint", "go_app", "fixture_tool", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: v for k, v in result.items() if k not in ("compared", "attestations", "phases")}))
