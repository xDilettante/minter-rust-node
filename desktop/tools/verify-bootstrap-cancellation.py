"""Stop and resume a real checkpoint import through the production desktop UI."""
import argparse
import hashlib
import importlib.util
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import platform
import re
import subprocess
import threading
import time

spec = importlib.util.spec_from_file_location("desktop_verification", Path(__file__).with_name("verify-desktop.py"))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class ForeignStatus(BaseHTTPRequestHandler):
    def do_GET(self):
        self.server.contacted = True
        body = json.dumps({"node_id": "f" * 40, "network": "another-chain",
                           "latest_block_height": "900", "latest_app_hash": "aabb",
                           "catching_up": False}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def verify(args):
    args.output.mkdir(parents=True)
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    digest = hashlib.sha256((args.checkpoint / "manifest.json").read_bytes()).hexdigest()
    helper.require(digest == args.trusted, "trusted manifest differs")
    data = args.output / "Данные ноды"
    log = data / "node.log"
    desktop = None
    try:
        desktop = helper.Desktop(args, *helper.allocate_ports(2))
        def click_ready(label):
            helper.wait_for(lambda: desktop.script(
                "return Array.from(document.querySelectorAll('button')).some("
                "b => b.textContent.trim() === arguments[0] && !b.disabled)", label),
                f"rendered enabled {label}", [])
            desktop.click(label)

        helper.wait_for(lambda: "Обзор ноды" in desktop.text(), "desktop overview", [])
        api, p2p = helper.allocate_ports(2)
        settings = desktop.invoke("load_settings")
        settings.update({"mode": "full", "fullStart": "checkpoint", "dataDir": str(data),
                         "checkpoint": str(args.checkpoint), "checkpointSha256": args.trusted,
                         "storageProfile": "full", "apiPort": api, "p2pPort": p2p, "peers": []})
        desktop.invoke("save_settings", {"settings": settings})
        desktop.call("POST", "/refresh", {})
        helper.wait_for(lambda: "Запустить ноду" in desktop.text(), "configured desktop", [])
        def progress():
            values = re.findall(r"Importing snapshot: (\d+)/(\d+) IAVL nodes", helper.logs(log))
            return next(((int(done), int(total)) for done, total in values
                         if 0 < int(done) < int(total)), None)

        click_ready("Запустить ноду")
        helper.wait_for(lambda: desktop.invoke("node_status")["managed"], "owned startup", [])
        with ThreadingHTTPServer(("127.0.0.1", api), ForeignStatus) as foreign:
            foreign.contacted = False
            thread = threading.Thread(target=lambda: foreign.serve_forever(poll_interval=0.01), daemon=True)
            thread.start()
            try:
                intermediate = helper.wait_for(progress, "intermediate snapshot batch", [], timeout=180)
                starting = desktop.invoke("node_status")
                helper.require(starting["phase"] == "starting" and starting["node"] is None
                               and starting["controllable"], starting)
                helper.require(not foreign.contacted, "bootstrap queried another node's API")
                desktop.screenshot("bootstrap-with-occupied-api")
                # Standalone API observation is allowed after the worker exits.
                # Remove this fixture before checking the stopped/offline state.
                foreign.shutdown()
                foreign.server_close()
                thread.join(timeout=10)
                requested = time.monotonic()
                click_ready("Остановить")
                stopped = helper.wait_for(lambda: (s if not (s := desktop.invoke("node_status"))["managed"] else None),
                                          "desktop stopped bootstrap", [], timeout=35)
                elapsed = time.monotonic() - requested
                helper.require(stopped["phase"] == "stopped", stopped)
                helper.require("Native startup cancelled" in helper.logs(log) and
                               "minterd завершился с кодом" not in helper.logs(log), helper.logs(log))
                helper.require(not foreign.contacted, "bootstrap queried another node's API")
                desktop.screenshot("cancelled-import")
                history_args = [args.cli, "iavl", "history", "--db", data / "state"]
                partial = subprocess.run(history_args, capture_output=True, timeout=30, creationflags=helper.HIDE_WINDOW)
                helper.require(partial.returncode != 0, "cancelled GUI import published a head")
            finally:
                foreign.shutdown()
                thread.join(timeout=10)
        click_ready("Запустить ноду")

        def ready():
            status = desktop.invoke("node_status")
            return status if status["managed"] and status["node"] and int(status["node"]["height"]) == manifest["height"] else None

        restored = helper.wait_for(ready, "GUI resumed checkpoint", [], timeout=240)
        desktop.screenshot("resumed-import")
        click_ready("Остановить")
        helper.wait_for(lambda: not desktop.invoke("node_status")["managed"], "stopped restored node", [])
        disk = json.loads(subprocess.check_output(history_args, timeout=30, creationflags=helper.HIDE_WINDOW))
        helper.require(disk["head"] == {"height": manifest["height"], "appHash": manifest["appHash"]}, disk)
        result = {"platform": platform.platform(), "manifestSha256": digest,
                  "intermediateNodes": intermediate[0], "totalNodes": intermediate[1],
                  "cancelSeconds": round(elapsed, 3), "cancelled": stopped,
                  "bootstrapWithForeignApi": starting, "foreignApiContacted": foreign.contacted,
                  "restored": restored, "stored": disk, "uiActions": ["start", "stop", "start", "stop"]}
        (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2)+"\n", encoding="utf-8")
        return result
    except BaseException:
        if desktop:
            try:
                desktop.screenshot("failure")
                (args.output / "failure-screen.txt").write_text(desktop.text(), encoding="utf-8")
            except (OSError, RuntimeError):
                pass
        raise
    finally:
        if desktop:
            desktop.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["application", "driver", "native_driver", "cli", "checkpoint"]:
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("trusted")
    parser.add_argument("output", type=lambda value: Path(value).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
