"""Compare Rust P2P block responses with a signed Go replay corpus."""

import argparse
import gzip
import hashlib
import json
import platform
from pathlib import Path

from verify_mainnet_empty import fields, read_varint
from verify_native_peer import cli_run, success
from verify_send_devnet import require


def read_block(corpus, height):
    for path in corpus.glob("*.pb"):
        first, last = map(int, path.stem.split("-"))
        if not first <= height <= last:
            continue
        with path.open("rb") as source:
            for _ in range(first, height):
                size = read_varint(source)
                require(len(source.read(size)) == size, f"truncated shard {path.name}")
            size = read_varint(source)
            payload = source.read(size)
            require(len(payload) == size, f"truncated shard {path.name}")
            header = fields(fields(fields(fields(payload)[3])[1])[1])
            require(header[3] == height, f"wrong height in {path.name}")
            return payload, header
    raise RuntimeError(f"missing corpus shard at {height}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("minterctl", type=lambda path: Path(path).resolve())
    parser.add_argument("blocks", type=Path)
    parser.add_argument("reference", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--peer", required=True)
    parser.add_argument("--heights", nargs="+", required=True, type=int)
    args = parser.parse_args()
    reference = json.loads((args.reference / "result.json").read_bytes())
    chain = reference["chain"]
    heights = sorted(set(args.heights))
    require(reference["from"] <= heights[0] <= heights[-1] <= reference["through"]
            < reference["signedThrough"], "requested height outside signed replay")
    go_hashes = {}
    with gzip.open(args.reference / "abci.jsonl.gz", "rt", encoding="utf-8") as source:
        for line in source:
            row = json.loads(line)
            if row["height"] in heights:
                go_hashes[row["height"]] = row["block_hash"]
            if row["height"] >= heights[-1]:
                break
    require(sorted(go_hashes) == heights, "missing Go replay heights")
    args.output.mkdir()
    connection = ["--peer", args.peer, "--chain", chain]
    request = [item for height in heights for item in ("--height", str(height))]
    output = args.output / "blocks.pb"
    captured_report = success(cli_run(
        args.minterctl, "block", *connection, *request, "--output", output,
    ))
    status = captured_report["peer"]
    require(status["base"] <= heights[0] <= heights[-1] <= status["height"], "peer range is too short")
    require(len(captured_report["blocks"]) == len(heights), "unexpected block count")
    evidence = []
    with output.open("rb") as captured:
        for height, report in zip(heights, captured_report["blocks"], strict=True):
            payload, header = read_block(args.blocks, height)
            require(captured.read(read_varint(captured)) == payload, f"P2P payload differs at {height}")
            require(report["peerId"] == status["peerId"] and report["chainId"] == chain
                    and report["height"] == height and report["blockHash"] == go_hashes[height]
                    and report["appHash"] == header.get(11, b"").hex()
                    and report["wireSha256"] == hashlib.sha256(payload).hexdigest(),
                    f"block report differs at {height}")
            evidence.append({**report, "payloadBytes": len(payload)})
        require(captured.read(1) == b"", "unexpected trailing block response")
    result = {"platform": platform.platform(), "peer": status, "blocks": evidence}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
