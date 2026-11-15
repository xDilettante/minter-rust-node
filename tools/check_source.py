"""Reject runtime files and credential-shaped data in the tracked source export."""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FORBIDDEN = {"node_key.json", "priv_validator_key.json", "priv_validator_state.json", ".env"}
DIRECTORIES = {"target", "node_modules", "__pycache__", "node-data", "artifacts"}
RUNTIME_ROOTS = {"data", "state", "node", "checkpoint", "outputs"}
DATABASE_SUFFIXES = {".sst", ".ldb", ".redb", ".wal", ".db"}
PATTERNS = {
    "github-token": re.compile(rb"\bgh[pousr]_[A-Za-z0-9_]{30,}\b"),
    "github-fine-grained-token": re.compile(rb"\bgithub_pat_[A-Za-z0-9_]{30,}\b"),
    "api-key": re.compile(rb"\bsk-(?:proj-)?[A-Za-z0-9_-]{40,}\b"),
    "bot-token": re.compile(rb"\b[0-9]{8,13}:[A-Za-z0-9_-]{30,}\b"),
    "private-key-file": re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"),
}


def main():
    names = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).decode().split("\0")
    failures = []
    count = 0
    for name in filter(None, names):
        relative = Path(name)
        path = ROOT / relative
        count += 1
        if (relative.name in FORBIDDEN or set(relative.parts) & DIRECTORIES
                or relative.parts[0] in RUNTIME_ROOTS
                or relative.suffix.lower() in DATABASE_SUFFIXES
                or re.fullmatch(r"MANIFEST-[0-9]+", relative.name)
                or relative.name in {"CURRENT", "IDENTITY", "LOCK"}
                or (relative.name.startswith(".env.") and relative.name != ".env.example")):
            failures.append((name, "runtime file"))
            continue
        if path.is_symlink():
            failures.append((name, "symlink needs explicit review"))
            continue
        if path.stat().st_size > 5 * 1024 * 1024:
            failures.append((name, "source file exceeds 5 MiB"))
            continue
        data = path.read_bytes()
        for label, pattern in PATTERNS.items():
            if pattern.search(data):
                failures.append((name, label))
    for name, label in failures:
        print(f"{name}: {label} (content redacted)", file=sys.stderr)
    if not count:
        print("No tracked source files; the distribution was not checked", file=sys.stderr)
        return 1
    print(f"Reviewed {count} tracked files; findings: {len(failures)}")
    return bool(failures)


if __name__ == "__main__":
    sys.exit(main())
