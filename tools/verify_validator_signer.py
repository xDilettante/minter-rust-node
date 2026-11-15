"""Compare the Rust private validator with Go and restart it in a Minter devnet."""

import argparse
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess

from verify_send_devnet import HIDE_WINDOW, Node, allocate_ports, logs, receipt, require, signed_root, start, stop, wait_for


def run(command, check=True):
    return subprocess.run(command, check=check, capture_output=True, text=True,
                          encoding="utf-8", errors="replace", timeout=15,
                          creationflags=HIDE_WINDOW)


class Signer:
    def __init__(self, daemon, root, fixture, chain_id):
        self.root = root
        root.mkdir()
        self.key = root / "priv_validator_key.json"
        shutil.copyfile(fixture / self.key.name, self.key)
        self.journal = root / "journal"
        self.identity = root / "node_key.json"
        command = [daemon, "validator-signer"]
        self.common = ["--key", self.key, "--chain-id", chain_id, "--journal", self.journal]
        self.launch = [*command, "run", *self.common, "--transport-key", self.identity]
        self.id = run([*command, "transport-id", "--transport-key", self.identity]).stdout.strip()
        require(re.fullmatch("[0-9a-f]{40}", self.id), "invalid transport ID")
        self.import_command = [*command, "import", *self.common, "--go-state",
                               fixture / "priv_validator_state.json"]
        imported = run(self.import_command, check=False)
        require(imported.returncode == 0, imported.stderr)
        self.process = None

    def start(self, phase, endpoint):
        log = self.root / f"{phase}.log"
        self.process = start([*self.launch, "--peer", endpoint], log)
        return self.process, log

    def close(self):
        stop(self.process)
        self.process = None


def read_probe(log):
    for line in logs(log).splitlines():
        if line.startswith("Go private validator probe listening "):
            return line.rsplit(" ", 1)[1]
    return None


def go_probe(args, chain_id):
    signer = Signer(args.daemon, args.output / "go-probe", args.fixture, chain_id)
    evidence = {}
    for mode in ("initial", "recover"):
        address = f"127.0.0.1:{allocate_ports(1)[0]}"
        output = signer.root / mode
        output.mkdir()
        log = output / "oracle.log"
        oracle = start([args.probe, mode, address, args.fixture, output], log)
        try:
            peer = wait_for(lambda: read_probe(log), "Go private validator listener", [(oracle, log)])
            rust, rust_log = signer.start(mode, f"{peer}@{address}")
            wait_for(lambda: "signer connected" in logs(rust_log), "Rust signer connected",
                     [(rust, rust_log)])
            if mode == "initial":
                locked = run([*signer.launch, "--peer", f"{peer}@{address}"], check=False)
                require(locked.returncode != 0 and ("exclusive lock" in locked.stderr
                                                    or "already in use" in locked.stderr), locked.stderr)
            require(oracle.wait(timeout=25) == 0, logs(log))
            require(rust.poll() is None and "signer connected" in logs(rust_log), logs(rust_log))
            result = json.loads(logs(log).splitlines()[-1])
            require(result["mode"] == mode and result["exactGoSignatures"], result)
            evidence[mode] = result
        finally:
            signer.close()
            stop(oracle)
        if mode == "initial":
            # Exercise missing live WAL after a forced process stop. The last
            # acknowledged signature must still be readable from the flushed SST.
            wals = list(signer.journal.glob("*.log"))
            require(wals and all(path.stat().st_size == 0 for path in wals),
                    "a nonempty live signing WAL remained after flush")
            for path in wals:
                path.unlink()
            evidence["removedWalFiles"] = len(wals)
    require(run(signer.import_command, check=False).returncode != 0,
            "existing signing journal can be overwritten")
    key_copy = signer.root / "validator-key-copy.json"
    shutil.copyfile(args.fixture / signer.key.name, key_copy)
    reused = run([*signer.launch[:-2], "--transport-key", key_copy,
                  "--peer", f"{peer}@{address}"], check=False)
    require(reused.returncode != 0 and "separate Ed25519 key" in reused.stderr, reused.stderr)
    return evidence


def mixed_devnet(args, fixture):
    ports = allocate_ports(7)
    rust = Node(args.output / "validator", args.tendermint, args.daemon, args.fixture, ports[:3], True)
    go = Node(args.output / "observer", args.tendermint, args.go_app, args.fixture, ports[3:6], False)
    signer = None
    first_height = fixture["genesisHeight"] + 1
    raw = fixture["transactions"]
    try:
        rust.launch(go, "local-go")
        go.launch(rust, "initial")
        wait_for(lambda: min(rust.height(), go.height()) >= first_height + 3,
                 "Go local signer and Go observer agree", rust.processes + go.processes)
        genesis = signed_root(rust, go, first_height + 1)
        require(genesis["appHash"] == fixture["blocks"][0]["appHash"], genesis)
        rust.close()
        go_state = rust.home / "data/priv_validator_state.json"
        before = json.loads(go_state.read_bytes())
        require(int(before["height"]) >= first_height + 3 and before["signature"],
                "Go journal lacks a signed height")
        signing_source = args.output / "migration"
        signing_source.mkdir()
        shutil.move(rust.home / "config/priv_validator_key.json",
                    signing_source / "priv_validator_key.json")
        shutil.move(go_state, signing_source / "priv_validator_state.json")
        signer = Signer(args.daemon, args.output / "signer", signing_source, fixture["chainId"])
        local = signer.root / "priv_validator_key.json"
        validator = json.loads(local.read_bytes())["address"]
        address = f"127.0.0.1:{ports[-1]}"
        config = rust.home / "config/config.toml"
        original = logs(config)
        changed, count = re.subn(r'(?m)^priv_validator_laddr[ \t]*=.*$',
                                 f'priv_validator_laddr = "tcp://{address}"', original)
        require(count == 1, "private validator listener missing in the config")
        configured = f'priv_validator_signer_id = "{signer.id}"\n' + changed
        endpoint = f"{rust.node_id}@{address}"
        wrong_id = "00" * 20
        untrusted = signer.root / "untrusted.json"
        run([args.daemon, "validator-signer", "transport-id", "--transport-key", untrusted])

        config.write_text(f'priv_validator_signer_id = "{signer.id}"\n' + original, encoding="utf-8")
        rust.launch(go, "missing-listener")
        require(rust.engine_process.wait(timeout=10) != 0
                and "priv_validator_signer_id requires priv_validator_laddr"
                in logs(rust.root / "tm-missing-listener.log"), "Go did not reject the missing signer listener")
        rust.close()
        config.write_text(configured, encoding="utf-8")

        wrong_log = signer.root / "wrong-listener.log"
        wrong_job = start([*signer.launch, "--peer", f"{wrong_id}@{address}"], wrong_log)
        try:
            rust.launch(go, "wrong-listener")
            wait_for(lambda: "public key does not match" in logs(wrong_log),
                     "Rust rejected the unpinned Go listener", [(wrong_job, wrong_log)])
            require("signer connected" not in logs(wrong_log), "signer accepted an unpinned listener")
        finally:
            stop(wrong_job)
            rust.close()
        impostor_log = signer.root / "wrong-signer.log"
        impostor_job = start([*signer.launch[:-2], "--transport-key", untrusted, "--peer", endpoint],
                             impostor_log)
        try:
            rust.launch(go, "wrong-signer", engine_args=("--log_level", "debug"))
            wait_for(lambda: "unexpected private validator signer peer ID"
                     in logs(rust.root / "tm-wrong-signer.log"),
                     "Go rejected the unpinned Rust signer", [(impostor_job, impostor_log)])
        finally:
            stop(impostor_job)
            rust.close()
        signed = signer.start("remote", endpoint)
        rust.launch(go, "remote")
        imported_height = int(before["height"])
        children = rust.processes + go.processes + [signed]
        wait_for(lambda: min(rust.height(), go.height()) > imported_height + 1,
                 "Rust signer commits with the imported Go state", children)
        require(rust.rpc("status")["validator_info"]["address"].upper() == validator.upper(),
                "Go switched to a different validator key")
        broadcast = go.rpc("broadcast_tx_sync", tx="0x" + raw["transfer"])
        require(broadcast.get("code", 0) == 0, broadcast)
        first = receipt(rust, go, raw["transfer"])
        before_restart = rust.height()
        signer.close()
        restarted = signer.start("restarted", endpoint)
        wait_for(lambda: min(rust.height(), go.height()) >= before_restart + 2,
                 "Go engines resumed after forced signer restart",
                 rust.processes + go.processes + [restarted])
        broadcast = go.rpc("broadcast_tx_sync", tx="0x" + raw["self"])
        require(broadcast.get("code", 0) == 0, broadcast)
        second = receipt(rust, go, raw["self"])
        roots = [signed_root(rust, go, height)
                 for height in range(imported_height + 1, second["height"] + 2)]
        require(not (rust.home / "config/priv_validator_key.json").exists(),
                "Go created a local validator key in pinned signer mode")
        return {"importedHeight": imported_height, "restartAtHeight": before_restart,
                "genesis": genesis, "first": first, "afterRestart": second,
                "signedRoots": roots, "rustValidatorAddress": validator,
                "rejectedUnpinnedListener": True, "rejectedUnpinnedSigner": True}
    finally:
        if signer:
            signer.close()
        rust.close()
        go.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "tendermint", "go_app", "probe", "fixture", "output"):
        parser.add_argument(name, type=lambda path: Path(path).resolve())
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    result = {"platform": platform.platform(), "goOracle": go_probe(args, fixture["chainId"]),
              "devnet": mixed_devnet(args, fixture)}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"platform": result["platform"], "importedHeight": result["devnet"]["importedHeight"],
                      "signedRoots": len(result["devnet"]["signedRoots"]),
                      "firstTransaction": result["devnet"]["first"]["height"],
                      "afterRestart": result["devnet"]["afterRestart"]["height"],
                      "goOracle": result["goOracle"]}))
