"""Run native Minter block sync against a live peer and a signed Go replay."""

import argparse
import base64
import gzip
import hashlib
import json
from pathlib import Path
import platform
import subprocess

from verify_mainnet_empty import fields, read_varint, validator_updates, varint, wire_fields
from verify_send_devnet import HIDE_WINDOW, require, start, stop, wait_for


def header(response):
    return fields(fields(fields(fields(response)[3])[1])[1])


def encode(fields_):
    data = bytearray()
    for tag, value in fields_:
        if isinstance(value, bytes):
            data += varint(tag << 3 | 2) + varint(len(value)) + value
        else:
            data += varint(tag << 3) + varint(value)
    return bytes(data)


def replace(data, tag, value):
    pairs = list(wire_fields(data))
    require(sum(field == tag for field, _ in pairs) == 1, f"expected one field {tag}")
    return encode((field, value if field == tag else original) for field, original in pairs)


def merkle(leaves):
    if len(leaves) == 1:
        return hashlib.sha256(b"\x00" + leaves[0]).digest()
    split = 1 << ((len(leaves) - 1).bit_length() - 1)
    return hashlib.sha256(b"\x01" + merkle(leaves[:split]) + merkle(leaves[split:])).digest()


def corrupt_commit(response):
    message = fields(response)[3]
    block = fields(message)[1]
    commit_fields = list(wire_fields(fields(block)[4]))
    for i, (tag, value) in enumerate(commit_fields):
        if tag != 4:
            continue
        signature = fields(value).get(4, b"")
        if signature:
            changed = bytes([signature[0] ^ 1]) + signature[1:]
            commit_fields[i] = (tag, replace(value, 4, changed))
            break
    else:
        raise RuntimeError("witness contains no signature")
    # Keep the commit root consistent so rejection requires checking the precommit.
    commit_hash = merkle([value for tag, value in commit_fields if tag == 4])
    block = replace(block, 4, encode(commit_fields))
    block = replace(block, 1, replace(fields(block)[1], 6, commit_hash))
    return replace(response, 3, replace(message, 1, block))


def response_stream(path, first, last):
    result = {}
    with path.open("rb") as source:
        for height in range(first, last + 1):
            size = read_varint(source)
            payload = source.read(size)
            require(len(payload) == size and header(payload)[3] == height,
                    f"unexpected block at {height}")
            result[height] = payload
    return result


def decode_trace(path):
    # A forcibly stopped process may leave one incomplete line.
    data = path.read_bytes()
    data = data[:data.rfind(b"\n") + 1]
    return [json.loads(line) for line in data.splitlines() if line.startswith(b"{")]


def check_trace(rows, reference, responses):
    for row in rows:
        height = row["height"]
        go = reference[height]
        current, following = header(responses[height]), header(responses[height + 1])
        block_id = fields(following[5])
        parts = fields(block_id[2])
        require(row["blockHash"] == go["block_hash"] == block_id[1].hex()
                and row["appHash"] == go["app_hash"] == following[11].hex()
                and row["resultsHash"] == following[12].hex()
                and row["partSetHash"] == parts[2].hex()
                and row["parts"] == parts[1]
                and row["validatorsHash"] == current[8].hex()
                and row["nextValidatorsHash"] == current[9].hex(),
                f"signed block data differs from Go at {height}")
        deliveries = [base64.b64decode(pair["response"] or "")
                      for pair in (go["deliver_tx"] or [])]
        begin = base64.b64decode(go["begin_block"]["request"])
        end = base64.b64decode(go["end_block"]["response"])
        delivery_bytes = b"".join(varint(len(response)) + response for response in deliveries)
        require(row["transactions"] == len(deliveries)
                and row["validatorUpdates"] == len(validator_updates(end))
                and row["beginRequestSha256"] == hashlib.sha256(begin).hexdigest()
                and row["deliverResponsesSha256"] == hashlib.sha256(delivery_bytes).hexdigest()
                and row["endResponseSha256"] == hashlib.sha256(end).hexdigest(),
                f"ABCI transcript differs from Go at {height}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "genesis", "shard", "reference", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("--peer", required=True)
    parser.add_argument("--through", type=int, default=10197365)
    args = parser.parse_args()
    ref = json.loads((args.reference / "result.json").read_bytes())
    first, last = ref["from"], args.through
    live_tip, interrupt = first + 7, first + 80
    require(interrupt < last <= ref["through"], "replay target is outside the test range")
    responses = response_stream(args.shard, first, last + 2)
    reference = {}
    with gzip.open(args.reference / "abci.jsonl.gz", "rt", encoding="utf-8") as source:
        for line in source:
            row = json.loads(line)
            reference[row["height"]] = row
            if row["height"] >= last:
                break

    output = args.output
    output.mkdir()
    db = output / "state"
    command = [args.daemon, "native-sync", "--trace"]

    def run(name, *options, failure=None, database=db):
        log = output / (name + ".log")
        with log.open("wb") as stream:
            result = subprocess.run(
                [*command, "--db", database, *options], stdout=stream, stderr=subprocess.STDOUT,
                creationflags=HIDE_WINDOW, timeout=300,
            )
        if failure is None:
            require(result.returncode == 0, f"{name} failed: {log.read_text()[-1800:]}")
        else:
            require(result.returncode != 0 and failure in log.read_text(),
                    f"{name} did not reject the invalid block: {log.read_text()[-1800:]}")
        return decode_trace(log)

    def history(database=db):
        return json.loads(subprocess.check_output(
            [args.cli, "iavl", "history", "--db", database], creationflags=HIDE_WINDOW, timeout=30,
        ))

    def check_head(height):
        actual = history()
        expected = {"height": height, "appHash": reference[height]["app_hash"]}
        require(actual["head"] == expected and actual["earliestHeight"] == first - 1
                and actual["keepIavlVersions"] == 0, "RocksDB head or archive floor differs from Go")
        return expected

    live = run("live", "--genesis", args.genesis, "--peer", args.peer, "--through", str(live_tip))
    require(live[0]["startingAppHash"] == ref["initialAppHash"], "genesis root differs from Go")
    live_rows = [row for row in live if "height" in row]
    require([row["height"] for row in live_rows] == list(range(first, live_tip + 1)),
            "incomplete live block sequence")
    check_trace(live_rows, reference, responses)
    after_live = check_head(live_tip)
    print(f"Checked live P2P from {first} through {live_tip}", flush=True)

    invalid = output / "invalid-precommit.pb"
    start_height = live_tip + 1
    with invalid.open("wb") as target:
        for height in range(start_height, start_height + 3):
            payload = responses[height]
            if height == start_height + 2:
                payload = corrupt_commit(payload)
            target.write(varint(len(payload)) + payload)
    run("rejected", "--blocks", invalid, "--through", str(start_height), failure="invalid precommit")
    require(check_head(live_tip) == after_live, "invalid precommit advanced RocksDB")
    print(f"Rejected invalid precommit before committing {start_height}", flush=True)

    interrupted_log = output / "interrupted.log"
    process = start([*command, "--db", db, "--blocks", args.shard, "--through", str(last)],
                    interrupted_log)
    try:
        wait_for(
            lambda: any(row.get("height", 0) >= interrupt for row in decode_trace(interrupted_log)),
            "native commit before forced termination", [(process, interrupted_log)],
        )
        require(process.poll() is None, "native replay stopped before the forced termination")
        process.kill()
        require(process.wait(timeout=15) != 0, "native replay did not terminate")
    finally:
        stop(process)
    interrupted = [row for row in decode_trace(interrupted_log) if "height" in row]
    check_trace(interrupted, reference, responses)
    recovered_height = history()["head"]["height"]
    require(interrupt <= recovered_height < last, "unexpected recovered height")
    require([row["height"] for row in interrupted] ==
            list(range(start_height, interrupted[-1]["height"] + 1)),
            "gap in interrupted trace")
    require(0 <= recovered_height - interrupted[-1]["height"] <= 1,
            "recovered state differs from emitted commit records")
    recovered = check_head(recovered_height)
    print(f"Reopened RocksDB after forced termination at {recovered_height}", flush=True)

    resumed = run("resumed", "--blocks", args.shard, "--through", str(last))
    require(resumed[0]["startingHeight"] == recovered_height
            and resumed[0]["startingAppHash"] == recovered["appHash"], "sync did not resume from RocksDB")
    resumed_rows = [row for row in resumed if "height" in row]
    require([row["height"] for row in resumed_rows] == list(range(recovered_height + 1, last + 1)),
            "incomplete resumed block sequence")
    check_trace(resumed_rows, reference, responses)
    final = check_head(last)
    require(run("reopened", "--blocks", args.shard, "--through", str(last)) ==
            [{"through": last, "appHash": final["appHash"], "blocks": 0}], "cannot reopen final cursor")
    verified = live_rows + interrupted + resumed_rows
    require(any(row["validatorsHash"] != row["nextValidatorsHash"] for row in verified),
            "test did not cross a validator set change")

    pruned = output / "pruned"
    prune_split, prune_end = first + 10, last
    pruned_first = run("pruned-start", "--genesis", args.genesis, "--blocks", args.shard,
                       "--through", str(prune_split), "--keep-iavl-versions", "4", database=pruned)
    pruned_resume = run("pruned-resumed", "--blocks", args.shard, "--through", str(prune_end),
                        database=pruned)
    pruned_rows = [row for row in pruned_first + pruned_resume if "height" in row]
    require([row["height"] for row in pruned_rows] == list(range(first, prune_end + 1))
            and pruned_resume[0]["startingHeight"] == prune_split, "pruned replay did not resume")
    check_trace(pruned_rows, reference, responses)
    pruned_history = history(pruned)
    require(pruned_history["head"] == {"height": prune_end, "appHash": reference[prune_end]["app_hash"]}
            and pruned_history["earliestHeight"] == prune_end - 3
            and pruned_history["keepIavlVersions"] == 4, "pruned IAVL window differs after restart")
    result = {
        "platform": platform.platform(), "first": first, "liveThrough": live_tip,
        "rejectedAt": start_height, "recovered": recovered, "final": final,
        "checkedAbciBlocks": len(verified),
        "transactions": sum(row["transactions"] for row in verified),
        "validatorUpdates": sum(row["validatorUpdates"] for row in verified),
        "chain": ref["chain"], "pruned": pruned_history, "checkedPrunedAbciBlocks": len(pruned_rows),
    }
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
