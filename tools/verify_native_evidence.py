"""Verify evidence gossip and punishment across native Rust and Go Tendermint 0.34."""

import argparse
import base64
import copy
import json
from pathlib import Path
import platform
import shutil
import subprocess

from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, set_toml, start, stop, wait_for


def verify(args):
    chain = args.output / "fixture"
    mode = "light-genesis" if args.light_attack or args.rust_proposer else "genesis"
    subprocess.run([args.fixture_tool, mode, args.fixture, chain],
                   check=True, timeout=20, creationflags=HIDE_WINDOW)
    genesis = json.loads((chain / "genesis.json").read_bytes())
    first = int(genesis["initial_height"])
    ports = allocate_ports(4)
    go = Node(args.output / "go", args.tendermint, args.go_app, chain, ports[:3], True)
    signer_flags = []
    signer_address = None
    if args.rust_proposer:
        for file, folder in (("key", "config"), ("state", "data")):
            shutil.copyfile(chain / f"second_validator_{file}.json",
                            go.home / folder / f"priv_validator_{file}.json")
        key = chain / "priv_validator_key.json"
        signer_address = json.loads(key.read_bytes())["address"].upper()
        journal = args.output / "signer"
        subprocess.run([args.daemon, "validator-signer", "import", "--key", key,
                        "--journal", journal, "--chain-id", genesis["chain_id"],
                        "--go-state", chain / "priv_validator_state.json"],
                       check=True, timeout=20, creationflags=HIDE_WINDOW)
        signer_flags = ["--validator-key", key, "--validator-journal", journal]
    if args.conflicting_votes:
        config = go.home / "config/config.toml"
        config.write_text(set_toml(logs(config), "consensus", {"timeout_commit": '"2500ms"'}),
                          encoding="utf-8")
    db = args.output / "state"
    rust, rust_log, peer = None, None, None
    traces = []
    compact_before_proof = None

    def launch(phase, initial=False, signing=True):
        nonlocal rust, rust_log, peer
        rust_log = args.output / f"rust-{phase}.log"
        traces.append(rust_log)
        flags = ["--genesis", chain / "genesis.json", "--keep-iavl-versions", "4"] if initial else []
        if initial and args.keep_native_blocks:
            flags += ["--keep-native-blocks", str(args.keep_native_blocks)]
        daemon = args.legacy_daemon if initial and args.legacy_daemon else args.daemon
        rust = start([daemon, "native-sync", "--db", db, "--consensus",
                      "--p2p-listen", f"127.0.0.1:{ports[3]}", *flags,
                      *(signer_flags if signing else [])], rust_log)
        address = wait_for(lambda: next((line.split()[-1] for line in logs(rust_log).splitlines()
                                       if line.startswith("Tendermint 0.34 P2P listening on")), None),
                           "Rust P2P listener", [(rust, rust_log)])
        require(peer in (None, address), "Rust peer identity changed")
        peer = address

    def rows():
        return {row["height"]: row for path in traces for row in decode_trace(path) if "height" in row}

    def wait_height(height):
        return wait_for(lambda: rows().get(height), f"Rust committed height {height}",
                        go.processes + [(rust, rust_log)])

    def write(name, value):
        path = args.output / f"{name}.json"
        path.write_text(json.dumps(value) + "\n", encoding="utf-8")
        return path

    def relay(mode, path):
        subprocess.run([args.fixture_tool, mode, peer, path],
                       check=True, timeout=12, creationflags=HIDE_WINDOW)

    def check_hashes(height):
        row = wait_height(height)
        wait_for(lambda: go.height() > height, "Go signed successor", go.processes)
        current = go.rpc("commit", height=height)["signed_header"]
        following = go.rpc("commit", height=height + 1)["signed_header"]
        require(row["appHash"] == following["header"]["app_hash"].lower()
                and row["blockHash"] == current["commit"]["block_id"]["hash"].lower()
                and row["partSetHash"] == current["commit"]["block_id"]["parts"]["hash"].lower()
                and row["parts"] == int(current["commit"]["block_id"]["parts"]["total"])
                and row["resultsHash"] == following["header"]["last_results_hash"].lower()
                and row["validatorsHash"] == current["header"]["validators_hash"].lower()
                and row["nextValidatorsHash"] == current["header"]["next_validators_hash"].lower(), row)
        results = write(f"block-results-{height}", go.rpc("block_results", height=height))
        end = json.loads(subprocess.check_output([args.fixture_tool, "end-block", results],
                                                timeout=15, creationflags=HIDE_WINDOW))
        require(row["endResponseSha256"] == end["endResponseSha256"]
                and row["validatorUpdates"] == end["validatorUpdates"],
                f"EndBlock differs from Go at {height}: {row} != {end}")
        return row

    try:
        launch("initial", initial=True)
        if args.conflicting_votes:
            vote_height = first + 1
            # Do not relay any evidence object. Two signed Go votes reach Rust
            # before the Go process starts, for a height that has yet to commit.
            relay("relay-votes", write("vote-height", str(first)))
            wait_for(lambda: f"Observed conflicting Tendermint votes at {first}/0" in logs(rust_log),
                     "Rust detected conflicting votes", [(rust, rust_log)])
            go.launch(None, "initial", use_rust=False, engine_args=("--p2p.persistent_peers", peer))
            wait_height(first)
            def found():
                for height in range(vote_height, go.height() + 1):
                    evidence = go.rpc("block", height=height)["block"]["evidence"]["evidence"] or []
                    for item in evidence:
                        if item["type"] == "tendermint/DuplicateVoteEvidence":
                            return height, item
                return None
            height, item = wait_for(found, "Go included evidence formed by Rust",
                                    go.processes + [(rust, rust_log)])
            votes = item["value"]["vote_a"], item["value"]["vote_b"]
            require(all(int(vote["height"]) == first and vote["type"] == 1 for vote in votes), votes)
            require(not votes[0]["block_id"]["hash"] and votes[1]["block_id"]["hash"], votes)
            committed = check_hashes(height)
            check_hashes(height + 2)
            return {"platform": platform.platform(), "infractionHeight": first, "includedHeight": height,
                    "conflictingVotesDetected": True, "committed": committed, "evidence": item}
        go.launch(None, "initial", use_rust=False, engine_args=("--p2p.persistent_peers", peer))
        wait_height(first + 6)
        wait_for(lambda: go.height() >= first + 6, "Go caught up before exporting the proof context",
                 go.processes + [(rust, rust_log)])
        if args.legacy_daemon:
            stop(rust)
            launch("upgraded-legacy")
        block = write("infraction-block", go.rpc("block", height=first))
        validators = write("infraction-validators", go.rpc("validators", height=first))
        kind = ("light-short-nil" if args.light_short_nil else "light") if args.light_attack else "vote"
        proof = json.loads(subprocess.check_output([args.fixture_tool, kind, block, validators],
                                                   timeout=15, creationflags=HIDE_WINDOW))
        accepted = proof["evidence"] if args.light_attack else proof["nilEvidence"]
        evidence_hash = (proof["hash"] if args.light_attack else proof["nilHash"]).lower()
        write("proof", proof)
        bad = copy.deepcopy(accepted)
        if args.light_attack:
            signatures = bad["value"]["ConflictingBlock"]["signed_header"]["commit"]["signatures"]
            signed = next(signature for signature in signatures if signature["block_id_flag"] == 2)
        else:
            signed = bad["value"]["vote_b"]
        signature = bytearray(base64.b64decode(signed["signature"], validate=True))
        signature[0] ^= 1
        signed["signature"] = base64.b64encode(signature).decode()
        relay("relay", write("invalid-signature", bad))
        wait_for(lambda: "signature verification failed" in logs(rust_log).lower()
                 or "signature error" in logs(rust_log).lower() or "invalid signature" in logs(rust_log).lower(),
                 "Rust rejected the forged vote", go.processes + [(rust, rust_log)])

        # With Go stopped, the accepted proof can only enter a block after
        # restarting Rust and relaying its persisted pending evidence.
        go.close()
        if args.rust_proposer:
            # Keep the signer journal, but accept the proof in an observer
            # process so it cannot be committed before the pending restart.
            stop(rust)
            if args.keep_native_blocks:
                compact_before_proof = json.loads(subprocess.check_output(
                    [args.daemon, "native-history", "status", "--db", db],
                    timeout=30, creationflags=HIDE_WINDOW))
                require(compact_before_proof["archiveFrom"] > first
                        and compact_before_proof["contextFrom"] <= first
                        and compact_before_proof["keepNativeBlocks"] == args.keep_native_blocks,
                        "infraction body was not replaced by compact history")
                write("before-proof-retention", compact_before_proof)
            launch("pending-only", signing=False)
        relay("relay", write("valid-evidence", accepted))
        wait_for(lambda: f"Accepted Tendermint evidence {evidence_hash}" in logs(rust_log),
                 "verified and persisted pending evidence", [(rust, rust_log)])
        stopped = max(rows())
        stop(rust)
        launch("recovered-pending")
        local_inclusion = None
        if args.rust_proposer:
            # No Go engine is running here. The Rust signer has 80% power;
            # the offline Go signer has the remaining 20%.
            local_inclusion = wait_for(
                lambda: next((height for height, row in sorted(rows().items())
                              if height > stopped and row["validatorUpdates"] > 0), None),
                "Rust proposed and committed the persisted proof without Go",
                [(rust, rust_log)],
            )
        go.launch(None, "restarted", use_rust=False, engine_args=("--p2p.persistent_peers", peer))

        def included():
            for height in range(stopped + 1, go.height() + 1):
                items = go.rpc("block", height=height)["block"]["evidence"]["evidence"] or []
                if accepted in items:
                    return height
            return None

        height = wait_for(included, "Go committed evidence relayed by Rust after restart",
                          go.processes + [(rust, rust_log)])
        if args.rust_proposer:
            header = go.rpc("block", height=height)["block"]["header"]
            require(height == local_inclusion and header["proposer_address"].upper() == signer_address,
                    "the proof was not in the block proposed by Rust while Go was stopped")
        if args.light_attack and not args.rust_proposer:
            # The old set still signs H+1. Let the remaining validator take
            # over at H+2, when the removal becomes effective.
            wait_height(height + 1)
            go.close()
            for file, folder in (("key", "config"), ("state", "data")):
                shutil.copyfile(chain / f"second_validator_{file}.json",
                                go.home / folder / f"priv_validator_{file}.json")
            go.launch(None, "remaining-validator", use_rust=False, engine_args=("--p2p.persistent_peers", peer))
        committed = check_hashes(height)
        accused = (accepted["value"]["ByzantineValidators"][0]["address"] if args.light_attack
                   else accepted["value"]["vote_a"]["validator_address"])
        wait_for(lambda: go.height() >= height + 2, "Go reached validator activation at H+2",
                 go.processes + [(rust, rust_log)])
        before_activation = {
            number: go.rpc("validators", height=number)["validators"]
            for number in (height, height + 1)
        }
        require(all(any(validator["address"] == accused for validator in validators)
                    for validators in before_activation.values()),
                "validator removal became effective before H+2")
        after = go.rpc("validators", height=height + 2)["validators"]
        if args.light_attack:
            require(committed["validatorUpdates"] > 0 and len(after) == 1
                    and after[0]["address"] != accepted["value"]["ByzantineValidators"][0]["address"], after)
        else:
            require(committed["validatorUpdates"] > 0, "evidence did not change validators")
            require(len(after) == 1 and after[0]["address"] != accepted["value"]["vote_a"]["validator_address"],
                    after)
        check_hashes(height + 3)

        stop(rust)
        launch("recovered-committed")
        before = go.height()
        relay("relay", args.output / "valid-evidence.json")
        check_hashes(before + 3)
        require(f"Accepted Tendermint evidence {evidence_hash}" not in logs(rust_log),
                "Rust accepted already committed evidence again")
        for number in range(height + 1, before + 4):
            items = go.rpc("block", height=number)["block"]["evidence"]["evidence"] or []
            require(accepted not in items, f"evidence replayed at {number}")
        return {"platform": platform.platform(), "infractionHeight": first, "rustStoppedAt": stopped,
                "legacyMigration": bool(args.legacy_daemon),
                "keepNativeBlocks": args.keep_native_blocks,
                "compactBeforeProof": compact_before_proof,
                "wireShortNilAccepted": bool(args.light_short_nil),
                "rustProposer": signer_address, "committedBeforeGoRestart": local_inclusion,
                "includedHeight": height, "evidenceHash": evidence_hash,
                "type": "light-client-lunatic" if args.light_attack else "duplicate-vote", "nilVote": not args.light_attack,
                "invalidSignatureRefused": True, "pendingRecovered": True, "duplicateRefusedAfterRestart": True,
                "endBlockMatchedGo": True, "validatorRemovalAtHPlus2": True,
                "committed": committed, "validatorsBeforeActivation": before_activation,
                "validatorsAtEffectiveHeight": after}
    finally:
        stop(rust)
        go.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "tendermint", "go_app", "fixture_tool", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    cases = parser.add_mutually_exclusive_group()
    cases.add_argument("--conflicting-votes", action="store_true")
    cases.add_argument("--light-attack", action="store_true")
    parser.add_argument("--light-short-nil", action="store_true",
                        help="Use a Go-valid light proof with a short Nil signature")
    parser.add_argument("--legacy-daemon", type=lambda value: Path(value).resolve(),
                        help="Create the pruned database with an older build before upgrading")
    parser.add_argument("--rust-proposer", action="store_true",
                        help="Let Rust propose and commit the persisted proof while Go is stopped")
    parser.add_argument("--keep-native-blocks", type=int, default=0,
                        help="Exercise compact history with a persisted native-body retention policy")
    args = parser.parse_args()
    if args.light_short_nil and not args.light_attack:
        parser.error("--light-short-nil requires --light-attack")
    if args.legacy_daemon and args.conflicting_votes:
        parser.error("--legacy-daemon tests historical proofs; use it without --conflicting-votes")
    if args.rust_proposer and (args.legacy_daemon or args.conflicting_votes):
        parser.error("--rust-proposer tests recovered proposals; use it without --legacy-daemon or --conflicting-votes")
    args.output.mkdir(parents=True)
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
