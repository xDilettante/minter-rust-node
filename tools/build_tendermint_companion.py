"""Build the Minter retention companion from the authenticated Tendermint module."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import zipfile


MODULE = "github.com/tendermint/tendermint"
VERSION = "v0.34.19"
MODULE_SUM = "h1:y0P1qI5wSa9IRuhKnTDA6IUcOrLi1hXJuALR+R7HFEk="
GOMOD_SUM = "h1:R5+wgIwSxMdKQcmOaeudL0Cjkr3HDkhpcdum6VeU3R4="
PATCHES = Path(__file__).resolve().parent.parent / "integrations/tendermint-0.34.19"


def extract_module(module_zip, target):
    # Go's dirhash.HashZip hashes the sorted filenames and their content digests.
    with zipfile.ZipFile(module_zip) as archive:
        checksum = hashlib.sha256()
        for name in sorted(archive.namelist()):
            with archive.open(name) as source:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
            checksum.update(f"{digest}  {name}\n".encode("utf-8"))
        actual = "h1:" + base64.b64encode(checksum.digest()).decode("ascii")
        if actual != MODULE_SUM:
            raise RuntimeError("Tendermint module archive failed checksum verification")
        archive.extractall(target)
    return Path(target) / f"{MODULE}@{VERSION}"


def build(destination, target, tests, signer):
    destination = destination.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "GOWORK": "off", "CGO_ENABLED": "0"}
    module = json.loads(subprocess.check_output(
        ["go", "mod", "download", "-json", f"{MODULE}@{VERSION}"], env=env))
    actual = (module["Path"], module["Version"], module["Sum"], module["GoModSum"])
    if actual != (MODULE, VERSION, MODULE_SUM, GOMOD_SUM):
        raise RuntimeError("Tendermint source does not match the pinned module")
    with TemporaryDirectory(prefix="minter-tendermint-") as temp:
        source = extract_module(module["Zip"], temp)
        patches = ["retention.patch", "signer.patch"] if signer else ["retention.patch"]
        subprocess.run(["git", "-c", "core.autocrlf=false", "apply", *(str(PATCHES / p) for p in patches)],
                       cwd=source, check=True)
        if tests:
            subprocess.run(["go", "test", "-mod=readonly", "./node", "./store", "./state",
                            "-run", "^TestMinter|^TestPrune"], cwd=source, check=True, env=env)
        env.update(GOOS=target, GOARCH="amd64")
        subprocess.run(["go", "build", "-mod=readonly", "-trimpath", "-buildvcs=false",
                        "-o", str(destination), "./cmd/tendermint"], cwd=source, check=True, env=env)
    print(destination)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--target", choices=("windows", "linux"),
                        default="windows" if os.name == "nt" else "linux")
    parser.add_argument("--test", action="store_true")
    parser.add_argument("--signer", action="store_true",
                        help="enable mutual peer authentication for the private validator TCP listener")
    args = parser.parse_args()
    build(args.destination, args.target, args.test, args.signer)
