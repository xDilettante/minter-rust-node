"""Verify a four-validator local network with two Go and two Rust nodes."""

import argparse
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import time

from tcp_link import TcpLink
from verify_minter_api_v2 import get, listen
from verify_native_consensus import delivery_digest
from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, set_toml, start, stop, wait_for


class RustValidator:
    def __init__(self, args, fixture, prefix, name, ports):
        self.args, self.fixture, self.name = args, fixture, name
        self.root = args.output / name
        self.root.mkdir()
        self.p2p, self.api = ports
        self.key = fixture / f"{prefix}_key.json"
        self.address = json.loads(self.key.read_bytes())["address"].upper()
        self.journal = self.root / "signer"
        chain = json.loads((fixture / "genesis.json").read_bytes())["chain_id"]
        subprocess.run([args.daemon, "validator-signer", "import", "--key", self.key,
                        "--journal", self.journal, "--chain-id", chain,
                        "--go-state", fixture / f"{prefix}_state.json"],
                       check=True, timeout=20, creationflags=HIDE_WINDOW)
        self.process = self.log = self.peer = None
        self.traces = []
        self.outgoing = []
        self.node_key = None

    def launch(self, phase):
        self.log = self.root / f"{phase}.log"
        self.traces.append(self.log)
        command = [
            self.args.daemon, "native-sync", "--db", self.root / "state",
            "--genesis", self.fixture / "genesis.json", "--keep-iavl-versions", "4",
            "--consensus", "--mempool", "--p2p-listen", listen(self.p2p),
            "--api-listen", listen(self.api), "--validator-key", self.key,
            "--validator-journal", self.journal,
        ]
        for peer in self.outgoing:
            command += ["--consensus-peer", peer]
        if self.node_key:
            command += ["--node-key", self.node_key]
        self.process = start(command, self.log)
        peer = wait_for(lambda: next((line.split()[-1] for line in logs(self.log).splitlines()
                                    if line.startswith("Tendermint 0.34 P2P listening on")), None),
                        self.name + " listener", self.processes())
        require(self.peer in (None, peer), self.name + " changed P2P identity")
        self.peer = peer

    def processes(self):
        return [(self.process, self.log)] if self.process is not None else []

    def rows(self):
        rows = {}
        for path in self.traces:
            for row in decode_trace(path):
                if "height" in row:
                    if row["height"] in rows:
                        require(rows[row["height"]] == row, "different execution after restart")
                    rows[row["height"]] = row
        return rows

    def close(self):
        stop(self.process)
        self.process = None


def verify(args):
    fixture = args.output / "fixture"
    subprocess.run([args.fixture_tool, "multinode", args.fixture, fixture],
                   check=True, timeout=20, creationflags=HIDE_WINDOW)
    source = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    first = source["genesisHeight"] + 1
    ports = allocate_ports(10)
    go = [Node(args.output / f"go{i}", args.tendermint, args.go_app, fixture,
               ports[i * 3:i * 3 + 3], True) for i in range(2)]
    rust = [RustValidator(args, fixture, prefix, f"rust{i}", ports[6 + i * 2:8 + i * 2])
            for i, prefix in enumerate(("priv_validator", "third_validator"))]
    for i, prefix in enumerate(("second_validator", "fourth_validator")):
        for kind, folder in (("key", "config"), ("state", "data")):
            shutil.copyfile(fixture / f"{prefix}_{kind}.json",
                            go[i].home / folder / f"priv_validator_{kind}.json")
        config = go[i].home / "config/config.toml"
        config.write_text(set_toml(logs(config), "p2p",
                                  {"persistent_peers_max_dial_period": '"500ms"'}), encoding="utf-8")
    links, peers, compared, events, attestations = {}, {}, {}, [], {}

    def processes():
        return [item for node in go for item in node.processes] + [
            item for node in rust for item in node.processes()]

    def record(phase, **data):
        event = {"phase": phase, **data}
        events.append(event)
        with (args.output / "phases.jsonl").open("a", encoding="utf-8") as file:
            file.write(json.dumps(event) + "\n")
        print(json.dumps(event), flush=True)

    def heights():
        return [node.height() for node in go] + [max(node.rows(), default=first - 1) for node in rust]

    def wait_live(height, go_indices=(0, 1), rust_indices=(0, 1)):
        def reached():
            values = [go[i].height() for i in go_indices] + [
                max(rust[i].rows(), default=first - 1) for i in rust_indices]
            return values if min(values) >= height else None
        return wait_for(reached, f"online validators reached {height}", processes(), timeout=90)

    def compare_through(height):
        wait_for(lambda: min(heights()) > height, f"all four nodes reached {height + 1}",
                 processes(), timeout=90)
        for number in range(first, height + 1):
            if number in compared:
                continue
            signed = [node.rpc("commit", height=number)["signed_header"] for node in go]
            following = [node.rpc("commit", height=number + 1)["signed_header"] for node in go]
            require(signed[0] == signed[1] and following[0]["header"] == following[1]["header"],
                    f"Go signed headers differ at {number}")
            header, commit = signed[0]["header"], signed[0]["commit"]
            next_header = following[0]["header"]
            results = [node.rpc("block_results", height=number) for node in go]
            require(results[0] == results[1], f"Go ABCI differs at {number}")
            path = args.output / f"block-results-{number}.json"
            path.write_text(json.dumps(results[0]) + "\n", encoding="utf-8")
            end = json.loads(subprocess.check_output([args.fixture_tool, "end-block", path],
                                                    timeout=15, creationflags=HIDE_WINDOW))
            rows = [node.rows()[number] for node in rust]
            require(rows[0] == rows[1], f"Rust execution differs at {number}")
            row = rows[0]
            require(row["appHash"] == next_header["app_hash"].lower()
                    and row["blockHash"] == commit["block_id"]["hash"].lower()
                    and row["partSetHash"] == commit["block_id"]["parts"]["hash"].lower()
                    and row["parts"] == int(commit["block_id"]["parts"]["total"])
                    and row["resultsHash"] == next_header["last_results_hash"].lower()
                    and row["validatorsHash"] == header["validators_hash"].lower()
                    and row["nextValidatorsHash"] == header["next_validators_hash"].lower()
                    and row["endResponseSha256"] == end["endResponseSha256"]
                    and row["validatorUpdates"] == end["validatorUpdates"],
                    f"Rust differs from Go at {number}: {row}")
            compared[number] = row
            attestations[number] = {
                "round": int(commit["round"]), "proposer": header["proposer_address"].upper(),
                "signers": [signature["validator_address"].upper() for signature in commit["signatures"]
                            if signature["block_id_flag"] == 2 and signature["signature"]],
            }
        record("compared", through=height, heights=heights())

    def launch_go(index, phase):
        go[index].launch(None, phase, use_rust=False,
                         engine_args=("--p2p.persistent_peers", ",".join(peers[index])))

    def confirm_rejoined(address, phase):
        boundary = max(heights())
        compare_through(boundary + 8)
        signed = [height for height, item in attestations.items()
                  if height > boundary and address in item["signers"]]
        proposed = [height for height, item in attestations.items()
                    if height > boundary and item["proposer"] == address]
        require(signed and proposed, f"{phase} returned without new votes and proposals")
        record(phase, after=boundary, signed=signed, proposed=proposed)

    try:
        for node in rust:
            node.launch("initial")
        for i in range(2):
            peers[i] = []
            targets = [(f"r{j}", node.p2p, node.peer.split("@")[0]) for j, node in enumerate(rust)]
            targets.append((f"g{1-i}", go[1-i].p2p, go[1-i].node_id))
            for name, target, identity in targets:
                link = TcpLink(target)
                links[f"g{i}-{name}"] = link
                peers[i].append(f"{identity}@127.0.0.1:{link.port}")
        for i in range(2):
            launch_go(i, "initial")
        compare_through(first + 5)
        validators = go[0].rpc("validators", height=first)["validators"]
        require(len(validators) == 4 and {int(v["voting_power"]) for v in validators} == {25000000},
                validators)
        expected = {node.address for node in rust} | {
            json.loads((fixture / f"{prefix}_key.json").read_bytes())["address"].upper()
            for prefix in ("second_validator", "fourth_validator")}
        require({v["address"].upper() for v in validators} == expected, "unexpected validator identities")
        raw = source["transactions"]["transfer"]
        reply = get(rust[0].api, f"/v2/send_transaction/0x{raw}")
        require(reply["status"] == 200, reply)
        digest = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
        receipt = wait_for(lambda: go[0].rpc("tx", hash=f"0x{digest}"), "Go executed transfer",
                           processes())
        require(receipt["tx_result"].get("code", 0) == 0, receipt)
        tx_height = int(receipt["height"])
        compare_through(tx_height + 1)
        require(compared[tx_height]["deliverResponsesSha256"] == delivery_digest(receipt["tx_result"]),
                "transaction response differs")
        require({address for item in attestations.values() for address in item["signers"]} == expected,
                "not all four validators signed the healthy chain")
        require({item["proposer"] for item in attestations.values()} == expected,
                "not all four validators proposed a committed healthy block")
        record("healthy", transactionHeight=tx_height, links={name: link.stats() for name, link in links.items()})
        before_stop = heights()
        go[0].close()
        require(go[0].engine_process.poll() is not None, "Go engine was not stopped")
        target = max(before_stop) + 4
        active = wait_live(target, go_indices=(1,))
        record("one-go-stopped", before=before_stop, online=active)
        launch_go(0, "rejoined")
        compare_through(target)
        go0_address = json.loads((fixture / "second_validator_key.json").read_bytes())["address"].upper()
        confirm_rejoined(go0_address, "go-rejoined")
        crossing = ["g0-r1", "g1-r0", "g0-g1", "g1-g0"]
        for name in crossing:
            links[name].set_enabled(False)
        wait_for(lambda: all(not links[name].stats()["connections"]
                             and not links[name].stats()["pending"] for name in crossing),
                 "all cross-group sockets closed", processes())
        record("split-2-2", heights=heights(), links={name: links[name].stats() for name in crossing})
        last, stable_since = heights(), time.monotonic()

        def settled():
            nonlocal last, stable_since
            current = heights()
            if current != last:
                last, stable_since = current, time.monotonic()
            return current if time.monotonic() - stable_since >= 2 else None

        stopped_at = wait_for(settled, "in-flight commits settled after partition", processes(), timeout=15)
        for _ in range(20):
            time.sleep(0.25)
            require(all(process.poll() is None for process, _ in processes()),
                    "a process exited during partition")
            require(heights() == stopped_at, "a half-weight partition committed another block")
        record("split-stalled", seconds=5, heights=stopped_at)
        for name in crossing:
            links[name].set_enabled(True)
        target = max(stopped_at) + 4
        compare_through(target)
        record("network-healed", through=target, links={name: links[name].stats() for name in crossing})
        before_stop = heights()
        rust[0].close()
        target = max(before_stop) + 4
        active = wait_live(target, rust_indices=(1,))
        record("one-rust-stopped", before=before_stop, online=active)
        rust[0].launch("rejoined")
        compare_through(target)
        confirm_rejoined(rust[0].address, "rust-rejoined")
        raw = source["transactions"]["self"]
        reply = get(rust[0].api, f"/v2/send_transaction/0x{raw}")
        require(reply["status"] == 200, reply)
        second_digest = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
        receipt = wait_for(lambda: go[0].rpc("tx", hash=f"0x{second_digest}"),
                           "Go executed transfer after Rust restart", processes())
        require(receipt["tx_result"].get("code", 0) == 0, receipt)
        second_height = int(receipt["height"])
        compare_through(second_height + 1)
        require(compared[second_height]["deliverResponsesSha256"] == delivery_digest(receipt["tx_result"]),
                "response after restart differs")
        record("transaction-after-restart", height=second_height)
        before_change = go[0].rpc("validators", height=second_height + 1)["validators"]
        require({v["address"].upper() for v in before_change} == expected,
                "validator set changed before the planned proof")
        block = args.output / "proof-block.json"
        validators = args.output / "proof-validators.json"
        block.write_text(json.dumps(go[0].rpc("block", height=first)) + "\n", encoding="utf-8")
        validators.write_text(json.dumps(go[0].rpc("validators", height=first)) + "\n", encoding="utf-8")
        proof = json.loads(subprocess.check_output([args.fixture_tool, "vote", block, validators],
                                                  timeout=15, creationflags=HIDE_WINDOW))
        accepted, evidence_hash = proof["nilEvidence"], proof["nilHash"].lower()
        evidence_file = args.output / "proof.json"
        evidence_file.write_text(json.dumps(accepted) + "\n", encoding="utf-8")
        start_height = max(heights())
        subprocess.run([args.fixture_tool, "relay", rust[0].peer, evidence_file],
                       check=True, timeout=12, creationflags=HIDE_WINDOW)
        wait_for(lambda: f"Accepted Tendermint evidence {evidence_hash}" in logs(rust[0].log),
                 "Rust accepted the historical four-validator proof", processes())

        def included():
            for height in range(start_height + 1, go[0].height() + 1):
                items = go[0].rpc("block", height=height)["block"]["evidence"]["evidence"] or []
                if accepted in items:
                    return height
            return None

        inclusion_height = wait_for(included, "four-validator network committed the proof", processes())
        compare_through(inclusion_height + 4)
        survivors = expected - {go0_address}
        activation = {}
        for height in range(inclusion_height, inclusion_height + 4):
            sets = [node.rpc("validators", height=height)["validators"] for node in go]
            reduced = [[(v["address"].upper(), int(v["voting_power"])) for v in values] for values in sets]
            require(reduced[0] == reduced[1], f"Go validator sets differ at {height}")
            required = expected if height < inclusion_height + 2 else survivors
            require({address for address, _ in reduced[0]} == required,
                    f"validator removal not applied at H+2: {height}")
            activation[height] = reduced[0]
            if height >= inclusion_height + 2:
                require(set(attestations[height]["signers"]) <= survivors,
                        "removed validator appears in a new-set commit")
        require(compared[inclusion_height]["validatorUpdates"] == 4,
                "expected removal plus three updated voting powers")
        record("validator-change", height=inclusion_height, evidenceHash=evidence_hash, sets=activation)
        rust[0].close()
        rust[0].launch("after-validator-change")
        subprocess.run([args.fixture_tool, "relay", rust[0].peer, evidence_file],
                       check=True, timeout=12, creationflags=HIDE_WINDOW)
        wait_for(lambda: any(row.get("event") == "evidence_already_committed"
                             and row.get("evidenceHash") == evidence_hash
                             for row in decode_trace(rust[0].log)),
                 "Rust confirmed the persisted committed-evidence record", processes())
        final_height = max(heights()) + 4
        compare_through(final_height)
        require(f"Accepted Tendermint evidence {evidence_hash}" not in logs(rust[0].log),
                "committed proof was accepted after restart")
        for height in range(inclusion_height + 1, final_height + 1):
            items = go[0].rpc("block", height=height)["block"]["evidence"]["evidence"] or []
            require(accepted not in items and compared[height]["validatorUpdates"] == 0,
                    f"proof or validator update repeated at {height}")
        record("committed-proof-restored", through=final_height)
        return {"platform": platform.platform(), "validators": 4, "goNodes": 2, "rustNodes": 2,
                "transactions": {tx_height: digest, second_height: second_digest}, "compared": list(compared.values()),
                "attestations": attestations, "phases": events, "finalValidators": sorted(survivors),
                "evidenceHash": evidence_hash, "includedHeight": inclusion_height,
                "activationHeight": inclusion_height + 2}
    finally:
        for node in go:
            node.close()
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
    print(json.dumps({key: value for key, value in result.items() if key != "compared"}))
