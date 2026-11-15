"""Stop a large real native-body cleanup with the production GUI and resume it."""
import argparse
import importlib.util
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time

spec = importlib.util.spec_from_file_location("desktop_verification", Path(__file__).with_name("verify-desktop.py"))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


def verify(args):
    args.output.mkdir(parents=True)
    data = args.output / "Данные ноды"
    database = data / "state"
    database.mkdir(parents=True)
    for path in args.seed.iterdir():
        if path.is_file() and re.fullmatch(r"[0-9]+\.(sst|log)|MANIFEST-[0-9]+|OPTIONS-[0-9]+|CURRENT|IDENTITY|LOCK", path.name):
            helper.require(not path.is_symlink(), "linked fixture input")
            shutil.copyfile(path, database / path.name)
    daemon = args.application.with_name("minterd" + args.application.suffix)

    def status():
        return json.loads(subprocess.check_output([daemon, "native-history", "status", "--db", database],
                                                 timeout=60, creationflags=helper.HIDE_WINDOW))

    initial = status()
    helper.require(initial["head"] == {
        "height": 40700741, "appHash": "fbc9b7b8bdfb064de75082b404bc863ccdfe9a78f2365340e957c2957dfdedf1"}, initial)
    target = initial["archiveFrom"] + 5000
    keep = initial["head"]["height"] - target + 1
    helper.require(keep > 2, initial)
    desktop = None
    try:
        desktop = helper.Desktop(args, *helper.allocate_ports(2))
        helper.wait_for(lambda: "Обзор ноды" in desktop.text(), "desktop overview", [])
        settings = desktop.invoke("load_settings")
        api, p2p = helper.allocate_ports(2)
        settings.update({"mode": "full", "fullStart": "resume", "dataDir": str(data), "peers": [],
                         "apiPort": api, "p2pPort": p2p, "storageProfile": "custom",
                         "keepVersions": initial["retention"]["keepIavlVersions"],
                         "keepEventBlocks": initial["retention"]["keepEventBlocks"], "keepNativeBlocks": keep})
        desktop.invoke("save_settings", {"settings": settings})
        desktop.call("POST", "/refresh", {})
        desktop.click("Запустить ноду")
        log = data / "node.log"
        helper.wait_for(lambda: "Native maintenance: native-blocks 1000" in helper.logs(log),
                        "GUI cleanup progress", [], timeout=240)
        active = desktop.invoke("node_status")
        helper.require(active["phase"] == "starting" and active["node"] is None and active["controllable"], active)
        desktop.screenshot("cleanup-running")
        began = time.monotonic()
        desktop.click("Остановить")
        cancelled = helper.wait_for(lambda: (s if not (s := desktop.invoke("node_status"))["controllable"] else None),
                                    "GUI stopped cleanup", [], timeout=40)
        elapsed = time.monotonic() - began
        helper.require(cancelled["phase"] == "stopped" and cancelled["error"] is None, cancelled)
        helper.require("Native startup cancelled" in helper.logs(log), helper.logs(log)[-2000:])
        interrupted = status()
        helper.require(initial["archiveFrom"] + 1000 <= interrupted["archiveFrom"] < target
                       and interrupted["head"] == initial["head"], interrupted)
        desktop.screenshot("cleanup-cancelled")
        desktop.click("Запустить ноду")

        def ready():
            s = desktop.invoke("node_status")
            return s if s["managed"] and s["node"] and int(s["node"]["height"]) == initial["head"]["height"] else None

        restored = helper.wait_for(ready, "GUI resumed cleanup", [], timeout=240)
        desktop.screenshot("cleanup-resumed")
        desktop.click("Остановить")
        helper.wait_for(lambda: not desktop.invoke("node_status")["controllable"], "stopped ready node", [])
        final = status()
        helper.require(final["head"] == initial["head"] and final["archiveFrom"] == final["contextFrom"] == target, final)
        result = {"platform": platform.platform(), "initial": initial, "interrupted": interrupted,
                  "final": final, "cancelSeconds": round(elapsed, 3), "cancelled": cancelled,
                  "restored": restored, "fullBlocksRetired": 5000, "uiActions": ["start", "stop", "start", "stop"]}
        (args.output / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
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
    for name in ("application", "driver", "native_driver", "cli", "seed", "output"):
        parser.add_argument(name, type=lambda p: Path(p).resolve())
    print(json.dumps(verify(parser.parse_args()), ensure_ascii=False))
