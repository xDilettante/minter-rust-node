"""Compare native receipts with the Go transcript and the signed block corpus."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path

from verify_mainnet_empty import read_varint
from verify_native_sync import check_trace, header
from verify_send_devnet import require


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def block(stream, height):
    size = read_varint(stream)
    payload = stream.read(size)
    require(len(payload) == size and header(payload)[3] == height,
            f"missing block {height} from the raw corpus")
    return payload


def receipts(paths, checkpoint, root):
    height = checkpoint
    for path in paths:
        with Path(path).open("rb") as stream:
            start = json.loads(next(stream))
            require(start["startingHeight"] == height and start["startingAppHash"] == root,
                    f"{path} does not start at the preceding state")
            initial = height
            for line in stream:
                row = json.loads(line)
                if "height" not in row:
                    break
                height += 1
                require(row["height"] == height, f"{path} is missing receipt {height}")
                root = row["appHash"]
                yield row
            else:
                raise RuntimeError(f"{path} is missing the final summary")
            require(row == {"through": height, "appHash": root, "blocks": height - initial}
                    and start["target"] == height and not stream.read(1),
                    f"{path} has an incomplete or inconsistent summary")


def audit(manifest, replay, raw, traces):
    source = json.loads(manifest.read_bytes())
    checkpoint = source["benchmark_checkpoint"]
    start, last = checkpoint["height"], source["measured"]["to"]
    require(start < last, "source manifest contains no blocks after the checkpoint")
    replay_digest = digest(replay)
    require(replay_digest == source["files"]["abci-iavl.jsonl.gz"]["sha256"],
            "Go transcript SHA-256 differs from the source manifest")
    runs = {name: receipts(paths, start, checkpoint["appHash"]) for name, *paths in traces}
    require(len(runs) == len(traces), "trace labels must be unique")
    transactions = updates = 0
    with gzip.open(replay, "rt", encoding="utf-8") as reference, raw.open("rb") as blocks:
        block(blocks, start)
        current = block(blocks, start + 1)
        for height in range(start + 1, last + 1):
            following = block(blocks, height + 1)
            go = json.loads(next(reference))
            require(go["height"] == height, f"Go transcript is missing {height}")
            row = None
            for name, trace in runs.items():
                actual = next(trace, None)
                require(actual is not None, f"{name} is missing receipt {height}")
                if row is None:
                    check_trace([actual], {height: go}, {height: current, height + 1: following})
                    row = actual
                else:
                    require(actual == row, f"{name} differs at {height}")
            transactions += row["transactions"]
            updates += row["validatorUpdates"]
            current = following
        block(blocks, last + 2)
        require(not blocks.read(1) and not reference.read(1), "extra corpus records")
    for name, trace in runs.items():
        require(next(trace, None) is None, f"{name} has extra receipts")
    require(transactions == source["go_trace"]["transactions"] and
            last - start == source["go_trace"]["count"] and
            row["appHash"] == source["last_measured_app_hash"], "unexpected corpus totals or final root")
    return {
        "checkpoint": {"height": start, "appHash": checkpoint["appHash"]},
        "from": start + 1, "through": last, "blocks": last - start, "transactions": transactions,
        "validatorUpdates": updates, "appHash": row["appHash"], "goTranscriptSha256": replay_digest,
        "rawBlocksSha256": digest(raw), "sourceManifestSha256": digest(manifest),
        "traces": {name: [{"file": Path(path).name, "sha256": digest(path)} for path in paths]
                   for name, *paths in traces},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("manifest", "replay", "raw"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--trace", metavar=("LABEL", "LOG"), action="append", nargs="+", required=True)
    args = parser.parse_args()
    if any(len(trace) < 2 for trace in args.trace):
        parser.error("--trace needs a label and at least one log")
    print(json.dumps(audit(args.manifest, args.replay, args.raw, args.trace), indent=2))


if __name__ == "__main__":
    main()
