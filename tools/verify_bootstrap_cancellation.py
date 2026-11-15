"""Cancel a real native checkpoint import and resume through the shared supervisor."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time

from verify_send_devnet import HTTP, HIDE_WINDOW, allocate_ports, logs, require, stop, wait_for

PARTS = {"manifest.json", "snapshot.zlib", "state.pb", "blocks.pb", "history.pb"}

def sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def verify(args):
    args.output.mkdir(parents=True)
    require(args.daemon == args.cli.with_name("minterd" + (".exe" if os.name == "nt" else "")),
            "daemon must be the supervisor's sibling binary")
    manifest = json.loads((args.package / "manifest.json").read_bytes())
    require(sha(args.package / "manifest.json") == args.trusted, "trusted manifest differs")
    require(manifest["nodes"] > 20_000, "use a real snapshot large enough for an intermediate batch")
    before = {p.name: sha(p) for p in args.package.iterdir() if p.is_file() and p.name in PARTS}
    p2p, api = allocate_ports(2)
    config = args.output / "Настройки.json"
    process = None

    def cli(*command, check=True):
        return subprocess.run([args.cli, *command], capture_output=True, text=True,
                              encoding="utf-8", check=check, timeout=45, creationflags=HIDE_WINDOW)

    def launch(command, label, input_mode=subprocess.PIPE):
        path = args.output / f"{label}.log"
        with path.open("wb") as output:
            child = subprocess.Popen(command, stdin=input_mode, stdout=output, stderr=subprocess.STDOUT,
                                     creationflags=HIDE_WINDOW)
        return child, path

    def daemon_command(database, package=args.package, trusted=args.trusted):
        return [args.daemon, "native-sync", "--db", database, "--checkpoint", package,
                "--checkpoint-sha256", trusted, "--p2p-listen", "127.0.0.1:0",
                "--shutdown-on-stdin-close"]

    def not_published(database):
        result = cli("iavl", "history", "--db", database, check=False)
        require(result.returncode != 0, "cancelled partial snapshot published a head")

    try:
        # EOF is present before the child is launched. Registration must precede bootstrap.
        pre_database = args.output / "Ранняя отмена/state"
        process, path = launch(daemon_command(pre_database), "early-eof", subprocess.DEVNULL)
        require(process.wait(timeout=35) == 0, logs(path))
        process = None
        require("Native startup cancelled" in logs(path), logs(path))
        not_published(pre_database)

        data = args.output / "Данные со снимком"
        database = data / "state"
        process, path = launch(daemon_command(database), "partial-import")

        def intermediate(log_path, offset=0):
            matches = re.findall(r"Importing snapshot: (\d+)/(\d+) IAVL nodes", logs(log_path)[offset:])
            return next(((int(done), int(total)) for done, total in matches
                         if 0 < int(done) < int(total)), None)

        progress = wait_for(lambda: intermediate(path), "intermediate durable snapshot batch",
                            [(process, path)], timeout=180)
        requested = time.monotonic()
        process.stdin.close()
        code = process.wait(timeout=35)
        elapsed = time.monotonic() - requested
        require(code == 0 and "Native startup cancelled" in logs(path), logs(path))
        process = None
        not_published(database)

        remote_cancel = None

        # Same snapshot bytes, a different full package identity: cannot reuse the intent.
        changed = args.output / "Другой пакет"
        changed.mkdir()
        for original in args.package.iterdir():
            if original.is_file() and original.name in PARTS - {"manifest.json"}:
                try:
                    os.link(original, changed / original.name)
                except OSError:
                    shutil.copy2(original, changed / original.name)
        altered = {**manifest, "comment": "different trusted native package"}
        (changed / "manifest.json").write_text(json.dumps(altered), encoding="utf-8")
        command = daemon_command(database, changed, sha(changed / "manifest.json"))
        process, other_log = launch(command, "different-package")
        other_code = process.wait(timeout=45)
        require(other_code != 0 and "same trusted checkpoint package" in logs(other_log), logs(other_log))
        process.stdin.close()
        process = None
        not_published(database)

        settings = json.loads(cli("node", "init", "--config", config, "--data-dir", data, "--mode", "full").stdout)
        settings.update({"checkpoint": str(args.package), "checkpointSha256": args.trusted,
                         "fullStart": "checkpoint", "storageProfile": "full", "apiPort": api, "p2pPort": p2p})
        config.write_text(json.dumps(settings, ensure_ascii=False), encoding="utf-8")
        process, supervisor_log = launch([args.cli, "node", "run", "--config", config,
                                          "--shutdown-on-stdin-close"], "supervisor-partial-import")
        managed_log = data / "node.log"
        managed_progress = wait_for(lambda: intermediate(managed_log), "supervised intermediate batch",
                                    [(process, supervisor_log)], timeout=180)
        requested = time.monotonic()
        process.stdin.close()
        managed_code = process.wait(timeout=35)
        managed_elapsed = time.monotonic() - requested
        require(managed_code == 0 and "Native startup cancelled" in logs(managed_log),
                logs(supervisor_log) + logs(managed_log))
        process = None
        not_published(database)

        if args.remote_stop:
            offset = len(logs(managed_log))
            process, remote_log = launch([args.cli, "node", "run", "--config", config,
                                          "--shutdown-on-stdin-close"], "supervisor-remote-partial")
            remote_progress = wait_for(lambda: intermediate(managed_log, offset), "new supervised import batch",
                                       [(process, remote_log)], timeout=180)
            requested = time.monotonic()
            stopped = cli("node", "stop", "--config", config)
            require(process.wait(timeout=45) == 0, logs(remote_log))
            remote_elapsed = time.monotonic() - requested
            require("Native startup cancelled" in logs(managed_log)[offset:], logs(managed_log)[offset:])
            process.stdin.close()
            process = None
            not_published(database)
            remote_cancel = {"exitCode": stopped.returncode, "seconds": round(remote_elapsed, 3),
                             "intermediateNodes": remote_progress[0]}

        def ready():
            with HTTP.open(f"http://127.0.0.1:{api}/v2/status", timeout=3) as reply:
                value = json.load(reply)
            return value if int(value["latest_block_height"]) == manifest["height"] else None

        recovered = []
        for mode in ["checkpoint", "resume"]:
            settings["fullStart"] = mode
            config.write_text(json.dumps(settings, ensure_ascii=False), encoding="utf-8")
            process, supervisor_log = launch([args.cli, "node", "run", "--config", config,
                                               "--shutdown-on-stdin-close"], f"supervisor-{mode}")
            status = wait_for(ready, f"shared supervisor {mode}", [(process, supervisor_log)], timeout=240)
            process.stdin.close()
            require(process.wait(timeout=45) == 0, logs(supervisor_log))
            process = None
            history = json.loads(cli("iavl", "history", "--db", database).stdout)
            require(history["head"]["height"] == manifest["height"] and
                    history["head"]["appHash"].lower() == manifest["appHash"].lower(), history)
            recovered.append({"mode": mode, "api": status, "stored": history})
        bad = {**settings, "dataDir": str(args.output / "Неверный SHA"), "fullStart": "checkpoint",
               "checkpointSha256": "0" * 64}
        config.write_text(json.dumps(bad), encoding="utf-8")
        failed = cli("node", "run", "--config", config, check=False)
        bad_log = logs(Path(bad["dataDir"]) / "node.log")
        require(failed.returncode != 0 and "trusted digest" in bad_log, failed.stderr + bad_log)
        (args.output / "bad-manifest.log").write_text(failed.stderr + bad_log, encoding="utf-8")
        require(before == {p.name: sha(p) for p in args.package.iterdir() if p.is_file() and p.name in PARTS}, "input package changed")
        result = {"platform": platform.platform(), "height": manifest["height"], "appHash": manifest["appHash"],
                  "manifestSha256": args.trusted, "inputHashes": before, "inputsStable": True,
                  "intermediateNodes": progress[0], "totalNodes": progress[1], "cancelSeconds": round(elapsed, 3),
                  "cancelExit": code, "differentPackageExit": other_code, "recovered": recovered,
                  "supervisedCancelExit": managed_code, "supervisedCancelSeconds": round(managed_elapsed, 3),
                  "supervisedIntermediateNodes": managed_progress[0],
                  "remoteCancel": remote_cancel,
                  "badManifestExit": failed.returncode, "earlyEofExit": 0}
        (args.output / "result.json").write_text(json.dumps(result, indent=2)+"\n", encoding="utf-8")
        return result
    finally:
        stop(process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["daemon", "cli", "package"]:
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    parser.add_argument("trusted")
    parser.add_argument("output", type=lambda value: Path(value).resolve())
    parser.add_argument("--remote-stop", action="store_true")
    print(json.dumps(verify(parser.parse_args())))
