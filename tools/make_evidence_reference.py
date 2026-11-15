"""Build a Go ABCI reference by injecting evidence after a verified checkpoint."""

import argparse
import base64
from collections import Counter
import hashlib
import json
from pathlib import Path
from types import SimpleNamespace

from verify_mainnet_empty import fields, varint, wire_fields
from verify_minter_api_v2 import grpc
from verify_minter_events import case, go_events, load_corpus
from verify_send_devnet import allocate_ports, require


def field(tag, value):
    if isinstance(value, int):
        return varint(tag << 3) + varint(value)
    return varint((tag << 3) | 2) + varint(len(value)) + value


def message(items):
    return b"".join(field(tag, value) for tag, value in items)


def base64_text(data):
    return base64.b64encode(data).decode("ascii")


def exchange(request, response):
    return {"request": base64_text(request), "response": base64_text(response)}


def begin_payload(block):
    return base64.b64decode(block["begin_block"]["request"], validate=True)


def evidence_for(block, previous_height, key, mixed):
    begin = fields(begin_payload(block))
    votes = [fields(vote) for tag, vote in wire_fields(begin[3]) if tag == 2]
    address = hashlib.sha256(key).digest()[:20]
    validator = next(vote[1] for vote in votes if fields(vote[1])[1] == address)
    total = sum(fields(vote[1]).get(3, 0) for vote in votes)
    block_time = fields(begin[2])[4]

    def evidence(kind, accused):
        return field(4, message([(1, kind), (2, accused), (3, previous_height), (4, block_time), (5, total)]))

    duplicate = evidence(1, validator)
    if not mixed:
        return duplicate
    unknown = b"\xff" * 20
    require(all(fields(vote[1])[1] != unknown for vote in votes), "test address is an active validator")
    other = next(vote[1] for vote in votes if vote.get(2) == 1 and fields(vote[1])[1] != address)
    return evidence(1, field(1, unknown)) + evidence(2, validator) + duplicate + evidence(1, other)


def generate(args):
    snapshot, manifest, checkpoint, authentic = load_corpus(args.corpus)
    source = json.loads((args.corpus / "corpus.json").read_bytes())
    require(args.validator.startswith("Mp") and len(args.validator) == 66, "expected a Minter public key")
    key = bytes.fromhex(args.validator[2:])
    base = checkpoint["height"]
    require(len(authentic) >= 2, "need two consecutive authenticated requests")
    evidence = evidence_for(authentic[0], base, key, args.mixed)
    abci_port, grpc_port, http_port = allocate_ports(3)
    ports = SimpleNamespace(grpc=grpc_port, http=http_port)
    result = []
    with go_events(args, args.output, snapshot, manifest, abci_port, ports) as app:
        require(app.info() == {"height": base, "appHash": checkpoint["appHash"]}, "wrong Go checkpoint")
        root = bytes.fromhex(checkpoint["appHash"])
        for original in authentic[:2]:
            raw = fields(begin_payload(original))
            require(4 not in raw, "original BeginBlock already contains evidence")
            header = fields(raw[2])
            require(len(header[11]) == 32, "wrong original Header.AppHash")
            header[11] = root
            raw[2] = message(header.items())
            begin = message(raw.items()) + evidence
            end = field(1, original["height"])
            started = app.call(7, begin)
            ended = app.call(10, end)
            commit = app.call(11)
            root = fields(commit)[2]
            result.append({"height": original["height"], "begin_block": exchange(begin, started),
                           "deliver_tx": [], "end_block": exchange(end, ended),
                           "commit_response": base64_text(commit), "app_hash": root.hex()})

        queries = [case(row["height"]) for row in result]
        responses = grpc(args.oracle, ports.grpc, queries, args.output / "queries.json")
    require(len(responses) == len(result) and all(row["code"] == 0 for row in responses), responses)
    events = [row["body"]["events"] for row in responses]
    counts = [dict(Counter(event["type"] for event in batch)) for batch in events]
    require(counts[0].get("minter/SlashEvent", 0) > 0, "the Go case did not slash a stake")
    scenario = "mixed evidence types and repeat within the same block" if args.mixed else "duplicate vote"
    return {"case": f"synthetic {scenario} on an authenticated checkpoint",
            "checkpoint": checkpoint, "sourceFiles": source["files"], "validator": args.validator,
            "goOracle": source["go_oracle_revision"], "blocks": result, "events": events, "eventCounts": counts}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("oracle", type=Path)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("validator")
    parser.add_argument("output", type=Path)
    parser.add_argument("--mixed", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    result = generate(args)
    (args.output / "go-reference.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"eventCounts": result["eventCounts"],
                      "roots": [row["app_hash"] for row in result["blocks"]]}))
