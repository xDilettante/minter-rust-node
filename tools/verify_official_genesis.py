"""Verify the official minter-mainnet-5 genesis through a local ABCI 0.34 socket."""

import argparse
import base64
from collections import Counter
import gzip
from hashlib import sha256
import json
from pathlib import Path
import platform
import re
import shutil
import socket
import subprocess
from time import monotonic

from verify_mainnet_empty import Abci, app, fields, validator_updates, varint
from verify_send_devnet import HTTP, HIDE_WINDOW, allocate_ports, logs, require, start, stop, wait_for


GENESIS_SHA256 = "906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5"
CHAIN = "minter-mainnet-5"
INITIAL_HEIGHT = 10_197_000
APP_STATE_BYTES = 127_580_513
MAX_FRAME = 100 * 1024 * 1024
MAX_INIT_FRAME = 128 * 1024 * 1024


def init_frame(genesis):
    raw = genesis.read_bytes()
    require(sha256(raw).hexdigest() == GENESIS_SHA256, "official genesis SHA-256 differs")
    start = re.search(rb'"app_state"\s*:\s*', raw).end()
    state = raw[start:start + APP_STATE_BYTES]
    require(state.startswith(b"{") and state.endswith(b"}"), "invalid genesis app_state range")
    chain = CHAIN.encode()
    prefix = b"\x12" + varint(len(chain)) + chain + b"\x2a" + varint(len(state))
    suffix = b"\x30" + varint(INITIAL_HEIGHT)
    payload_len = len(prefix) + len(state) + len(suffix)
    request = b"\x2a" + varint(payload_len)
    size = len(request) + payload_len
    require(MAX_FRAME < size <= MAX_INIT_FRAME, "fixture does not exercise the larger ABCI frame")
    return varint(size << 1) + request + prefix, state, suffix


def send_init(abci, parts):
    for part in parts:
        abci.connection.sendall(part)


def abort_import(abci, process, parts):
    send_init(abci, parts)
    # Info runs on the same application thread. Wait until it queues behind
    # InitChain, then terminate the writer before it publishes the first root.
    deadline = monotonic() + 5
    while monotonic() < deadline:
        with socket.create_connection(abci.connection.getpeername(), timeout=1) as probe:
            probe.settimeout(0.25)
            probe.sendall(b"\x04\x1a\x00")
            try:
                require(bool(probe.recv(1)), "Info connection closed during InitChain")
            except TimeoutError:
                require(process.poll() is None, "minterd exited during InitChain")
                stop(process)
                return
    raise RuntimeError("could not interrupt InitChain while the application thread was busy")


def check_size_limits(address):
    for size, tag in ((MAX_FRAME + 1, b"\x32"), (MAX_INIT_FRAME + 1, b"\x2a")):
        with socket.create_connection(address, timeout=5) as probe:
            probe.sendall(varint(size << 1) + tag)
            try:
                require(probe.recv(1) == b"", "oversized frame was accepted")
            except ConnectionResetError:
                pass


def check_changed_genesis(address, parts):
    prefix, state, suffix = parts
    version = state.rfind(b'"v300"')
    require(version >= 0, "missing v300 marker")
    with socket.create_connection(address, timeout=10) as connection:
        connection.settimeout(30)
        with connection.makefile("rb") as reader:
            client = Abci(connection, reader)
            send_init(client, (prefix, memoryview(state)[:version], b'"v301"',
                               memoryview(state)[version + len(b'"v300"'):], suffix))
            try:
                client.receive(5)
            except RuntimeError as error:
                require(str(error) == "InitChain does not match stored genesis", str(error))
                return
    raise RuntimeError("changed genesis was accepted")


def history(minterctl, db):
    return subprocess.run([minterctl, "iavl", "history", "--db", db], capture_output=True,
                          text=True, timeout=120, creationflags=HIDE_WINDOW)


def samples(reference):
    exact = {b"c", b"d", b"p", b"v", b"si", b"sn", b"\xc5", b"\xc7", b"\xcc", b"\xd3"}
    exact.update(b"c" + i.to_bytes(4, "little") + b"t" for i in (181, 182, 183))
    usdt = b"q" + (1993).to_bytes(4, "little")
    exact.update((usdt, usdt + b"i"))
    prefixes = {b"q", b"f", b"w", b"t", b"sd", b"so", b"l"}
    selected = {}
    count = 0
    with gzip.open(reference / "go-init-iavl.jsonl.gz", "rt") as stream:
        for line in stream:
            count += 1
            if line.startswith('{"key":"61'):
                continue
            row = json.loads(line)
            key = bytes.fromhex(row["key"])
            if key in exact:
                exact.remove(key)
            else:
                prefix = next((p for p in prefixes if key.startswith(p)), None)
                if prefix is None:
                    continue
                prefixes.remove(prefix)
            selected[key] = bytes.fromhex(row.get("value", ""))
    require(not exact and not prefixes, "incomplete Go registry reference")
    return selected, count


def check_reads(abci, expected, balances):
    for key, value in expected.items():
        result = abci.iavl_key(key, INITIAL_HEIGHT - 1)
        require(result.get(1, 0) == 0 and result.get(6) == key
                and result.get(7, b"") == value and result.get(8)
                and result.get(9) == INITIAL_HEIGHT - 1, f"IAVL query differs for {key.hex()}")
    for coin, amount in balances.items():
        value = abci.balance("00" * 20, coin, INITIAL_HEIGHT - 1)
        require(value == {"balance": amount, "nonce": 0}, f"zero-account balance differs for coin {coin}")


def verify_synthetic_blocks(args, db, balances):
    records = json.loads((args.go_reference / "synthetic-blocks.json").read_text(encoding="utf-8"))
    require([r["height"] for r in records] == [INITIAL_HEIGHT, INITIAL_HEIGHT + 1],
            "unexpected synthetic block heights")
    scratch = args.output / "synthetic-state"
    shutil.copytree(db, scratch)
    commits = []
    last_commit = None
    for record in records:
        height = record["height"]
        with app(args.minterd, scratch, args.output / f"app-synthetic-{height}.log") as (abci, _):
            if last_commit is not None:
                require(abci.info() == last_commit, "first synthetic block did not survive reopening")
            _, updates = abci.stage(dict(record, deliver_tx=[]))
            commit = abci.commit(record)
            require(abci.info() == commit, f"Info differs after synthetic block {height}")
            for coin, amount in record["zero_address_balances"].items():
                value = abci.balance("00" * 20, int(coin), height)
                require(value == {"balance": amount, "nonce": 0}, f"balance differs at {height}")
            check_reads(abci, {}, balances)
            commits.append({**commit, "validatorUpdates": len(updates)})
            last_commit = commit
    return commits


def handshake(binary, home, address, log, expected):
    rpc, p2p = allocate_ports(2)
    command = [binary, "node", "--home", home, "--abci", "socket",
               "--proxy_app", f"tcp://{address[0]}:{address[1]}",
               "--rpc.laddr", f"tcp://127.0.0.1:{rpc}", "--p2p.laddr", f"tcp://127.0.0.1:{p2p}",
               "--p2p.pex=false", "--p2p.seeds", "", "--p2p.persistent_peers", ""]
    process = start(command, log)

    def get_round():
        with HTTP.open(f"http://127.0.0.1:{rpc}/dump_consensus_state", timeout=4) as response:
            return json.load(response).get("result")

    try:
        started = monotonic()
        state = wait_for(get_round, "official genesis Tendermint handshake", [(process, log)], timeout=240)
        elapsed = monotonic() - started
        require("Completed ABCI Handshake" in logs(log), "missing Tendermint handshake confirmation")
        round_state = state["round_state"]
        require(int(round_state["height"]) == INITIAL_HEIGHT and not state["peers"],
                "unexpected genesis consensus height or peer connection")
        actual = Counter(
            (base64.b64decode(v["pub_key"]["value"], validate=True).hex(), int(v["voting_power"]))
            for v in round_state["validators"]["validators"]
        )
        require(actual == expected, "Tendermint genesis validator set differs from Go")
        return elapsed
    finally:
        stop(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("genesis", "go-reference", "minterd", "minterctl", "tendermint", "output"):
        parser.add_argument(f"--{option}", required=True, type=Path)
    args = parser.parse_args()
    for key, value in vars(args).items():
        setattr(args, key, value.resolve())
    args.output.mkdir()
    parts = init_frame(args.genesis)
    reference = json.loads((args.go_reference / "result.json").read_text(encoding="utf-8"))
    raw_reply = (args.go_reference / "init-response.pb").read_bytes()
    expected = Counter((v["key"], v["power"]) for v in validator_updates(raw_reply, tag=2))
    require(reference["chain"] == CHAIN and reference["initialHeight"] == INITIAL_HEIGHT
            and reference["stateHeight"] == INITIAL_HEIGHT - 1
            and reference["initialValidators"] == expected.total(), "Go reference has unexpected genesis")
    registry, rows = samples(args.go_reference)
    require(rows == reference["operations"], "Go genesis trace row count differs")
    state = json.loads(parts[1])
    zero = next(row for row in state["accounts"] if row["address"] == "Mx" + "00" * 20)
    balances = {int(row["coin"]): row["value"] for row in zero["balance"] if int(row["coin"]) in (0, 1, 1993)}
    require(len(balances) == 3, "missing BIP or custom asset sample in genesis")
    del state

    db = args.output / "state"
    with app(args.minterd, db, args.output / "app-interrupted.log", timeout=240) as (abci, process):
        check_size_limits(abci.connection.getpeername())
        abort_import(abci, process, parts)
    failed = history(args.minterctl, db)
    require(failed.returncode != 0 and "no committed Minter state" in failed.stderr,
            f"interrupted import published a root or failed to reopen: {failed.stdout} {failed.stderr}")

    home = args.output / "tm"
    with (args.output / "tm-init.log").open("wb") as log:
        subprocess.run([args.tendermint, "init", "--home", home], check=True, timeout=30,
                       stdout=log, stderr=subprocess.STDOUT, creationflags=HIDE_WINDOW)
    shutil.copyfile(args.genesis, home / "config" / "genesis.json")
    config = home / "config" / "config.toml"
    text, edits = re.subn(r"(?m)^fast_sync = true$", "fast_sync = false", logs(config))
    require(edits == 1, "unexpected Tendermint config for local genesis test")
    config.write_text(text, encoding="utf-8")

    times = {}
    for phase in ("import", "restart"):
        with app(args.minterd, db, args.output / f"app-{phase}.log", timeout=240) as (abci, _):
            times[phase] = handshake(args.tendermint, home, abci.connection.getpeername(),
                                     args.output / f"tm-{phase}.log", expected)
            if phase == "import":
                send_init(abci, parts)
                require(abci.receive(5) == raw_reply, "InitChain protobuf differs from Go")
                check_changed_genesis(abci.connection.getpeername(), parts)
            info = fields(abci.call(3))
            require(info.get(3) == 8 and not info.get(4) and not info.get(5), "unexpected pre-block Info")
            check_reads(abci, registry, balances)
        committed = history(args.minterctl, db)
        require(committed.returncode == 0, committed.stderr)
        saved = json.loads(committed.stdout)
        require(saved["head"] == {"height": reference["stateHeight"], "appHash": reference["appHash"]}
                and saved["earliestHeight"] == reference["stateHeight"], "RocksDB genesis root differs from Go")
    report = {
        "platform": platform.platform(), "chain": CHAIN, "genesisSha256": GENESIS_SHA256,
        "appStateBytes": APP_STATE_BYTES, "initResponseSha256": sha256(raw_reply).hexdigest(),
        "initialValidators": expected.total(), "registryQueries": len(registry), "balanceQueries": len(balances),
        "history": saved, "handshakeSeconds": times, "interruptedImport": "recovered",
        "largeNonInitFrames": "rejected",
        "changedGenesis": "rejected",
        "rocksDbBytes": sum(path.stat().st_size for path in db.iterdir() if path.is_file()),
    }
    report["syntheticBlocks"] = verify_synthetic_blocks(args, db, balances)
    encoded = json.dumps(report, indent=2) + "\n"
    (args.output / "result.json").write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    main()
