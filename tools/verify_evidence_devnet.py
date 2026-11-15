"""Send signed duplicate-vote evidence through Tendermint in a mixed Minter devnet."""

import argparse
import base64
import copy
import json
from pathlib import Path
import platform
import subprocess
import time
from types import SimpleNamespace
import urllib.request

from verify_minter_api_v2 import listen, rpc, verify_queries
from verify_minter_events import case
from verify_minter_subscribe import Probes, expect
from verify_send_devnet import HTTP, HIDE_WINDOW, Node, allocate_ports, logs, require, signed_root, start, stop, wait_for


def broadcast(port, evidence):
    call = {"id": 1, "jsonrpc": "2.0", "method": "broadcast_evidence", "params": {"evidence": evidence}}
    request = urllib.request.Request(f"http://{listen(port)}/", json.dumps(call).encode(),
                                     {"Content-Type": "application/json"})
    with HTTP.open(request, timeout=10) as response:
        return json.load(response)


def evidence_hash(reply):
    return base64.b64decode(reply.get("result", {}).get("hash", ""), validate=True).hex().upper()


def verify(args):
    fixture = args.output / "fixture"
    subprocess.run([args.fixture_tool, "genesis", args.base_fixture, fixture], check=True, timeout=20,
                   creationflags=HIDE_WINDOW)
    genesis = json.loads((fixture / "genesis.json").read_bytes())
    first = int(genesis["initial_height"])
    ports = allocate_ports(9)
    rust_ports = SimpleNamespace(rpc=ports[1], http=ports[3], grpc=ports[4])
    go_ports = SimpleNamespace(rpc=ports[5], p2p=ports[6], grpc=ports[7], http=ports[8])
    rust = Node(args.output / "rust", args.tendermint, args.daemon, fixture, ports[:3], False)
    log = args.output / "go.log"
    oracle = start([args.oracle, "serve", args.output / "go", fixture, listen(go_ports.rpc),
                    listen(go_ports.p2p), listen(go_ports.grpc), listen(go_ports.http), "0"], log)
    probes = Probes(args.oracle, args.output)
    flags = ["--api-listen", listen(rust_ports.http), "--grpc-listen", listen(rust_ports.grpc),
             "--tendermint-rpc", listen(rust_ports.rpc)]

    def wait_height(height):
        return wait_for(lambda: min(rust.height(), int(peer.rpc("status")["sync_info"]["latest_block_height"]))
                        >= height, f"both nodes at {height}", rust.processes + [(oracle, log)])

    try:
        info = wait_for(lambda: rpc(go_ports.rpc, "status"), "Go Tendermint RPC", [(oracle, log)])
        peer = SimpleNamespace(p2p=go_ports.p2p, node_id=info["node_info"]["id"],
                               rpc=lambda method, **options: rpc(go_ports.rpc, method, **options))
        rust.launch(peer, "initial", use_rust=True, app_args=flags)
        wait_height(first + 1)

        validators = peer.rpc("validators", height=first)
        require(len(validators["validators"]) == 2, validators)
        block = args.output / "source-block.json"
        valset = args.output / "source-validators.json"
        block.write_text(json.dumps(peer.rpc("block", height=first)), encoding="utf-8")
        valset.write_text(json.dumps(validators), encoding="utf-8")
        proof = json.loads(subprocess.check_output([args.fixture_tool, "vote", block, valset], timeout=15,
                                                   creationflags=HIDE_WINDOW))
        evidence = proof["evidence"]
        invalid = copy.deepcopy(evidence)
        signature = bytearray(base64.b64decode(invalid["value"]["vote_b"]["signature"], validate=True))
        signature[0] ^= 1
        invalid["value"]["vote_b"]["signature"] = base64.b64encode(signature).decode()
        refused = broadcast(rust_ports.rpc, invalid)
        require("invalid signature" in refused.get("error", {}).get("data", "").lower(), refused)

        subscriptions = {
            "go": probes.start("live-go", "grpc", go_ports, "tm.event = 'NewEvidence'"),
            "rust": probes.start("live-rust", "ws", rust_ports, "tm.event = 'NewEvidence'"),
        }
        children = rust.processes + [(oracle, log)]
        wait_for(lambda: all('"ready":true' in logs(probe.log) for probe in subscriptions.values()),
                 "evidence subscribers", children)
        time.sleep(0.4)
        before = rust.height()
        accepted = broadcast(rust_ports.rpc, evidence)
        require(evidence_hash(accepted) == proof["hash"], accepted)

        def included():
            head = int(peer.rpc("status")["sync_info"]["latest_block_height"])
            for height in range(before + 1, head + 1):
                observed = peer.rpc("block", height=height)["block"]["evidence"]["evidence"] or []
                if evidence in observed:
                    return height
            return None

        height = wait_for(included, "evidence gossiped and committed by Go", children)
        wait_height(height + 3)
        rust_block = rust.rpc("block", height=height)["block"]
        require(evidence in rust_block["evidence"]["evidence"], "Rust companion lacks the evidence")
        require(rust.rpc("block_results", height=height) == peer.rpc("block_results", height=height),
                "EndBlock consensus responses differ")
        root = signed_root(rust, peer, height + 1)
        after = rust.rpc("validators", height=height + 2)
        require(after == peer.rpc("validators", height=height + 2) and len(after["validators"]) == 1,
                after)
        require(after["validators"][0]["address"] != evidence["value"]["vote_a"]["validator_address"],
                "the punished validator stayed in the set")

        live = {node: expect(probe.result())["messages"] for node, probe in subscriptions.items()}
        require(live["go"] == live["rust"] and len(live["rust"]) == 1
                and live["rust"][0]["data"]["height"] == height, live)
        repeat = broadcast(rust_ports.rpc, evidence)
        require(evidence_hash(repeat) == proof["hash"], repeat)
        repeat_head = rust.height()
        wait_height(repeat_head + 2)

        rust.close()
        rust.launch(peer, "restarted", use_rust=True, app_args=flags)
        wait_height(repeat_head + 3)
        cases = [case(height), case(height, Search=[proof["validator"]]),
                 {"Method": "Block", "Height": height, "Fields": [6], "Events": True, "Code": 0},
                 *(case(h) for h in range(height + 1, repeat_head + 3))]
        compared = verify_queries(args, cases, go_ports, rust_ports, "devnet-evidence")
        rows = compared[0]["rustGrpc"]["body"]["events"]
        candidate = next(row for row in genesis["app_state"]["candidates"] if row["public_key"] == proof["validator"])
        require(rows == [{"type": "minter/SlashEvent", "value": {
            "address": candidate["owner_address"], "amount": str(50 * 10**18), "coin": "0",
            "validator_pub_key": proof["validator"],
        }}], rows)
        require(compared[2]["rustGrpc"]["body"]["evidence"]["evidence"] == [evidence], compared[2])
        require(all(not row["rustGrpc"]["body"]["events"] for row in compared[3:]),
                "the committed evidence was applied again")
        return {"platform": platform.platform(), "includedHeight": height, "evidenceHash": proof["hash"],
                "refusedSignature": refused["error"], "signedHeader": root, "validatorSet": after,
                "live": live, "comparisons": compared, "resumedHeight": rust.height()}
    finally:
        probes.close()
        rust.close()
        stop(oracle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "oracle", "tendermint", "fixture_tool", "base_fixture", "output"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: result[k] for k in ("platform", "includedHeight", "signedHeader", "resumedHeight")}))
