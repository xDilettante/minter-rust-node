"""Collect a portable developer build, dependency notices and checksums."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def output(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def main():
    windows = os.name == "nt"
    suffix = ".exe" if windows else ""
    label = "windows-x64" if windows else "linux-x64"
    folder = ROOT / "artifacts" / ("minter-rust-node-" + label)
    folder.mkdir(parents=True)
    for name, source in (("minterd", ROOT / "target/release"), ("minterctl", ROOT / "target/release"),
                         ("minter-desktop", ROOT / "desktop/src-tauri/target/release")):
        shutil.copy2(source / (name + suffix), folder / (name + suffix))
    for name in ("LICENSE", "TECHNICAL.md", "THIRD_PARTY_NOTICES.md", "source-provenance.json"):
        shutil.copyfile(ROOT / name, folder / name)
    shutil.copyfile(ROOT / "README.md", folder / "SOURCE-README.md")
    shutil.copytree(ROOT / "docs", folder / "docs")
    shutil.copyfile(ROOT / "docs/portable.md", folder / "README.md")
    (folder / "desktop").mkdir()
    shutil.copyfile(ROOT / "desktop/README.md", folder / "desktop/README.md")
    notices = folder / "licenses"
    notices.mkdir()
    for path in (ROOT / "licenses").rglob("*"):
        if path.is_file():
            shutil.copyfile(path, notices / path.name)
    shutil.copyfile(ROOT / "vendor/tendermint-p2p/LICENSE", notices / "tendermint-p2p-Apache-2.0.txt")
    for path in (ROOT / "crates/minter-api/schema").glob("*LICENSE.txt"):
        shutil.copyfile(path, notices / path.name)
    target = next(line.split(": ", 1)[1] for line in output("rustc", "-vV").splitlines() if line.startswith("host: "))
    dependencies = {}
    for manifest in ("Cargo.toml", "desktop/src-tauri/Cargo.toml"):
        metadata = json.loads(output("cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target,
                                     "--manifest-path", manifest))
        for package in metadata["packages"]:
            key = package["name"] + "-" + package["version"]
            dependencies[key] = {"name": package["name"], "version": package["version"], "license": package["license"]}
            source = Path(package["manifest_path"]).parent
            if source == ROOT or ROOT in source.parents:
                continue
            for path in source.rglob("*"):
                if path.is_file() and path.name.upper().startswith(("LICENSE", "COPYING", "NOTICE", "OFL")):
                    destination = notices / "rust" / key / path.relative_to(source)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(path, destination)
    javascript = {}
    lock = json.loads((ROOT / "desktop/package-lock.json").read_text(encoding="utf-8"))
    for location, package in lock["packages"].items():
        if not location or package.get("dev", False):
            continue
        source = ROOT / "desktop" / location
        if not source.is_dir():
            if package.get("optional", False):
                continue
            raise RuntimeError(f"Missing runtime dependency: {location}")
        found = [p for p in source.iterdir() if p.is_file() and p.name.upper().startswith(("LICENSE", "COPYING", "NOTICE", "OFL"))]
        if not found:
            name = location.rsplit("node_modules/", 1)[1]
            supplement = ROOT / "licenses/javascript" / f"{name}-{package['version']}-MIT.txt"
            if package.get("license") != "MIT" or not supplement.is_file():
                raise RuntimeError(f"Missing runtime dependency notice: {location}")
            found = [supplement, source / "package.json", source / "README.md"]
        javascript[location] = {"version": package["version"], "license": package.get("license")}
        for path in found:
            destination = notices / "javascript" / location / path.name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, destination)
    (folder / "dependency-licenses.json").write_text(json.dumps(dependencies, indent=2) + "\n", encoding="utf-8")
    (folder / "javascript-dependencies.json").write_text(json.dumps(javascript, indent=2) + "\n", encoding="utf-8")
    (folder / "build-info.json").write_text(json.dumps({"commit": output("git", "rev-parse", "HEAD"),
        "rustc": output("rustc", "--version"), "node": output("node", "--version"), "target": target,
        "kind": "experimental developer build; unsigned; GUI runtime dependencies required"}, indent=2) + "\n", encoding="utf-8")
    sums = []
    for path in sorted(folder.rglob("*")):
        if path.is_file():
            with path.open("rb") as stream:
                sha = hashlib.file_digest(stream, "sha256").hexdigest()
            sums.append(f"{sha}  {path.relative_to(folder).as_posix()}")
    (folder / "SHA256SUMS").write_text("\n".join(sums) + "\n", encoding="utf-8")
    if windows:
        archive = folder.with_suffix(".zip")
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as stream:
            for path in sorted(folder.rglob("*")):
                if path.is_file():
                    stream.write(path, folder.name + "/" + path.relative_to(folder).as_posix())
    else:
        archive = folder.with_suffix(".tar.gz")
        with tarfile.open(archive, "w:gz") as stream:
            stream.add(folder, arcname=folder.name)
    print(archive)


if __name__ == "__main__":
    main()
