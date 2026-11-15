"""Verify round >=2 and signer recovery with seven Rust validators and a Go replay."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess

from tcp_link import TcpLink
from verify_minter_api_v2 import get, listen
from verify_native_consensus import delivery_digest
from verify_native_multinode import RustValidator
from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, start, wait_for


def verify(args):
    fixture = args.output / "fixture"
    subprocess.run([args.fixture_tool, "quorum", args.fixture, fixture],
                   check=True, timeout=20, creationflags=HIDE_WINDOW)
    source = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    first = source["genesisHeight"] + 1
    ports = allocate_ports(17)
    oracle = Node(args.output / "go-observer", args.tendermint, args.go_app, fixture, ports[14:], False)
    rust, links, compared, phases = [], {}, {}, []
    keys = ("priv_validator", "second_validator", "third_validator", "fourth_validator",
            "fifth_validator", "sixth_validator", "seventh_validator")
    peer_ids = json.loads((fixture / "quorum-peers.json").read_bytes())

    def processes():
        return [item for node in rust for item in node.processes()]

    def record(phase, **data):
        row = {"phase": phase, **data}
        phases.append(row)
        with (args.output / "phases.jsonl").open("a", encoding="utf-8") as output:
            output.write(json.dumps(row) + "\n")
        print(json.dumps(row), flush=True)

    def heights(indices):
        return [max(rust[i].rows(), default=first - 1) for i in indices]

    def compare_through(height):
        wait_for(lambda: min(heights(range(7))) > height, f"seven Rust nodes passed {height}", processes(), timeout=120)
        rows = [node.rows() for node in rust]
        for h in range(first, height + 1):
            if h in compared:
                continue
            require(all(items[h] == rows[0][h] for items in rows), f"Rust execution differs at {h}")
            require(rows[0][h]["validatorUpdates"] == 0, f"unexpected validator change at {h}")
            compared[h] = rows[0][h]
        record("rust-compared", through=height, heights=heights(range(7)))

    def ready_links():
        events = [[row for path in node.traces for row in decode_trace(path)
                   if row.get("event") == "consensus_peer_ready"] for node in rust]
        for i in range(7):
            for j in range(i):
                stats = links[f"r{i}-r{j}"].stats()
                if not stats["connections"] or len(stats["sessions"]) != stats["connections"]:
                    return False
                for session in stats["sessions"]:
                    if not any(row["peerId"] == peer_ids[keys[j]] and row["localPort"] == session["sourcePort"]
                               for row in events[i]):
                        return False
                    if not any(row["peerId"] == peer_ids[keys[i]] and row["remotePort"] == session["upstreamPort"]
                               for row in events[j]):
                        return False
        return True

    try:
        # Read initial proposer/priorities from Go before any Rust validator starts.
        oracle.launch(None, "initial-state", use_rust=False)
        consensus = wait_for(lambda: oracle.rpc("dump_consensus_state"), "Go initial consensus state", oracle.processes)
        (args.output / "go-initial-consensus.json").write_text(json.dumps(consensus, indent=2) + "\n")
        state = consensus["round_state"]
        require(int(state["height"]) == first and int(state["round"]) == 0, "unexpected initial Go height/round")
        validators = {"block_height": str(first), "validators": state["validators"]["validators"],
                      "proposer": state["validators"]["proposer"]["address"]}
        require(len(validators["validators"]) == 7, validators)
        require({int(v["voting_power"]) for v in validators["validators"]} == {14285714}, validators)
        initial = args.output / "go-initial-validators.json"
        initial.write_text(json.dumps(validators, indent=2) + "\n")
        schedule = json.loads(subprocess.check_output([args.fixture_tool, "proposers", initial, "64"],
                                                      timeout=15, creationflags=HIDE_WINDOW))
        (args.output / "go-proposers.json").write_text(json.dumps(schedule, indent=2) + "\n")
        require(len(set(schedule["proposers"][:3])) == 3, "initial three proposers must be different")
        oracle.close()

        for i, prefix in enumerate(keys):
            node = RustValidator(args, fixture, prefix, f"rust{i}", ports[2*i:2*i+2])
            node.node_key = fixture / f"{prefix}_node_key.json"
            node.peer = f"{peer_ids[prefix]}@{listen(node.p2p)}"
            rust.append(node)
        held = [i for i, node in enumerate(rust) if node.address in schedule["proposers"][:2]]
        active = [i for i in range(7) if i not in held]
        require(len(held) == 2 and len(active) == 5, "wrong initial withheld validators")
        require(sum(int(v["voting_power"]) for v in validators["validators"]
                    if v["address"].upper() in {rust[i].address for i in active}) * 3
                > schedule["votingPower"] * 2, "five online validators lack a quorum")
        for i, node in enumerate(rust):
            for j in range(i):
                link = TcpLink(rust[j].p2p)
                links[f"r{i}-r{j}"] = link
                node.outgoing.append(f"{peer_ids[keys[j]]}@127.0.0.1:{link.port}")
        record("two-proposers-withheld", height=first, withheld=held,
               proposerOrder=schedule["proposers"][:3], online=active, goProcesses=0)
        for i in active:
            rust[i].launch("initial")
        recovery = rust[active[0]]
        signed = wait_for(lambda: next((row for row in decode_trace(recovery.log)
                                       if row.get("event") == "validator_vote_signed"
                                       and row.get("consensusHeight") == first and row.get("round") == 1
                                       and row.get("voteType") == 2), None),
                          "durable precommit at round one", processes(), timeout=40)
        require(max(heights(active)) == first - 1, "first block committed before the round-one stop")
        recovery.close()
        record("stopped-in-round", node=recovery.name, signed=signed, height=first)
        recovery.launch("round-recovered")
        restored = f"Restored Tendermint signing state at {first}/1 step 6"
        require(restored in logs(recovery.log), "round-one precommit was not restored")
        record("round-restored", node=recovery.name, message=restored)
        wait_for(lambda: min(heights(active)) >= first + 2, "five Rust validators committed after round two",
                 processes(), timeout=90)
        join_boundary = max(heights(active))
        for i in held:
            rust[i].launch("joined-later")
        compare_through(join_boundary + 24)
        healthy_through = max(compared)
        wait_for(ready_links, "all 21 current sessions exchanged consensus messages in both directions", processes())
        record("seven-rust-validators", after=join_boundary, through=healthy_through,
               links={name: link.stats() for name, link in links.items()})
        raw = source["transactions"]["transfer"]
        reply = get(rust[0].api, f"/v2/send_transaction/0x{raw}")
        require(reply["status"] == 200, reply)
        tx_height = wait_for(lambda: next((h for h, row in rust[0].rows().items() if row["transactions"]), None),
                             "seven-validator transfer", processes())
        compare_through(tx_height + 1)
        through = max(compared)
        wait_for(lambda: min(heights(range(7))) >= through + 3, "later commit witnesses", processes())
        for node in rust:
            node.close()
        archive_tips = heights(range(7))
        for link in links.values():
            link.close()
        record("rust-validation-finished", through=through, witnessArchiveTips=archive_tips, goProcessesDuringConsensus=0)

        archive = rust[active[0]]
        archive.log = archive.root / "archive-only.log"
        archive.process = start([args.daemon, "native-sync", "--db", archive.root / "state",
                                 "--p2p-listen", listen(archive.p2p), "--node-key", archive.node_key], archive.log)
        wait_for(lambda: "Tendermint 0.34 P2P listening on" in logs(archive.log), "archive server", processes())
        # The verifier has a fresh app lifetime: it must not reuse an app that
        # was stopped between InitChain and its first Commit.
        oracle = Node(args.output / "go-verifier", args.tendermint, args.go_app, fixture, ports[14:], False)
        oracle.launch(None, "completed-chain", use_rust=False,
                      engine_args=("--p2p.persistent_peers", archive.peer))
        wait_for(lambda: oracle.height() > through, "Go replayed completed round-two chain",
                 oracle.processes + processes(), timeout=120)
        attestations = {}
        for h, row in compared.items():
            signed = oracle.rpc("commit", height=h)["signed_header"]
            following = oracle.rpc("commit", height=h+1)["signed_header"]["header"]
            (args.output / f"commit-{h}.json").write_text(json.dumps(signed) + "\n")
            header, commit = signed["header"], signed["commit"]
            results = oracle.rpc("block_results", height=h)
            path = args.output / f"block-results-{h}.json"
            path.write_text(json.dumps(results) + "\n")
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
                    and row["validatorUpdates"] == end["validatorUpdates"], f"Go differs at {h}: {row}")
            attestations[h] = {"round": int(commit["round"]), "proposer": header["proposer_address"].upper(),
                               "signers": [s["validator_address"].upper() for s in commit["signatures"]
                                           if s["block_id_flag"] == 2 and s["signature"]]}
        first_commit = attestations[first]
        require(2 <= first_commit["round"] < len(schedule["proposers"]), "round two was not committed")
        require(first_commit["proposer"] == schedule["proposers"][first_commit["round"]],
                "first proposer differs from Go priorities")
        require(set(first_commit["signers"]) == {rust[i].address for i in active},
                "first commit must include the five available validators")
        require(recovery.address in first_commit["signers"], "recovered signer did not sign the first commit")
        for node in rust:
            require(any(join_boundary < h <= healthy_through and node.address in item["signers"] for h, item in attestations.items())
                    and any(join_boundary < h <= healthy_through and node.address == item["proposer"] for h, item in attestations.items()),
                    f"{node.name} did not sign and propose after all seven joined")
        digest = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
        receipt = oracle.rpc("tx", hash="0x"+digest)
        require(int(receipt["height"]) == tx_height and receipt["tx_result"].get("code", 0) == 0, receipt)
        require(compared[tx_height]["deliverResponsesSha256"] == delivery_digest(receipt["tx_result"]),
                "transaction result differs")
        record("go-replay-matched", blocks=len(compared), firstRound=first_commit["round"], through=through)
        return {"platform": platform.platform(), "validators": 7, "withheld": held, "active": active,
                "firstCommit": first_commit, "restored": restored, "compared": list(compared.values()),
                "attestations": attestations, "phases": phases, "transactionHeight": tx_height, "transactionHash": digest,
                "comparedFrom": first, "comparedThrough": through, "witnessArchiveTips": archive_tips}
    finally:
        oracle.close()
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
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k: v for k, v in result.items() if k not in ("compared", "attestations", "phases")}))
