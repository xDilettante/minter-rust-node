"""Compare Rust execution of Tendermint consensus traffic with the Go chain."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import time

from verify_minter_api_v2 import get, listen
from verify_mainnet_empty import varint
from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, start, stop, wait_for


def delivery_digest(result):
    def field(tag, data):
        return varint(tag << 3 | 2) + varint(len(data)) + data

    encoded = bytearray()
    for tag, key in ((1, "code"), (5, "gas_wanted"), (6, "gas_used")):
        if value := int(result[key]):
            encoded += varint(tag << 3) + varint(value)
    require(not any(result.get(key) for key in ("data", "log", "info", "codespace")), result)
    for event in result["events"]:
        attributes = bytearray()
        for tag in event["attributes"]:
            attribute = field(1, base64.b64decode(tag["key"])) + field(2, base64.b64decode(tag["value"]))
            attributes += field(2, attribute + (b"\x18\x01" if tag["index"] else b""))
        encoded += field(7, field(1, event["type"].encode()) + attributes)
    return hashlib.sha256(varint(len(encoded)) + encoded).hexdigest()


def verify(args):
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    first = fixture["genesisHeight"] + 1
    ports = allocate_ports(5)
    chain_fixture = args.fixture
    signer_address = None
    if args.validator_fixture_tool:
        chain_fixture = args.output / "fixture"
        subprocess.run([args.validator_fixture_tool, "consensus", args.fixture, chain_fixture],
                       check=True, timeout=20, creationflags=HIDE_WINDOW)
    source = Node(args.output / "go", args.tendermint, args.go_app, chain_fixture, ports[:3], True)
    database = args.output / "rust-state"
    rust = None
    rust_log = None
    command = [args.daemon, "native-sync", "--db", database, "--consensus", "--mempool",
               "--p2p-listen", listen(ports[3]), "--api-listen", listen(ports[4])]
    if args.validator_fixture_tool:
        chain = json.loads((chain_fixture / "genesis.json").read_bytes())["chain_id"]
        signer_address = json.loads((chain_fixture / "priv_validator_key.json").read_bytes())["address"].upper()
        for local, dest in (("key", "config"), ("state", "data")):
            shutil.copyfile(chain_fixture / f"second_validator_{local}.json",
                            source.home / dest / f"priv_validator_{local}.json")
        key, journal = chain_fixture / "priv_validator_key.json", args.output / "signer"
        subprocess.run([args.daemon, "validator-signer", "import", "--key", key, "--journal", journal,
                        "--chain-id", chain, "--go-state", chain_fixture / "priv_validator_state.json"],
                       check=True, timeout=20, creationflags=HIDE_WINDOW)
        command += ["--validator-key", key, "--validator-journal", journal]
    peer = None
    compared = {}
    commit_rounds = {}
    rust_logs = []
    signing_recovery = None

    def launch(phase, initial=False):
        nonlocal rust, rust_log, peer
        rust_log = args.output / f"rust-{phase}.log"
        rust_logs.append(rust_log)
        extra = ["--genesis", chain_fixture / "genesis.json", "--keep-iavl-versions", "4"] if initial else []
        rust = start(command + extra, rust_log)
        node = wait_for(
            lambda: next((line.split()[-1] for line in logs(rust_log).splitlines()
                          if line.startswith("Tendermint 0.34 P2P listening on")), None),
            "native consensus listener", [(rust, rust_log)],
        )
        require(peer in (None, node), "Rust changed P2P identity on restart")
        peer = node

    def rows():
        return {row["height"]: row for path in rust_logs for row in decode_trace(path) if "height" in row}

    def compare_through(height, from_height):
        def reached():
            captured = rows()
            return captured if height in captured else None

        actual = wait_for(reached,
                          f"Rust consensus reached {height}", source.processes + [(rust, rust_log)])
        wait_for(lambda: source.height() > height, "Go signed the next AppHash", source.processes)
        for number in range(from_height, height + 1):
            row = actual[number]
            current = source.rpc("commit", height=number)["signed_header"]
            following = source.rpc("commit", height=number + 1)["signed_header"]
            header, next_header = current["header"], following["header"]
            block_id = current["commit"]["block_id"]
            require(row["appHash"] == following["header"]["app_hash"].lower()
                    and row["blockHash"] == block_id["hash"].lower()
                    and row["partSetHash"] == block_id["parts"]["hash"].lower()
                    and row["parts"] == int(block_id["parts"]["total"])
                    and row["resultsHash"] == next_header["last_results_hash"].lower()
                    and row["validatorsHash"] == header["validators_hash"].lower()
                    and row["nextValidatorsHash"] == header["next_validators_hash"].lower(),
                    f"native consensus differs at {number}")
            compared[number] = row
            commit_rounds[number] = int(current["commit"]["round"])

    def transaction(name):
        raw = fixture["transactions"][name]
        reply = get(ports[4], f"/v2/send_transaction/0x{raw}")
        require(reply["status"] == 200, reply)
        digest = hashlib.sha256(bytes.fromhex(raw)).hexdigest()
        delivered = wait_for(lambda: source.rpc("tx", hash=f"0x{digest}"), "Go executed transaction",
                             source.processes + [(rust, rust_log)])
        require(delivered["tx_result"].get("code", 0) == 0, delivered)
        return int(delivered["height"]), digest, delivery_digest(delivered["tx_result"])

    try:
        launch("initial", initial=True)
        if signer_address:
            # Rust is the first proposer in this fixture. Without Go running it
            # can sign and persist a proposal or prevote, but cannot commit.
            wait_for(lambda: f"Signed Tendermint proposal at {first}/0" in logs(rust_log),
                     "Rust signed before quorum", [(rust, rust_log)])
            require(not rows(), "Rust committed before Go started")
            stop(rust)
            launch("signer-recovered", initial=True)
            signing_recovery = wait_for(
                lambda: next((line for line in logs(rust_log).splitlines()
                              if line.startswith(f"Restored Tendermint signing state at {first}/0 step ")), None),
                "restored proposal or prevote before first commit", [(rust, rust_log)])
        source.launch(None, "validator", use_rust=False,
                      engine_args=("--p2p.persistent_peers", peer))
        compare_through(first + 3, first)
        tx, tx_hash, tx_digest = transaction("transfer")
        compare_through(tx + 1, first)
        require(compared[tx]["transactions"] == 1 and compared[tx]["deliverResponsesSha256"] == tx_digest,
                compared[tx])
        stop(rust)
        history = json.loads(subprocess.check_output(
            [args.cli, "iavl", "history", "--db", database], creationflags=HIDE_WINDOW, timeout=20))
        stopped = history["head"]["height"]
        if signer_address:
            time.sleep(2.0)
            require(source.height() <= stopped + 1, "Go committed without the quorum-critical Rust validator")
        else:
            wait_for(lambda: source.height() >= stopped + 5, "Go advanced while Rust was stopped",
                     source.processes)
        launch("restarted")
        compare_through(stopped + 5, stopped)
        second, second_hash, second_digest = transaction("self")
        compare_through(second + 1, stopped + 1)
        require(compared[second]["transactions"] == 1
                and compared[second]["deliverResponsesSha256"] == second_digest, compared[second])
        rust_proposals = []
        if signer_address:
            for height in (tx, second):
                signed = source.rpc("commit", height=height)["signed_header"]
                require(len(signed["commit"]["signatures"]) == 2
                        and all(signature["block_id_flag"] == 2 and signature["signature"]
                                for signature in signed["commit"]["signatures"]),
                        f"expected both Go and Rust to sign {height}")
            rust_proposals = [height for height in compared if source.rpc("block", height=height)
                              ["block"]["header"]["proposer_address"].upper() == signer_address]
            require(rust_proposals, "Go did not accept any block proposed by Rust")
        return {"platform": platform.platform(), "first": first, "goPeerId": source.node_id,
                "rustPeer": peer, "transactions": {tx: tx_hash, second: second_hash},
                "restartFrom": stopped, "signingRecovery": signing_recovery,
                "rustValidator": signer_address, "rustProposals": rust_proposals,
                "commitRounds": commit_rounds, "compared": list(compared.values())}
    finally:
        stop(rust)
        source.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "tendermint", "go_app", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--validator-fixture-tool", type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: v for k, v in result.items() if k != "compared"}))
    print(f"Compared {len(result['compared'])} blocks")
