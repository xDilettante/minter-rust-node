"""Replay authentic Minter blocks through the Rust ABCI 0.34 socket."""

import argparse
import base64
from contextlib import contextmanager
import csv
import gzip
import io
import json
import platform
import re
import socket
import subprocess
from time import perf_counter_ns
from pathlib import Path

from verify_mainnet_iavl import check_file, require
from verify_send_devnet import logs, start, stop, wait_for


def varint(value):
    encoded = bytearray()
    while value >= 128:
        encoded.append((value & 127) | 128)
        value >>= 7
    encoded.append(value)
    return bytes(encoded)


def read_varint(stream):
    value = 0
    for shift in range(0, 70, 7):
        byte = stream.read(1)
        require(bool(byte), "truncated ABCI varint")
        value |= (byte[0] & 127) << shift
        if byte[0] < 128:
            return value
    raise RuntimeError("ABCI varint overflow")


def wire_fields(data):
    stream = io.BytesIO(data)
    while stream.tell() < len(data):
        tag = read_varint(stream)
        kind, field = tag & 7, tag >> 3
        if kind == 0:
            yield field, read_varint(stream)
        elif kind == 2:
            length = read_varint(stream)
            value = stream.read(length)
            require(len(value) == length, "truncated ABCI message")
            yield field, value
        else:
            raise RuntimeError(f"unexpected ABCI wire type {kind}")


def fields(data):
    return dict(wire_fields(data))


def coin_creation(response, height):
    tags = {}
    for field, event in wire_fields(response):
        if field != 7:
            continue
        for event_field, attribute in wire_fields(event):
            if event_field == 2:
                item = fields(attribute)
                tags[item[1].decode()] = item.get(2, b"").decode()
    if tags.get("tx.type") not in ("05", "1e") or "tx.burned_for_symbol" not in tags:
        return None
    return {"height": height, "coinId": int(tags["tx.coin_id"]), "symbol": tags["tx.coin_symbol"],
            "priceCoin": int(tags["tx.commission_price_coin"]),
            "chargedBipPip": tags["tx.commission_in_base_coin"],
            "tickerBipPip": tags["tx.burned_for_symbol"]}


def validator_updates(response, *, tag=1):
    updates = []
    for field, encoded in wire_fields(response):
        if field == tag:
            validator = fields(encoded)
            key = fields(validator[1])[1]
            require(len(key) == 32, "invalid Ed25519 validator key")
            updates.append({"key": key.hex(), "power": validator.get(2, 0)})
    return updates


class Abci:
    def __init__(self, connection, reader, *, flush=False):
        self.connection, self.reader = connection, reader
        self.flush = flush

    def call(self, request_tag, payload=b""):
        # v0.34 responses add an exception alternative at field 1.
        request = varint((request_tag << 3) | 2) + varint(len(payload)) + payload
        frame = varint(len(request) << 1) + request
        if self.flush:
            # Go buffers responses until it receives RequestFlush (field 2).
            frame += b"\x04\x12\x00"
        self.connection.sendall(frame)
        response = self.receive(request_tag)
        if self.flush:
            require(self.receive(2) == b"", "unexpected ABCI Flush payload")
        return response

    def receive(self, request_tag):
        length = read_varint(self.reader)
        require(length & 1 == 0 and length >> 1 <= 1024 * 1024, "invalid ABCI frame")
        data = self.reader.read(length >> 1)
        require(len(data) == length >> 1, "truncated ABCI frame")
        response = fields(data)
        if 1 in response:
            raise RuntimeError(fields(response[1])[1].decode())
        require(request_tag + 1 in response, "ABCI response method differs")
        return response[request_tag + 1]

    def info(self):
        response = fields(self.call(3))
        require(response.get(3) == 8, "unexpected ABCI app version")
        return {"height": response[4], "appHash": response[5].hex()}

    def balance(self, address, coin, height):
        data = bytes.fromhex(address) + coin.to_bytes(4, "little")
        payload = b"\x0a" + varint(len(data)) + data + b"\x12\x08/balance\x18" + varint(height)
        response = fields(self.call(6, payload))
        require(response.get(1, 0) == 0 and response[6] == data and response[9] == height,
                f"invalid balance response for {address}:{coin} at height {height}")
        return json.loads(response[7])

    def iavl_key(self, key, height):
        payload = (b"\x0a" + varint(len(key)) + key + b"\x12\x09/iavl/key\x18"
                   + varint(height) + b"\x20\x01")
        return fields(self.call(6, payload))

    def stage(self, record):
        def verify(tag, name, pair):
            response = self.call(tag, base64.b64decode(pair["request"], validate=True))
            require(response == base64.b64decode(pair["response"] or "", validate=True),
                    f"{name} differs from Go at height {record['height']}")
            return response

        verify(7, "begin_block", record["begin_block"])
        deliveries = [
            verify(9, f"deliver_tx[{i}]", pair) for i, pair in enumerate(record["deliver_tx"] or ())
        ]
        end = verify(10, "end_block", record["end_block"])
        return deliveries, validator_updates(end)

    def commit(self, record):
        response = self.call(11)
        height, expected = record["height"], record["app_hash"]
        actual = fields(response)[2].hex()
        require(response == base64.b64decode(record["commit_response"], validate=True)
                and actual == expected, f"Commit differs at {height}, expected {expected}, got {actual}")
        return {"height": height, "appHash": actual}


@contextmanager
def app(binary, db, log, *args, timeout=60):
    process = start([binary, "--db", db, "--abci-listen", "127.0.0.1:0", *args], log)
    try:
        ready = wait_for(lambda: re.search(r"ABCI 0\.34 listening on 127\.0\.0\.1:(\d+)", logs(log)),
                         "Rust ABCI socket", [(process, log)])
        with socket.create_connection(("127.0.0.1", int(ready[1])), timeout=10) as connection:
            connection.settimeout(timeout)
            with connection.makefile("rb") as reader:
                yield Abci(connection, reader), process
    finally:
        stop(process)


def load_inputs(trace, headers, checkpoint, through, skip_prefix=False):
    base = checkpoint["height"]
    index = {}
    with gzip.open(headers, "rt", encoding="utf-8", newline="") as stream:
        for row in csv.DictReader(stream):
            height = int(row["height"])
            if height > through + 1:
                break
            require(height not in index, f"duplicate verified header {height}")
            index[height] = row
    require(index[base]["block_hash"] == checkpoint["blockHash"]
            and index[base + 1]["header_app_hash"] == checkpoint["appHash"],
            "checkpoint differs from verified headers")
    blocks = []
    with gzip.open(trace, "rt", encoding="utf-8") as stream:
        for line in stream:
            record = json.loads(line)
            height = record["height"]
            if skip_prefix and height <= base:
                continue
            require(height == base + len(blocks) + 1, "nonconsecutive Go trace")
            require(record["block_hash"] == index[height]["block_hash"]
                    and record["app_hash"] == index[height + 1]["header_app_hash"],
                    f"Go trace differs from verified headers at {height}")
            blocks.append(record)
            if height == through:
                break
    require(len(blocks) == through - base, "incomplete Go inputs")
    return blocks, index


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "corpus", "snapshot", "manifest", "trace", "headers", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--through", type=int, required=True)
    parser.add_argument("--interrupt-before", type=int,
                        help="block height at which to stop after EndBlock and before Commit")
    parser.add_argument("--interrupt-after-commit", type=int,
                        help="stop and reopen after receiving Commit at this height")
    parser.add_argument("--keep-iavl-versions", type=int,
                        help="enable a persisted IAVL retention window on the first launch")
    parser.add_argument("--go-balances", nargs="+", type=Path, default=[],
                        help="historical balances read by the Go reference")
    parser.add_argument("--go-state", nargs="+", type=Path, default=[],
                        help="historical IAVL values read by the Go reference")
    parser.add_argument("--existing-db", type=Path,
                        help="append to a RocksDB database whose head matches the verified checkpoint")
    args = parser.parse_args()
    corpus = json.loads(logs(args.corpus))
    checkpoint = json.loads(logs(args.manifest))
    for path, name in ((args.snapshot, "go_checkpoint/snapshot.zlib"),
                       (args.manifest, "go_checkpoint/manifest.json"),
                       (args.trace, "abci-iavl.jsonl.gz"), (args.headers, "blocks.csv.gz")):
        check_file(path, corpus["files"][name])
    records, _ = load_inputs(args.trace, args.headers, checkpoint, args.through)
    require(bool(records), "need a block to replay")
    args.output.mkdir(parents=True)
    daemon, cli = (binary.resolve(strict=True) for binary in (args.daemon, args.cli))
    if args.existing_db is None:
        db = args.output / "rocksdb"
        imported = subprocess.run(
            [cli, "checkpoint", "import", "--db", db, "--snapshot", args.snapshot, "--manifest", args.manifest],
            capture_output=True, text=True, timeout=300,
        )
        (args.output / "import.log").write_text(imported.stdout + imported.stderr, encoding="utf-8")
        require(imported.returncode == 0, f"checkpoint import failed: {imported.stderr}")
    else:
        db = args.existing_db.resolve(strict=True)
        require((db / "CURRENT").is_file(), "existing RocksDB database is missing CURRENT")

    initial = {"height": checkpoint["height"], "appHash": checkpoint["appHash"]}
    cut = (len(records) // 2 if args.interrupt_before is None
           else args.interrupt_before - records[0]["height"])
    require(0 <= cut < len(records), "interruption is outside the replay range")
    after = args.interrupt_after_commit
    stop_at = len(records) if after is None else after - initial["height"]
    require(cut < stop_at <= len(records), "post-Commit interruption is outside the resumed replay")
    keep = args.keep_iavl_versions
    require(keep is None or keep > 0, "the pruning test requires a positive IAVL window")
    policy = [] if keep is None else ["--keep-iavl-versions", str(keep)]
    roots = []
    transactions = []
    creations = []
    validators = []
    commit_ns = []

    def check_retention(client, head):
        if keep is None:
            return None
        first = max(initial["height"], head - keep + 1)
        for height in {first, head}:
            response = client.iavl_key(b"p", height)
            require(response.get(1, 0) == 0 and response.get(9) == height and response.get(8),
                    f"missing retained IAVL proof at {height}")
        if first > initial["height"]:
            old = client.iavl_key(b"p", first - 1)
            require(old.get(1) == 1 and f"{first}..={head}".encode() in old.get(3, b""),
                    f"IAVL query did not reject discarded height {first - 1}")
            expired_balance = b"\x0a\x14" + bytes(20) + b"\x12\x08/balance\x18" + varint(first - 1)
            require(fields(client.call(6, expired_balance)).get(1) == 1,
                    f"balance query did not reject discarded height {first - 1}")
        return {"first": first, "head": head, "versions": head - first + 1}

    def replay(client, records):
        for record in records:
            deliveries, updates = client.stage(record)
            started = perf_counter_ns()
            roots.append(client.commit(record))
            commit_ns.append(perf_counter_ns() - started)
            if updates:
                validators.append({"height": record["height"], "updates": updates})
            if deliveries:
                transactions.append({"height": record["height"],
                                     "codes": [fields(delivery).get(1, 0) for delivery in deliveries]})
                for delivery in deliveries:
                    creation = coin_creation(delivery, record["height"])
                    if creation:
                        creations.append(creation)

    replay_started = perf_counter_ns()
    with app(daemon, db, args.output / "initial.log", *policy) as (client, process):
        require(client.info() == initial, "ABCI did not open at the verified Go checkpoint")
        replay(client, records[:cut])
        prior = roots[-1] if roots else initial
        client.stage(records[cut])
        require(client.info() == prior, "EndBlock changed committed state")
        stop(process)
        require(process.returncode != 0, "Rust process did not stop at the cut")
    reopen_started = perf_counter_ns()
    with app(daemon, db, args.output / "resumed.log") as (client, process):
        reopened = client.info()
        reopen_before_ms = (perf_counter_ns() - reopen_started) / 1_000_000
        require(reopened == prior, "Rust reported uncommitted state after interruption")
        prior_window = check_retention(client, prior["height"])
        replay(client, records[cut:stop_at])
        require(client.info() == roots[-1], "final ABCI head differs")
        if after is not None:
            stop(process)
            require(process.returncode != 0, "Rust process did not stop after Commit")
    recovered_after = None
    reopen_after_ms = None
    if after is not None:
        reopen_started = perf_counter_ns()
        with app(daemon, db, args.output / "resumed-after-commit.log") as (client, _):
            recovered_after = client.info()
            reopen_after_ms = (perf_counter_ns() - reopen_started) / 1_000_000
            require(recovered_after == roots[-1], "Rust lost a committed block after interruption")
            check_retention(client, recovered_after["height"])
            replay(client, records[stop_at:])
    replay_seconds = (perf_counter_ns() - replay_started) / 1_000_000_000
    reads = []
    by_height = {row["height"]: row["appHash"] for row in [initial, *roots]}
    with app(daemon, db, args.output / "final.log") as (client, _):
        require(client.info() == roots[-1], "final RocksDB head did not survive reopen")
        final_window = check_retention(client, roots[-1]["height"])
        for path in args.go_balances:
            expected = json.loads(logs(path))
            height = expected["height"]
            require(by_height.get(height) == expected["appHash"],
                    f"Go balance root differs at height {height}")
            for query in expected["balances"]:
                actual = client.balance(query["address"], query["coin"], height)
                require(actual == {"balance": query["balance"], "nonce": query["nonce"]},
                        f"balance differs from Go at height {height}, {query['address']}:{query['coin']}")
                reads.append({"height": height, **query})
    state = []
    for path in args.go_state:
        expected = json.loads(logs(path))
        height = expected["height"]
        require(by_height.get(height) == expected["appHash"], f"Go state root differs at height {height}")
        for row in expected["values"]:
            completed = subprocess.run(
                [cli, "iavl", "get", "--db", db, "--height", str(height), "--key", row["key"]],
                capture_output=True, text=True, timeout=30,
            )
            require(completed.returncode == 0, f"IAVL read failed: {completed.stderr}")
            actual = json.loads(completed.stdout)
            require(actual == {"height": height, **row},
                    f"IAVL value differs from Go at height {height}, key {row['key']}")
            state.append(actual)
    commit_ns.sort()
    p99 = commit_ns[(len(commit_ns) * 99 + 99) // 100 - 1] / 1_000_000
    result = {"os": platform.platform(), "goOracleRevision": corpus["go_oracle_revision"],
              "chainId": corpus["chain_id"], "checkpoint": initial, "roots": roots,
              "transactions": transactions, "balances": reads, "coinCreations": creations, "stateReads": state,
              "validatorUpdates": validators,
              "cutBeforeCommitAt": records[cut]["height"], "recovered": reopened, "final": roots[-1],
              "cutAfterCommitAt": after, "recoveredAfterCommit": recovered_after,
              "priorWindow": prior_window, "finalWindow": final_window, "keepIavlVersions": keep,
              "replaySeconds": replay_seconds, "commitRoundTripP99Ms": p99,
              "reopenBeforeCommitMs": reopen_before_ms, "reopenAfterCommitMs": reopen_after_ms}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"os": result["os"], "blocks": len(roots), "transactions": transactions,
                      "coinCreations": creations,
                      "validatorUpdates": {row["height"]: len(row["updates"]) for row in validators},
                      "final": roots[-1]}))


if __name__ == "__main__":
    main()
