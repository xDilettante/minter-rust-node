"""Copy a verified Minter checkpoint and the ABCI inputs for a bounded range."""

import argparse
import csv
import gzip
import hashlib
import io
import json
from pathlib import Path
import shutil

from verify_mainnet_empty import load_inputs
from verify_mainnet_iavl import check_file, require


def file_info(path):
    with path.open("rb") as stream:
        return {"bytes": path.stat().st_size, "sha256": hashlib.file_digest(stream, "sha256").hexdigest()}


def write_gzip(path, write):
    with path.open("wb") as target:
        with gzip.GzipFile(filename="", fileobj=target, mode="wb", mtime=0) as compressed:
            with io.TextIOWrapper(compressed, encoding="utf-8", newline="") as output:
                write(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("corpus", "snapshot", "checkpoint", "trace", "headers", "output"):
        parser.add_argument(name, type=Path)
    parser.add_argument("--through", type=int, required=True)
    parser.add_argument("--go-reference", type=Path,
                        help="reference.json from make_mainnet_checkpoint.go for a derived snapshot")
    args = parser.parse_args()
    corpus = json.loads(args.corpus.read_bytes())
    checkpoint = json.loads(args.checkpoint.read_bytes())
    base = checkpoint["height"]
    require(args.through > base, "target must follow the checkpoint")
    inputs = {
        "go_checkpoint/snapshot.zlib": (args.snapshot, "snapshot.zlib"),
        "go_checkpoint/manifest.json": (args.checkpoint, "checkpoint.json"),
        "abci-iavl.jsonl.gz": (args.trace, "abci-iavl.jsonl.gz"),
        "blocks.csv.gz": (args.headers, "blocks.csv.gz"),
    }
    reference = None
    if args.go_reference:
        reference = json.loads(args.go_reference.read_bytes())
        derived = reference["checkpoint"]
        require(reference["goOracleRevision"] == corpus["go_oracle_revision"]
                and checkpoint["network"] == corpus["chain_id"]
                and all(checkpoint[key] == derived[key] for key in (
                    "height", "network", "blockHash", "appHash", "snapshotSha256", "bytes", "nodes",
                ))
                and reference["targetHeight"] == base + 1, "invalid Go reference")
        check_file(args.snapshot, {"bytes": checkpoint["bytes"], "sha256": checkpoint["snapshotSha256"]})
    for name, (source, _) in inputs.items():
        if not reference or not name.startswith("go_checkpoint/"):
            check_file(source, corpus["files"][name])
    records, headers = load_inputs(args.trace, args.headers, checkpoint, args.through,
                                   skip_prefix=reference is not None)
    if reference:
        require(records[0]["app_hash"] == reference["targetAppHash"],
                "Go reference differs from the authenticated target block")

    def write_trace(output):
        for record in records:
            output.write(json.dumps(record, separators=(",", ":")) + "\n")

    def write_headers(output):
        writer = csv.DictWriter(output, fieldnames=list(headers[base]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(headers[height] for height in range(base, args.through + 2))

    out = args.output
    out.mkdir(parents=True)
    shutil.copyfile(args.snapshot, out / "snapshot.zlib")
    shutil.copyfile(args.checkpoint, out / "checkpoint.json")
    write_gzip(out / "abci-iavl.jsonl.gz", write_trace)
    write_gzip(out / "blocks.csv.gz", write_headers)
    sliced = {
        "format": "minter-corpus-subset-v1",
        "chain_id": corpus["chain_id"],
        "go_oracle_revision": corpus["go_oracle_revision"],
        "checkpoint": base,
        "through": args.through,
        "parent_manifest": file_info(args.corpus),
        "parent_files": {name: corpus["files"][name] for name in inputs},
        "files": {name: file_info(out / target) for name, (_, target) in inputs.items()},
    }
    if args.go_reference:
        shutil.copyfile(args.go_reference, out / "reference.json")
        sliced["go_reference"] = file_info(out / "reference.json")
    (out / "corpus.json").write_text(json.dumps(sliced, indent=2) + "\n", encoding="utf-8")
    print(f"Verified {len(records)} blocks through {args.through}, AppHash {records[-1]['app_hash']}")


if __name__ == "__main__":
    main()
