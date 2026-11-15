"""Verify compact pre-snapshot mainnet history, restart, malformed files and replay."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import platform
import subprocess

from verify_mainnet_empty import fields, read_varint
from verify_native_sync import decode_trace, encode, header, replace
from verify_send_devnet import HIDE_WINDOW, require

MAGIC = b"MINTER-CONTEXT\x01"


def frames(data):
    require(data.startswith(MAGIC), "context magic differs")
    stream = io.BytesIO(data[len(MAGIC):])
    chunks, payloads = [], []
    while stream.tell() < len(data) - len(MAGIC):
        start = stream.tell()
        size = read_varint(stream)
        raw = stream.read(size)
        require(len(raw) == size, "context frame truncated")
        payloads.append(raw)
        end = stream.tell()
        chunks.append(data[len(MAGIC)+start:len(MAGIC)+end])
    return chunks, payloads


def verify(args):
    output = args.output
    output.mkdir(parents=True)
    manifest = json.loads((args.checkpoint / "manifest.json").read_bytes())
    trusted = hashlib.sha256((args.checkpoint / "manifest.json").read_bytes()).hexdigest()
    data = args.context.read_bytes()
    chunks, payloads = frames(data)
    packet = fields(payloads[0])
    anchor, floor = packet[2], packet[3]
    require(anchor == manifest["height"] and len(chunks) == anchor-floor+1, "range differs")
    database = output / "state"

    def command(name, options, error=None):
        path = output / f"{name}.log"
        with path.open("wb") as log:
            result = subprocess.run([args.daemon, *options], stdout=log, stderr=subprocess.STDOUT,
                                    timeout=360, creationflags=HIDE_WINDOW)
        text = path.read_text(errors="replace")
        if error:
            require(result.returncode != 0 and error in text, f"{name}: {text[-2000:]}")
            return None
        require(result.returncode == 0, f"{name}: {text[-2000:]}")
        return decode_trace(path)[-1]

    def status(name):
        return command(name, ["native-history", "status", "--db", database])

    def invariant(current, expected_floor):
        require(current["head"] == original["head"] and current["archiveFrom"] == anchor
                and current["archiveThrough"] == anchor and current["contextFrom"] == expected_floor,
                f"context import changed application/archive or floor: {current}")
        after = json.loads(subprocess.check_output([args.cli, "iavl", "history", "--db", database],
                                                  timeout=30, creationflags=HIDE_WINDOW))
        require(after == retention, "context import changed retained IAVL")

    command("restore", ["native-sync", "--db", database, "--checkpoint", args.checkpoint,
                        "--checkpoint-sha256", trusted, "--blocks", args.checkpoint / "blocks.pb",
                        "--through", str(anchor), "--keep-iavl-versions", "4"])
    original = status("initial-status")
    retention = json.loads(subprocess.check_output([args.cli, "iavl", "history", "--db", database],
                                                   timeout=30, creationflags=HIDE_WINDOW))
    invariant(original, anchor)
    go = json.loads(subprocess.check_output([args.go_verifier, args.context, args.checkpoint / "blocks.pb"],
                                           timeout=90, creationflags=HIDE_WINDOW))
    require(go["records"] == anchor-floor and go["sha256"] == hashlib.sha256(data).hexdigest(), go)
    (output / "go-reference.json").write_text(json.dumps(go, indent=2) + "\n")
    imported = ["native-history", "import", "--db", database, "--input"]

    # Bad anchor metadata and a missing immediate predecessor must publish nothing.
    wrong = output / "wrong-chain.pb"
    from verify_mainnet_empty import varint
    changed = replace(payloads[0], 1, b"other-chain")
    wrong.write_bytes(MAGIC + varint(len(changed)) + changed + b"".join(chunks[1:]))
    command("wrong-chain", [*imported, wrong], "differs from the snapshot anchor")
    invariant(status("after-wrong-chain"), anchor)
    gap = output / "gap.pb"
    gap.write_bytes(MAGIC + chunks[0] + b"".join(chunks[2:]))
    command("gap", [*imported, gap], "differs from its trusted successor")
    invariant(status("after-gap"), anchor)

    # A complete valid first record followed by EOF must fail, keeping only that prefix.
    truncated = output / "boundary-truncated.pb"
    truncated.write_bytes(MAGIC + b"".join(chunks[:2]))
    command("boundary-truncated", [*imported, truncated], "truncated context package")
    invariant(status("after-boundary"), anchor-1)
    result = command("resumed-import", [*imported, args.context])
    require(result == {"anchor": anchor, "from": floor, "records": anchor-floor, "appended": anchor-floor-1}, result)
    invariant(status("after-resume"), floor)
    repeated = command("repeated-import", [*imported, args.context])
    require(repeated["appended"] == 0, repeated)
    invariant(status("after-repeat"), floor)
    extra = output / "trailing.pb"
    extra.write_bytes(data + b"\x00")
    command("trailing", [*imported, extra], "trailing context package data")
    invariant(status("after-trailing"), floor)
    extra_prefix = output / "trailing-length-prefix.pb"
    extra_prefix.write_bytes(data + b"\x01")
    with (output / "go-trailing-length-prefix.log").open("wb") as log:
        rejected = subprocess.run([args.go_verifier, extra_prefix, args.checkpoint / "blocks.pb"],
                                  stdout=log, stderr=subprocess.STDOUT, timeout=90, creationflags=HIDE_WINDOW)
    require(rejected.returncode != 0 and b"trailing context data" in
            (output / "go-trailing-length-prefix.log").read_bytes(), "Go accepted a truncated trailing frame")
    command("trailing-length-prefix", [*imported, extra_prefix], "trailing context package data")
    invariant(status("after-trailing-length-prefix"), floor)
    exported = output / "reexport.pb"
    command("reexport", ["native-history", "export", "--db", database, "--anchor", str(anchor),
                         "--from", str(floor), "--output", exported])
    require(exported.read_bytes() == data, "re-export changed authenticated context")

    # Continue executing one actual mainnet block using the unchanged checkpoint state.
    continued = command("continued", ["native-sync", "--db", database, "--blocks", args.continuation,
                                       "--through", str(anchor+1), "--trace"])
    source = io.BytesIO(args.continuation.read_bytes())
    blocks = [source.read(read_varint(source)) for _ in range(4)]
    expected_root = header(blocks[2])[11].hex()
    require(continued["through"] == anchor+1 and continued["appHash"] == expected_root, continued)
    final = status("final-status")
    require(final["head"] == {"height": anchor+1, "appHash": expected_root}
            and final["contextFrom"] == floor and final["archiveFrom"] == anchor
            and final["archiveThrough"] == anchor+1, final)
    return {"platform": platform.platform(), "anchor": anchor, "from": floor, "records": anchor-floor,
            "contextBytes": len(data), "contextSha256": hashlib.sha256(data).hexdigest(),
            "trustedManifestSha256": trusted, "go": go, "original": original, "final": final,
            "retentionBeforeContext": retention, "boundaryTruncationRefused": True,
            "resumedAfterOneRecord": True, "repeatedImportAppended": repeated["appended"],
            "trailingDataRefused": True, "wrongChainRefused": True, "gapRefused": True,
            "trailingLengthPrefixRefusedByGoAndRust": True,
            "reexportByteEqual": True, "continuedThrough": anchor+1}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "cli", "go_verifier", "checkpoint", "context", "continuation", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    result = verify(args)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))
