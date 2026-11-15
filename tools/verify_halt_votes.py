"""Compare Minter halt votes with Go and test recovery through ABCI 0.34."""

import argparse
import base64
import json
from pathlib import Path
import platform
import subprocess

from verify_mainnet_empty import app, fields
from verify_send_devnet import HIDE_WINDOW, require, stop
from verify_update_votes import exchange


def head(block):
    return {key: block[key] for key in ("height", "appHash")}


def check_vote_row(client, height, scheduled, exists):
    key = b"h" + scheduled.to_bytes(8, "little")
    response = client.iavl_key(key, height)
    require(response.get(1, 0) == 0 and response[6] == key and response[9] == height and response[8],
            f"invalid halt vote proof at {height} for {scheduled}")
    require(bool(response.get(7)) == exists, f"unexpected halt vote row at {height} for {scheduled}")


def replay(client, block):
    exchange(client, block["exchanges"], block["height"])
    require(client.info() == head(block), f"Go AppHash differs at {block['height']}")


def verify(daemon, cli, fixture, output):
    data = json.loads(fixture.read_bytes())
    checkpoint = data["pending"]["manifest"]
    output.mkdir()
    manifest, snapshot, db = (output / name for name in ("manifest.json", "snapshot.zlib", "rocksdb"))
    manifest.write_text(json.dumps(checkpoint), encoding="utf-8")
    snapshot.write_bytes(base64.b64decode(data["pending"]["snapshot"], validate=True))
    with (output / "import.log").open("wb") as log:
        subprocess.run([cli, "checkpoint", "import", "--db", db, "--snapshot", snapshot,
                        "--manifest", manifest], stdout=log, stderr=subprocess.STDOUT,
                       creationflags=HIDE_WINDOW, check=True, timeout=120)

    blocks = {block["height"]: block for block in data["blocks"]}
    first = checkpoint["height"]
    expiry = first + 1
    with app(daemon, db, output / "before-commit.log") as (client, process):
        require(client.info() == head(blocks[first]), "Go checkpoint differs")
        exchange(client, blocks[expiry]["exchanges"][:-1], expiry)
        require(client.info() == head(blocks[first]), "EndBlock published a head")
        check_vote_row(client, first, expiry, True)
        stop(process)
        require(process.returncode != 0, "process did not stop before Commit")

    committed = 720
    with app(daemon, db, output / "after-abort.log") as (client, process):
        require(client.info() == head(blocks[first]), "interruption advanced the head")
        check_vote_row(client, first, expiry, True)
        for height in range(expiry, committed + 1):
            replay(client, blocks[height])
            check_vote_row(client, height, height, height == committed)
        stop(process)
        require(process.returncode != 0, "process did not stop after Commit")

    [(tag, blocked)] = fields(base64.b64decode(data["blocked"], validate=True)).items()
    last = max(blocks)
    message = f"application halted by validator votes at height {last + 1}"

    def check_halted(client):
        require(client.info() == head(blocks[last]), "halt changed the last committed head")
        check_vote_row(client, last, last + 1, True)
        try:
            client.call(tag, blocked)
        except RuntimeError as error:
            require(str(error) == message, f"unexpected response to halt quorum: {error}")
        else:
            raise RuntimeError("minterd started a block with a halt quorum")

    with app(daemon, db, output / "after-commit.log") as (client, _):
        require(client.info() == head(blocks[committed]), "committed vote was lost")
        check_vote_row(client, committed, committed, True)
        replay(client, blocks[last])
        check_halted(client)
    with app(daemon, db, output / "after-halt.log") as (client, _):
        check_halted(client)

    result = {
        "platform": platform.platform(), "checkpoint": head(blocks[first]),
        "abcis": sum(len(block["exchanges"]) for height, block in blocks.items() if height > first),
        "cutBeforeCommitAt": expiry, "cutAfterCommitAt": committed,
        "haltHeight": last + 1, "final": head(blocks[last]),
    }
    encoded = json.dumps(result, indent=2) + "\n"
    (output / "result.json").write_text(encoded, encoding="utf-8")
    print(encoded, end="")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("daemon", "cli", "fixture", "output"):
        parser.add_argument(argument, type=Path)
    args = parser.parse_args()
    verify(args.daemon.resolve(), args.cli.resolve(), args.fixture.resolve(), args.output.resolve())


if __name__ == "__main__":
    main()
