"""Exercise the real Tauri UI, minterd and minterctl against two local Go nodes."""

import argparse
import base64
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools"))
from verify_send_devnet import HIDE_WINDOW, HTTP, Node, allocate_ports, logs, require, set_toml, stop, wait_for


class Desktop:
    def __init__(self, args, port, native_port):
        self.args = args
        self.base = f"http://127.0.0.1:{port}"
        self.session = None
        self.log = args.output / "webdriver.log"
        environment = os.environ.copy()
        environment["MINTER_DESKTOP_HOME"] = str(args.output / "Профиль GUI")
        with self.log.open("wb") as output:
            self.process = subprocess.Popen([
                args.driver, "--port", str(port), "--native-port", str(native_port),
                "--native-driver", args.native_driver,
            ], env=environment, stdout=output, stderr=subprocess.STDOUT, creationflags=HIDE_WINDOW)
        wait_for(lambda: self.request("GET", "/status"), "Tauri WebDriver", [(self.process, self.log)])
        value = self.request("POST", "/session", {
            "capabilities": {"alwaysMatch": {"tauri:options": {"application": str(args.application)}}},
        })
        self.session = value["sessionId"]
        self.call("POST", "/timeouts", {"script": 55000, "pageLoad": 30000, "implicit": 0})

    def request(self, method, path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method,
                                         headers={"Content-Type": "application/json"})
        try:
            with HTTP.open(request, timeout=65) as reply:
                value = json.load(reply)["value"]
        except urllib.error.HTTPError as error:
            raise RuntimeError(error.read().decode(errors="replace")) from error
        if isinstance(value, dict) and "error" in value:
            raise RuntimeError(value)
        return value

    def call(self, method, path, body=None):
        return self.request(method, f"/session/{self.session}" + path, body)

    def script(self, script, *args):
        return self.call("POST", "/execute/sync", {"script": script, "args": list(args)})

    def invoke(self, command, args=None):
        result = self.call("POST", "/execute/async", {
            "script": """const done = arguments[arguments.length - 1];
                window.__TAURI_INTERNALS__.invoke(arguments[0], arguments[1])
                .then(value => done({value})).catch(error => done({error: String(error)}));""",
            "args": [command, args or {}],
        })
        require("error" not in result, result)
        return result.get("value")

    def click(self, label):
        wait_for(lambda: self.script("return Array.from(document.querySelectorAll('button')).some("
                                    "b => b.textContent.trim() === arguments[0]);", label),
                 f"rendered {label}", [])
        element = self.call("POST", "/element", {
            "using": "xpath", "value": f"//button[normalize-space(.)='{label}']",
        })
        key = element["element-6066-11e4-a52e-4f735466cecf"]
        wait_for(lambda: self.call("GET", f"/element/{key}/enabled"), f"enabled {label}", [])
        self.call("POST", f"/element/{key}/click", {})

    def text(self):
        return self.script("return document.body.innerText")

    def fill(self, selector, text):
        element = self.call("POST", "/element", {"using": "css selector", "value": selector})
        key = element["element-6066-11e4-a52e-4f735466cecf"]
        self.call("POST", f"/element/{key}/clear", {})
        self.call("POST", f"/element/{key}/value", {"text": text})

    def select(self, selector, value):
        wait_for(lambda: self.script("const e = document.querySelector(arguments[0]); "
                                     "return e && !e.matches(':disabled');", selector),
                 f"enabled {selector}", [])
        self.script("const e = document.querySelector(arguments[0]); e.value = arguments[1]; "
                    "e.dispatchEvent(new Event('change', {bubbles: true}));", selector, value)

    def screenshot(self, name):
        data = self.call("GET", "/screenshot")
        (self.args.output / f"{name}.png").write_bytes(base64.b64decode(data, validate=True))

    def close(self):
        try:
            if self.session:
                status = self.invoke("node_status")
                if status.get("controllable", status["managed"]):
                    self.invoke("stop_node", {"controlId": status.get("controlId")})
                self.script("window.__TAURI_INTERNALS__.invoke('exit_app', {acknowledgedError: true}); return true")
        except (RuntimeError, OSError):
            pass
        stop(self.process)


def verify(args):
    args.output.mkdir(parents=True)
    ports = allocate_ports(10)
    primary, witness = [
        Node(args.output / name, args.tendermint, args.go_app, args.fixture, ports[i:i + 3], i == 0)
        for name, i in (("primary", 0), ("witness", 3))
    ]
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    genesis["consensus_params"]["version"] = {"app_version": "8"}
    genesis_path = args.output / "Начальное состояние.json"
    genesis_path.write_text(json.dumps(genesis), encoding="utf-8")
    for node in (primary, witness):
        (node.home / "config/genesis.json").write_bytes(genesis_path.read_bytes())
        config = node.home / "config/config.toml"
        # Keep fixture block time near the wall clock over this longer GUI run.
        config.write_text(set_toml(logs(config), "consensus", {"timeout_commit": '"1100ms"'}),
                          encoding="utf-8")
    desktop = None
    try:
        primary.launch(witness, "initial", use_rust=False)
        witness.launch(primary, "initial", use_rust=False)
        processes = primary.processes + witness.processes
        first = int(genesis["initial_height"])
        wait_for(lambda: min(primary.height(), witness.height()) >= first + 5, "two Go nodes", processes)
        desktop = Desktop(args, ports[6], ports[7])
        wait_for(lambda: "Обзор ноды" in desktop.text(), "Minter overview", processes)
        desktop.screenshot("first-launch")
        settings = desktop.invoke("load_settings")
        settings.update({
            "mode": "full", "dataDir": str(args.output / "Данные полной ноды"),
            "fullStart": "genesis", "genesis": str(genesis_path),
            "apiPort": ports[8], "p2pPort": ports[9], "storageProfile": "full",
            "peers": [f"{primary.node_id}@127.0.0.1:{primary.p2p}"],
        })
        desktop.invoke("save_settings", {"settings": settings})
        desktop.call("POST", "/refresh", {})
        wait_for(lambda: "Запустить ноду" in desktop.text(), "configured full node", processes)
        desktop.click("Запустить ноду")

        def full_ready():
            status = desktop.invoke("node_status")
            if (status["managed"] and status["phase"] in ("syncing", "running") and status["node"]
                    and int(status["node"]["height"]) >= first + 5):
                return status
            return None

        full = wait_for(full_ready, "GUI launched full Rust node", processes, timeout=90)
        height = int(full["node"]["height"])
        signed = primary.rpc("commit", height=height)["signed_header"]["header"]
        require(full["node"]["appHash"].lower() == signed["app_hash"].lower(), full)
        require(full["managed"], "GUI does not own its launched node")
        wait_for(lambda: int(desktop.script(
            "return document.querySelector('.height').textContent.replace(/[^0-9]/g, '')") or "0") >= height,
                 "rendered live height", processes)
        desktop.screenshot("full-running")
        desktop.click("Перезапустить")
        recovered = wait_for(lambda: (s if (s := full_ready()) and
                             int(s["node"]["height"]) > height else None),
                             "GUI restarted full node", processes, timeout=90)
        desktop.click("Остановить")
        wait_for(lambda: not desktop.invoke("node_status")["managed"], "graceful stop", processes)
        log = args.output / "Данные полной ноды/node.log"
        require("minterd завершился с кодом" not in logs(log), "daemon did not stop successfully")
        stored = subprocess.run([args.cli, "iavl", "history", "--db",
                                 args.output / "Данные полной ноды/state"], check=True,
                                capture_output=True, text=True, timeout=30, creationflags=HIDE_WINDOW)
        disk = json.loads(stored.stdout)
        require(disk["head"]["height"] >= int(recovered["node"]["height"])
                and disk["keepIavlVersions"] == 120, disk)

        daemon = args.application.with_name("minterd" + (".exe" if os.name == "nt" else ""))
        def history():
            value = subprocess.run([daemon, "native-history", "status", "--db",
                                   args.output / "Данные полной ноды/state"], check=True,
                                   capture_output=True, text=True, timeout=30, creationflags=HIDE_WINDOW)
            return json.loads(value.stdout)

        profiles = [history()]
        require(profiles[0]["storageProfile"] == "full" and profiles[0]["retention"] == {
            "keepIavlVersions": 120, "keepEventBlocks": 100000, "keepNativeBlocks": 100000,
        }, profiles[0])
        for profile, limits in [("archive", (0, 0, 0)), ("full", (120, 100000, 100000)),
                                ("custom", (4, 0, 0))]:
            desktop.click("Настройки")
            desktop.select("#storage-profile", profile)
            if profile == "custom":
                desktop.fill("#keep-versions", "4")
                desktop.fill("#keep-event-blocks", "0")
                desktop.fill("#keep-native-blocks", "0")
            desktop.click("Сохранить настройки")
            settings = wait_for(lambda: (s if (s := desktop.invoke("load_settings"))["storageProfile"]
                                == profile else None), f"saved profile {profile}", processes)
            desktop.screenshot(f"profile-{profile}")
            desktop.invoke("start_node")
            current = wait_for(full_ready, f"GUI profile {profile}", processes, timeout=90)
            signed = primary.rpc("commit", height=int(current["node"]["height"]))["signed_header"]["header"]
            require(current["node"]["appHash"].lower() == signed["app_hash"].lower(), current)
            desktop.invoke("stop_node")
            profile_disk = history()
            require(tuple(profile_disk["retention"][key] for key in
                    ("keepIavlVersions", "keepEventBlocks", "keepNativeBlocks")) == limits, profile_disk)
            profiles.append(profile_disk)

        config = args.output / "Профиль GUI/settings.json"
        shared = subprocess.run([args.cli, "node", "settings", "--config", config],
                                capture_output=True, check=True, text=True, encoding="utf-8", timeout=15,
                                creationflags=HIDE_WINDOW)
        require(json.loads(shared.stdout) == settings, "CLI and GUI settings differ")

        # A failed local owner must remain diagnosable without hiding its replacement.
        desktop.click("К обзору")
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", ports[9]))
            occupied.listen()
            desktop.click("Запустить ноду")
            own_failure = wait_for(lambda: (s if (s := desktop.invoke("node_status"))["phase"] == "error"
                                    and not s["managed"] else None), "GUI retained its child failure", processes)
            require(own_failure["error"], own_failure)
            try:
                desktop.invoke("exit_app")
                raise AssertionError("unacknowledged worker failure was hidden by exit")
            except RuntimeError as error:
                require("minterd" in str(error), str(error))
            require("Minter" in desktop.text(), "failed exit closed the application")

        # CLI and GUI control the same profile through a generation-bound channel.
        external_log = args.output / "external-cli.log"
        external = None
        try:
            with external_log.open("wb") as output:
                external = subprocess.Popen([
                    args.cli, "node", "run", "--config", config,
                    "--shutdown-on-stdin-close",
                ], stdin=subprocess.PIPE, stdout=output, stderr=subprocess.STDOUT,
                   creationflags=HIDE_WINDOW)
            attached = wait_for(lambda: (s if (s := desktop.invoke("node_status"))["phase"] == "attached"
                                else None), "GUI attached to independent CLI node", processes)
            require(not attached["managed"] and attached["controllable"] and attached["controlId"], attached)
            require(attached["previousError"] == own_failure["error"] and not attached["error"], attached)
            wait_for(lambda: "Подключена локально" in desktop.text(),
                     "GUI rendered the current CLI owner", processes)
            desktop.screenshot("external-cli-controllable")
            for command, values in [("save_settings", {"settings": settings}), ("start_node", None)]:
                try:
                    desktop.invoke(command, values)
                    raise AssertionError(f"{command} ignored the CLI configuration lease")
                except RuntimeError as error:
                    require("используются другим процессом" in str(error), str(error))
            desktop.click("Остановить")
            wait_for(lambda: not desktop.invoke("node_status")["controllable"], "GUI stopped CLI owner", processes)
            desktop.click("Запустить ноду")
            takeover = wait_for(full_ready, "GUI took over from the CLI", processes, timeout=90)
            require(takeover["controlId"] != attached["controlId"], takeover)
            require(external.wait(timeout=40) == 0, "old CLI cleanup failed")
            for command in ["stop_node", "exit_app"]:
                try:
                    desktop.invoke(command, {"controlId": attached["controlId"]})
                    raise AssertionError("stale owner command reached replacement")
                except RuntimeError as error:
                    require("Владелец ноды изменился" in str(error), str(error))
            require(desktop.invoke("node_status")["managed"], "stale command stopped GUI replacement")
            from_cli = subprocess.run([args.cli, "node", "stop", "--config", config],
                                      capture_output=True, timeout=50, creationflags=HIDE_WINDOW)
            require(from_cli.returncode == 0, from_cli.stderr.decode(errors="replace"))
            wait_for(lambda: not desktop.invoke("node_status")["managed"], "CLI stopped GUI child", processes)
            desktop.screenshot("cli-stopped-gui-node")
            handoff = {"cli": attached, "gui": takeover, "staleCommandsRejected": True,
                       "oldCleanupPreservedReplacement": True, "cliStopExit": from_cli.returncode,
                       "localFailurePreserved": own_failure, "unacknowledgedExitRejected": True}
        finally:
            if external:
                external.stdin.close()
                try:
                    require(external.wait(timeout=40) == 0, "independent CLI did not stop gracefully")
                finally:
                    stop(external)

        # A daemon bind failure must reach the foreground CLI exit status.
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", ports[9]))
            occupied.listen()
            failed = subprocess.run([args.cli, "node", "run", "--config", config],
                                    capture_output=True, timeout=30, creationflags=HIDE_WINDOW)
            require(failed.returncode != 0, "foreground CLI swallowed daemon failure")
            (args.output / "foreground-failure.log").write_bytes(failed.stdout + failed.stderr)
        desktop.invoke("save_settings", {"settings": settings})
        history()

        anchor_height = min(primary.height(), witness.height()) - 2
        signed = primary.rpc("commit", height=anchor_height)["signed_header"]
        anchor = args.output / "Доверенная точка.json"
        anchor.write_text(json.dumps({
            "chainId": genesis["chain_id"], "initialHeight": str(first),
            "height": str(anchor_height), "hash": signed["commit"]["block_id"]["hash"],
            "trustingPeriodSecs": 3600,
        }), encoding="utf-8")
        settings.update({
            "mode": "light", "dataDir": str(args.output / "Данные лёгкого клиента"),
            "primary": f"http://127.0.0.1:{primary.rpc_port}",
            "witness": f"http://127.0.0.1:{witness.rpc_port}", "lightAnchor": str(anchor),
        })
        desktop.invoke("save_settings", {"settings": settings})
        desktop.call("POST", "/refresh", {})
        wait_for(lambda: "Лёгкий клиент" in desktop.text(), "light settings persisted", processes)
        desktop.click("Запустить ноду")
        light = wait_for(lambda: (s if (s := desktop.invoke("node_status"))["node"] and
                         int(s["node"]["height"]) >= anchor_height and s["phase"] == "running" else None),
                         "GUI verified a light header", processes, timeout=90)
        desktop.screenshot("light-running")
        desktop.click("Остановить")
        require(not desktop.invoke("node_status")["managed"], "light worker kept running")
        require((args.output / "Данные лёгкого клиента/light.redb").is_file(), "missing redb")
        desktop.click("Настройки")
        wait_for(lambda: "Настройки ноды" in desktop.text(), "settings screen", processes)
        new_folder = str(args.output / "Папка из формы GUI")
        desktop.fill("#data-dir", new_folder)
        desktop.click("Сохранить настройки")
        wait_for(lambda: desktop.invoke("load_settings")["dataDir"] == new_folder,
                 "settings saved from the UI form", processes)
        desktop.screenshot("light-settings")
        desktop.click("К обзору")
        desktop.click("Весь журнал")
        wait_for(lambda: "Последние 120 строк" in desktop.text(), "log screen", processes)
        desktop.screenshot("light-log")
        short_height = min(primary.height(), witness.height()) - 1
        short_signed = primary.rpc("commit", height=short_height)["signed_header"]
        short_anchor = args.output / "Короткое доверие.json"
        short_anchor.write_text(json.dumps({
            "chainId": genesis["chain_id"], "initialHeight": str(first),
            "height": str(short_height), "hash": short_signed["commit"]["block_id"]["hash"],
            "trustingPeriodSecs": 15,
        }), encoding="utf-8")
        short_dir = args.output / "Проверка срока доверия"
        short_dir.mkdir()
        short_init = subprocess.run([args.cli, "light", "init", "--db", short_dir / "light.redb",
                        "--anchor", short_anchor, "--primary", settings["primary"],
                        "--witness", settings["witness"]], capture_output=True,
                       timeout=30, creationflags=HIDE_WINDOW)
        require(short_init.returncode == 0, short_init.stderr.decode(errors="replace"))
        short_settings = {**settings, "dataDir": str(short_dir), "lightAnchor": str(short_anchor)}
        desktop.invoke("save_settings", {"settings": short_settings})
        current = desktop.invoke("node_status")
        require(current["node"] and not current["node"]["stale"], current)
        expired = wait_for(lambda: (s if (s := desktop.invoke("node_status"))["node"]["stale"] else None),
                           "cached light trust expired while GUI remained open", processes, timeout=25)
        desktop.invoke("save_settings", {"settings": settings})
        desktop.close()
        desktop = None
        primary.close()
        witness.close()
        processes = []
        desktop = Desktop(args, *allocate_ports(2))
        cached = wait_for(lambda: (s if (s := desktop.invoke("node_status"))["node"]
                          and int(s["node"]["height"]) >= int(light["node"]["height"]) else None),
                          "cached light status after GUI restart without RPC", processes)
        require(cached["phase"] == "stopped" and not cached["managed"], cached)
        desktop.screenshot("light-offline-recovered")
        result = {
            "platform": platform.platform(), "full": full, "afterRestart": recovered,
            "storedHead": disk, "light": light, "unicodeDataPaths": True,
            "gracefulStop": True, "attachedCli": attached, "settingsSavedFromUi": True,
            "lightOfflineRecovered": cached,
            "lightTrustExpired": expired,
            "storageProfiles": profiles, "sharedControllerCli": True,
            "configurationLease": True, "foregroundFailureExit": failed.returncode,
            "crossProcessControl": handoff,
            "uiActions": ["start", "restart", "stop", "settings", "logs"],
        }
        (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n",
                                                encoding="utf-8")
        return result
    except BaseException:
        if desktop:
            try:
                desktop.screenshot("failure")
                (args.output / "failure-screen.txt").write_text(desktop.text(), encoding="utf-8")
            except (RuntimeError, OSError):
                pass
        raise
    finally:
        if desktop:
            desktop.close()
        primary.close()
        witness.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("application", "driver", "native_driver", "cli", "tendermint", "go_app", "fixture", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
