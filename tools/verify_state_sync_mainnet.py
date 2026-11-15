"""Import the authenticated Go mainnet snapshot over ABCI 0.34 and resume."""

import argparse
import hashlib
import json
import platform
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from verify_mainnet_empty import app, check_file, fields, load_inputs, require, varint
from verify_send_devnet import stop, wait_for


CHUNK_BYTES = 10_000_000


def encode(field, value):
    if isinstance(value, int):
        return varint(field << 3) + varint(value)
    return varint((field << 3) | 2) + varint(len(value)) + value


def offer(client, manifest, hashes, app_hash):
    snapshot = b"".join(encode(i, value) for i, value in enumerate((
        manifest["height"], manifest["format"], len(hashes),
        bytes.fromhex(manifest["snapshotSha256"]), b"".join(encode(1, h) for h in hashes),
    ), 1))
    return fields(client.call(13, encode(1, snapshot) + encode(2, app_hash)))[1]


def apply(client, index, chunk, sender=b"fixture"):
    return fields(client.call(15, encode(1, index) + encode(2, chunk) + encode(3, sender)))


def require_empty(client):
    response = fields(client.call(3))
    require(response.get(4, 0) == 0 and not response.get(5), "uncommitted snapshot is visible")


def verify(args):
    corpus = json.loads(args.corpus.read_bytes())
    manifest = json.loads(args.manifest.read_bytes())
    for path, name in ((args.snapshot, "go_checkpoint/snapshot.zlib"),
                       (args.manifest, "go_checkpoint/manifest.json"),
                       (args.trace, "abci-iavl.jsonl.gz"), (args.headers, "blocks.csv.gz")):
        check_file(path, corpus["files"][name])
    records, _ = load_inputs(args.trace, args.headers, manifest, manifest["height"] + 3)
    args.output.mkdir(parents=True)
    db = args.output / "rocksdb"
    args.daemon = args.daemon.resolve(strict=True)
    config = ("--state-sync-checkpoint", args.manifest.resolve(strict=True))
    with args.snapshot.open("rb") as stream:
        chunks = list(iter(lambda: stream.read(CHUNK_BYTES), b""))
    require(len(chunks) > 1, "need a multi-chunk mainnet snapshot")
    hashes = [hashlib.sha256(chunk).digest() for chunk in chunks]
    app_hash = bytes.fromhex(manifest["appHash"])
    expected_head = {"height": manifest["height"], "appHash": manifest["appHash"]}

    with app(args.daemon, db, args.output / "download.log", *config) as (client, process):
        require_empty(client)
        require(offer(client, manifest, hashes, bytes(32)) == 3, "accepted a foreign AppHash")
        require(offer(client, manifest, hashes, app_hash) == 1, "did not accept the manifest")
        require(apply(client, 1, chunks[1])[1] == 5, "accepted an out-of-order chunk")
        require(offer(client, manifest, hashes, app_hash) == 1, "did not restart the download")
        require(apply(client, 0, chunks[0])[1] == 1, "did not stage the first chunk")
        require(apply(client, 0, chunks[0])[1] == 1, "did not accept the idempotent duplicate")
        require_empty(client)
        stop(process)

    with app(args.daemon, db, args.output / "import-cut.log", *config) as (client, process):
        require_empty(client)
        require(apply(client, 1, chunks[1])[1] == 2, "accepted a chunk without an offer")
        require(offer(client, manifest, hashes, app_hash) == 1, "did not restart the offer")
        corrupt = bytes([chunks[0][0] ^ 1]) + chunks[0][1:]
        retry = apply(client, 0, corrupt, b"corrupt")
        require(retry == {1: 3, 2: b"\x00", 3: b"corrupt"}, "did not refetch a corrupt chunk")
        for index, chunk in enumerate(chunks[:-1]):
            require(apply(client, index, chunk)[1] == 1, f"failed to stage chunk {index}")
        require_empty(client)
        with ThreadPoolExecutor(max_workers=1) as pool:
            last = pool.submit(apply, client, len(chunks) - 1, chunks[-1])
            wait_for(lambda: any(path.stat().st_size > 1_000_000 for path in db.glob("*.log")),
                     "first IAVL nodes in the RocksDB WAL", [(process, args.output / "import-cut.log")],
                     timeout=45)
            stop(process)
            try:
                last.result(timeout=15)
            except (OSError, RuntimeError):
                pass
            else:
                raise RuntimeError("the checkpoint was committed before the forced stop")

    with app(args.daemon, db, args.output / "reimport.log", *config, timeout=300) as (client, _):
        require_empty(client)
        require(offer(client, manifest, hashes, app_hash) == 1, "did not accept the retry")
        for index, chunk in enumerate(chunks):
            require(apply(client, index, chunk)[1] == 1, f"failed to restore chunk {index}")
        require(client.info() == expected_head, "restored AppHash differs from the signed checkpoint")

    roots = []
    with app(args.daemon, db, args.output / "reopened.log") as (client, _):
        require(client.info() == expected_head, "checkpoint did not survive reopening RocksDB")
        for record in records:
            client.stage(record)
            roots.append(client.commit(record))
    result = {"os": platform.platform(), "checkpoint": expected_head, "chunks": len(chunks),
              "rejectedWrongAppHash": True, "rejectedOutOfOrderChunk": True, "retriedCorruptChunk": True,
              "restartedDownload": True, "restartedDuringImport": True, "roots": roots}
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "corpus", "snapshot", "manifest", "trace", "headers", "output"):
        parser.add_argument(name, type=Path)
    verify(parser.parse_args())
