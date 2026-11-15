"""Exercise the shared foreground supervisor with real Go peers and controlled RPC delays."""

import argparse
import json
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from verify_send_devnet import HTTP, HIDE_WINDOW, Node, allocate_ports, require, set_toml, stop, wait_for


class DelayedRpc:
    def __init__(self, target, fail=False):
        self.requested = threading.Event()
        self.release = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                owner.requested.set()
                if not owner.release.wait(30):
                    self.send_error(504)
                    return
                if fail:
                    self.send_error(503)
                    return
                with HTTP.open(target + self.path, timeout=15) as reply:
                    body = reply.read()
                    self.send_response(reply.status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def verify(args):
    args.output.mkdir(parents=True)
    ports = allocate_ports(8)
    nodes = [Node(args.output / name, args.tendermint, args.go_app, args.fixture,
                  ports[i:i + 3], i == 0) for name, i in (("primary", 0), ("witness", 3))]
    primary, witness = nodes
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    genesis["consensus_params"]["version"] = {"app_version": "8"}
    genesis_path = args.output / "Начальное состояние.json"
    genesis_path.write_text(json.dumps(genesis), encoding="utf-8")
    for node in nodes:
        (node.home / "config/genesis.json").write_bytes(genesis_path.read_bytes())
        config_path = node.home / "config/config.toml"
        config_path.write_text(set_toml(config_path.read_text(encoding="utf-8"), "consensus",
                                       {"timeout_commit": '"1100ms"'}), encoding="utf-8")
    running = stopper = None
    proxies = []

    def cli(*command, check=True):
        return subprocess.run([args.cli, *command], capture_output=True, text=True,
                              encoding="utf-8", check=check, timeout=50, creationflags=HIDE_WINDOW)

    def launch(config, label):
        path = args.output / f"{label}.log"
        with path.open("wb") as output:
            process = subprocess.Popen([args.cli, "node", "run", "--config", config,
                                        "--shutdown-on-stdin-close"], stdin=subprocess.PIPE,
                                       stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=os.name == "posix", creationflags=HIDE_WINDOW)
        return process, path

    config = args.output / "Настройки.json"
    try:
        primary.launch(witness, "initial", use_rust=False)
        witness.launch(primary, "initial", use_rust=False)
        processes = primary.processes + witness.processes
        first = int(genesis["initial_height"])
        wait_for(lambda: min(node.height() for node in nodes) >= first + 5, "two Go peers", processes)
        settings = json.loads(cli("node", "init", "--config", config, "--data-dir",
                                  args.output / "Данные полной ноды", "--mode", "full").stdout)
        original = config.read_bytes()
        require(cli("node", "init", "--config", config, "--data-dir", args.output / "other",
                    check=False).returncode != 0 and config.read_bytes() == original,
                "node init replaced existing settings")
        settings.update({"fullStart": "genesis", "genesis": str(genesis_path),
                         "p2pPort": ports[6], "apiPort": ports[7],
                         "peers": [f"{primary.node_id}@127.0.0.1:{primary.p2p}"]})
        profiles = []
        for name, limits in [("full", (120, 100000, 100000)), ("archive", (0, 0, 0)),
                             ("custom", (4, 0, 0))]:
            settings.update({"storageProfile": name, "keepVersions": 4,
                             "keepEventBlocks": 0, "keepNativeBlocks": 0})
            config.write_text(json.dumps(settings, ensure_ascii=False), encoding="utf-8")
            running, path = launch(config, name)

            def ready():
                with HTTP.open(f"http://127.0.0.1:{ports[7]}/v2/status", timeout=3) as reply:
                    status = json.load(reply)
                return status if int(status["latest_block_height"]) >= first + 3 else None

            current = wait_for(ready, name, processes + [(running, path)], timeout=90)
            header = primary.rpc("commit", height=int(current["latest_block_height"]))["signed_header"]["header"]
            require(current["latest_app_hash"].lower() == header["app_hash"].lower(), current)
            conflict = cli("node", "run", "--config", config, check=False)
            require(conflict.returncode != 0 and "Настройки уже используются другим процессом" in conflict.stderr,
                    "second supervisor did not fail on the configuration lease: " + conflict.stderr)
            other = dict(settings)
            other["apiPort"], other["p2pPort"] = allocate_ports(2)
            if os.name == "posix":
                alias = args.output / f"data-alias-{name}"
                alias.symlink_to(Path(settings["dataDir"]), target_is_directory=True)
                other["dataDir"] = str(alias)
            alternate = args.output / "other-settings.json"
            alternate.write_text(json.dumps(other), encoding="utf-8")
            data_conflict = cli("node", "run", "--config", alternate, check=False)
            require(data_conflict.returncode != 0 and "Папка данных уже используется другим процессом" in data_conflict.stderr,
                    "second supervisor did not fail on the canonical data lease: " + data_conflict.stderr)
            observed = json.loads(cli("node", "status", "--config", alternate).stdout)
            require(not observed["managed"] and observed["controllable"] and observed["controlId"], observed)
            handoff = None
            if name == "full":
                wrong = {**settings, "mode": "light"}
                wrong_path = args.output / "wrong-mode.json"
                wrong_path.write_text(json.dumps(wrong), encoding="utf-8")
                refused = cli("node", "stop", "--config", wrong_path, check=False)
                require(refused.returncode != 0 and "другим режимом" in refused.stderr, refused.stderr)
                require(running.poll() is None, "wrong-mode request stopped the owner")
                cli("node", "stop", "--config", alternate)
                old = running
                old_path = path
                old_alive = old.poll() is None
                running, path = launch(config, "handoff-replacement")
                require(old.wait(timeout=45) == 0, old_path.read_text(encoding="utf-8", errors="replace"))
                old.stdin.close()
                wait_for(ready, "replacement after remote stop", processes + [(running, path)], timeout=90)
                newer = json.loads(cli("node", "status", "--config", config).stdout)
                require(newer["controlId"] != observed["controlId"] and newer["controllable"], newer)
                require(running.poll() is None, "old owner cleanup stopped its replacement")
                cli("node", "stop", "--config", config)
                handoff = {"oldOwnerAliveAtReplacementLaunch": old_alive,
                           "oldControlId": observed["controlId"], "newControlId": newer["controlId"],
                           "replacementSurvivedOldCleanup": True, "wrongModeExit": refused.returncode}
                method = "local IPC and ownership handoff"
            elif os.name == "posix" and name == "custom":
                running.send_signal(signal.SIGTERM)
                method = "SIGTERM"
            else:
                running.stdin.close()
                method = "stdin EOF"
            require(running.wait(timeout=45) == 0, path.read_text(encoding="utf-8", errors="replace"))
            running = None
            stored = subprocess.run([args.daemon, "native-history", "status", "--db",
                                     Path(settings["dataDir"]) / "state"], check=True, capture_output=True,
                                    text=True, encoding="utf-8", timeout=30, creationflags=HIDE_WINDOW)
            disk = json.loads(stored.stdout)
            require(tuple(disk["retention"][key] for key in
                    ("keepIavlVersions", "keepEventBlocks", "keepNativeBlocks")) == limits, disk)
            profiles.append({"requested": name, "stored": disk, "stopMethod": method,
                             "remoteStatus": observed, "handoff": handoff})
            settings["fullStart"] = "resume"

        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", ports[6]))
            occupied.listen()
            failed = cli("node", "run", "--config", config, check=False)
            require(failed.returncode != 0, "daemon bind failure was swallowed")
            (args.output / "bind-failure.log").write_text(failed.stderr, encoding="utf-8")

        anchor_height = min(node.height() for node in nodes) - 2
        signed = primary.rpc("commit", height=anchor_height)["signed_header"]
        anchor = args.output / "Доверенная точка.json"
        anchor.write_text(json.dumps({"chainId": genesis["chain_id"], "initialHeight": str(first),
                                     "height": str(anchor_height), "hash": signed["commit"]["block_id"]["hash"],
                                     "trustingPeriodSecs": 3600}), encoding="utf-8")
        light = []
        for failing in (False, True):
            pair = [DelayedRpc(f"http://127.0.0.1:{node.rpc_port}", fail=failing) for node in nodes]
            proxies.extend(pair)
            label = "light-failed-stop" if failing else "light-finished-stop"
            settings.update({"mode": "light", "dataDir": str(args.output / label),
                             "lightAnchor": str(anchor), "primary": pair[0].url, "witness": pair[1].url})
            config.write_text(json.dumps(settings, ensure_ascii=False), encoding="utf-8")
            running, path = launch(config, label)
            require(pair[0].requested.wait(20), "light process did not request RPC")
            observed = json.loads(cli("node", "status", "--config", config).stdout)
            require(not observed["managed"] and observed["controllable"] and observed["controlId"], observed)
            if os.name == "posix" and not failing:
                children = Path(f"/proc/{running.pid}/task/{running.pid}/children").read_text().split()
                require(len(children) == 1 and os.getpgid(int(children[0])) != os.getpgid(running.pid),
                        "light child shares terminal process group")
            stopper_log = args.output / f"{label}-remote-stop.log"
            with stopper_log.open("wb") as output:
                stopper = subprocess.Popen([args.cli, "node", "stop", "--config", config],
                                           stdout=output, stderr=subprocess.STDOUT,
                                           stdin=subprocess.DEVNULL, creationflags=HIDE_WINDOW)
            time.sleep(0.25)
            require(stopper.poll() is None and running.poll() is None,
                    "remote stop returned before the one-shot light request finished")
            method = "local IPC during gated RPC"
            for proxy in pair:
                proxy.release.set()
            code = running.wait(timeout=45)
            require((code != 0) == failing, path.read_text(encoding="utf-8", errors="replace"))
            stop_code = stopper.wait(timeout=50)
            require((stop_code != 0) == failing, stopper_log.read_text(encoding="utf-8", errors="replace"))
            stopper = None
            running = None
            if not failing:
                saved = json.loads(cli("light", "status", "--db", Path(settings["dataDir"]) / "light.redb").stdout)
                require(int(saved["height"]) >= anchor_height and not saved["stale"], saved)
            else:
                saved = None
            light.append({"failedRpc": failing, "exitCode": code, "remoteStopExit": stop_code,
                          "stopMethod": method, "savedHeader": saved, "remoteStatus": observed})
        result = {"platform": platform.platform(), "profiles": profiles, "light": light,
                  "bindFailureExit": failed.returncode, "configurationLease": True,
                  "canonicalDataLease": True, "initPreservedExistingFile": True}
        (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        return result
    finally:
        for proxy in proxies:
            proxy.release.set()
        stop(running)
        stop(stopper)
        for proxy in proxies:
            proxy.close()
        for node in nodes:
            node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("cli", "daemon", "tendermint", "go_app", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
