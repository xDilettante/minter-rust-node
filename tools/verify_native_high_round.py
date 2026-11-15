"""Replay the Rust round-65 recovery test's completed archive using fresh Go nodes.

The Rust test explicitly selects rounds and supplies signed fixture votes. This
checks interoperability of that completed chain, not a timed round-65 network.
"""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess

from verify_native_sync import decode_trace
from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, require, start, stop, wait_for


def verify(args):
    rows = {row["height"]: row for row in decode_trace(args.trace) if "height" in row}
    require(sorted(rows) == list(range(101, 107)), "expected six completed Rust fixture blocks")
    ports = allocate_ports(4)
    archive_log = args.output / "archive.log"
    archive = start([args.daemon, "native-sync", "--db", args.corpus / "state",
                     "--p2p-listen", f"127.0.0.1:{ports[0]}"], archive_log)
    oracle = None
    compared, attestations = [], {}
    try:
        peer = wait_for(lambda: next((line.split()[-1] for line in logs(archive_log).splitlines()
                                     if line.startswith("Tendermint 0.34 P2P listening on")), None),
                        "Rust archive listener", [(archive, archive_log)])
        oracle = Node(args.output / "go-verifier", args.tendermint, args.go_app, args.corpus, ports[1:], False)
        oracle.launch(None, "replay", use_rust=False, engine_args=("--p2p.persistent_peers", peer))
        wait_for(lambda: oracle.height() >= 105, "Go replay through the successor of block 104",
                 oracle.processes + [(archive, archive_log)], timeout=90)
        for h in range(101, 105):
            row = rows[h]
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
                    and row["validatorUpdates"] == end["validatorUpdates"], f"Go differs at {h}")
            require(row["transactions"] == 0, "fixture unexpectedly contains transactions")
            attestations[h] = {"round": int(commit["round"]), "proposer": header["proposer_address"],
                               "signers": [s["validator_address"] for s in commit["signatures"]
                                           if s["block_id_flag"] == 2 and s["signature"]]}
            compared.append(row)
        require(attestations[101]["round"] == 65, "round 65 was not committed")
        require(len(attestations[101]["signers"]) >= 3, "round 65 lacks a quorum")
        return {"platform": platform.platform(), "scenario": "explicit-round fixture and journal reopen",
                "comparedFrom": 101, "comparedThrough": 104, "compared": compared,
                "witnessArchiveTip": 106, "attestations": attestations,
                "genesisSha256": hashlib.sha256((args.corpus / "genesis.json").read_bytes()).hexdigest()}
    finally:
        if oracle:
            oracle.close()
        stop(archive)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "tendermint", "go_app", "fixture_tool", "corpus", "trace", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key not in ("compared", "attestations")}))
