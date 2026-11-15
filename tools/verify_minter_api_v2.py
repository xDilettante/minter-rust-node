"""Compare Rust HTTP and gRPC with the Go Minter v2 service in a mixed devnet."""

import argparse
import base64
import copy
from datetime import datetime
import hashlib
import json
from pathlib import Path
import platform
import subprocess
from types import SimpleNamespace
import urllib.error
import urllib.parse
import urllib.request

from verify_send_devnet import HTTP, HIDE_WINDOW, Node, allocate_ports, require, signed_root, start, stop, wait_for


def listen(port):
    return f"127.0.0.1:{port}"


def get(port, path):
    try:
        with HTTP.open(f"http://{listen(port)}{path}", timeout=10) as response:
            return {"status": response.status, "body": json.load(response)}
    except urllib.error.HTTPError as response:
        with response:
            return {"status": response.code, "body": json.load(response)}


def rpc(port, method, **args):
    query = "?" + urllib.parse.urlencode(args) if args else ""
    answer = get(port, f"/{method}{query}")
    require(answer["status"] == 200 and "result" in answer["body"], answer)
    return answer["body"]["result"]


def grpc(oracle, port, cases, path):
    path.write_text(json.dumps(cases) + "\n", encoding="utf-8")
    return json.loads(subprocess.check_output([oracle, "query", listen(port), path], timeout=30,
                                              creationflags=HIDE_WINDOW))


def url(case):
    method = case["Method"]
    if method == "VersionNetwork":
        return "/v2/version_network"
    if method == "PriceCommission":
        height = case.get("Height", 0)
        return "/v2/price_commissions" + (f"?height={height}" if height else "")
    if method == "Transactions":
        return "/v2/transactions?" + urllib.parse.urlencode(
            {"query": case["Query"], "page": case.get("Page", 0), "per_page": case.get("PerPage", 0)})
    route, key = {"Address": ("address", "Address"), "CoinInfo": ("coin_info", "Symbol"),
                  "CoinInfoById": ("coin_info_by_id", "ID"), "Block": ("block", "Height"),
                  "Events": ("events", "Height"),
                  "Halts": ("halts", "Height"),
                  "CommissionVotes": ("commission_votes", "TargetVersion"),
                  "UpdateVotes": ("update_votes", "TargetVersion"),
                  "Transaction": ("transaction", "Hash"), "SendTransaction": ("send_transaction", "Tx")}[method]
    if method == "Block":
        params = {"fields": case.get("Fields", []), "failed_txs": str(case.get("FailedTxs", False)).lower(),
                  "events": str(case.get("Events", False)).lower()}
    elif method == "Events":
        params = {"search": case.get("Search", [])}
    else:
        params = {}
    if case.get("Height") and key != "Height":
        params["height"] = case["Height"]
    if case.get("Delegated"):
        params["delegated"] = "true"
    query = "?" + urllib.parse.urlencode(params, doseq=True) if params else ""
    return f"/v2/{route}/{urllib.parse.quote(str(case[key]), safe='')}{query}"


def canonical(body):
    body = copy.deepcopy(body)
    if "body" in body:
        body["body"] = canonical(body["body"])
    for field in ("delegated", "total"):
        if field in body:
            # Go collects these two arrays from maps.
            body[field].sort(key=lambda item: int(item["coin"]["id"]))
    return body


def verify_queries(args, cases, go_ports, rust_ports, label):
    ref = grpc(args.oracle, go_ports.grpc, cases, args.output / f"queries-{label}.json")
    got = grpc(args.oracle, rust_ports.grpc, cases, args.output / f"queries-{label}.json")
    results = []
    for case, go, rust in zip(cases, ref, got, strict=True):
        require(go["code"] == case["Code"], f"unexpected Go result for {case}: {go}")
        require(canonical(go) == canonical(rust),
                f"gRPC mismatch for {case}\nGo {go}\nRust {rust}")
        record = {"query": case, "goGrpc": go, "rustGrpc": rust}
        if case.get("Http", True):
            path = url(case)
            go_http, rust_http = (get(ports.http, path) for ports in (go_ports, rust_ports))
            require(go_http["status"] == rust_http["status"] and
                    canonical(go_http["body"]) == canonical(rust_http["body"]),
                    f"HTTP mismatch for {path}\nGo {go_http}\nRust {rust_http}")
            if go["code"] == 0:
                require(canonical(go["body"]) == canonical(go_http["body"]),
                        f"Go HTTP and gRPC differ at {path}")
                require(canonical(rust["body"]) == canonical(rust_http["body"]),
                        f"Rust HTTP and gRPC differ at {path}")
            record.update({"path": path, "goHttp": go_http, "rustHttp": rust_http})
        results.append(record)
    return results


def http_request(port, path, method, headers, data=None):
    request = urllib.request.Request(f"http://{listen(port)}{path}", data=data,
                                     headers=headers, method=method)
    try:
        response = HTTP.open(request, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read()
        return {"status": response.code, "body": canonical(json.loads(raw)) if raw else None,
                "cors": {key.lower(): value for key, value in response.headers.items()
                         if key.lower().startswith("access-control-allow-")}}


def verify_status(body, port, version, keep, first):
    status = rpc(port, "status")
    node, validator = status["node_info"], status["validator_info"]
    height = int(body["latest_block_height"])
    block = rpc(port, "block", height=height)
    header = block["block"]["header"]
    want = {
        "version": version, "network": node["network"], "moniker": node["moniker"],
        "initial_height": str(first), "latest_block_height": str(height),
        "latest_block_hash": block["block_id"]["hash"].upper(),
        "latest_app_hash": header["app_hash"].upper(),
        "keep_last_states": str(keep),
        "node_id": node["id"], "public_key": "Mp" + base64.b64decode(validator["pub_key"]["value"]).hex(),
        "catching_up": False,
    }
    require({key: body[key] for key in want} == want, f"Status differs from Tendermint: {body} vs {want}")
    require(datetime.fromisoformat(body["latest_block_time"]) == datetime.fromisoformat(header["time"]),
            f"Status block time differs from the header: {body}")
    require(set(body) == set(want) | {"current_emission", "total_slashed", "latest_block_time"} and
            body["current_emission"].isdigit() and body["total_slashed"].isdigit(),
            f"wrong Status protobuf JSON shape: {body}")


def verify(args):
    args.output.mkdir(parents=True)
    fixture = json.loads((args.fixture / "go-abci-v034.json").read_bytes())
    genesis = json.loads((args.fixture / "genesis.json").read_bytes())
    first = int(genesis["initial_height"])
    accounts = genesis["app_state"]["accounts"]
    sender, staker, creator, multisig = (account["address"] for account in accounts[:4])
    keep = 36
    ports = allocate_ports(9)
    rust_ports = SimpleNamespace(rpc=ports[1], http=ports[3], grpc=ports[4])
    go_ports = SimpleNamespace(rpc=ports[5], p2p=ports[6], grpc=ports[7], http=ports[8])
    rust = Node(args.output / "rust", args.tendermint, args.daemon, args.fixture, ports[:3], False)
    log = args.output / "go.log"
    oracle = start([args.oracle, "serve", args.output / "go", args.fixture, listen(go_ports.rpc),
                    listen(go_ports.p2p), listen(go_ports.grpc), listen(go_ports.http), str(keep)], log)
    rust_flags = ["--api-listen", listen(rust_ports.http), "--grpc-listen", listen(rust_ports.grpc),
                  "--tendermint-rpc", listen(rust_ports.rpc)]

    def wait_height(height):
        wait_for(lambda: min(rust.height(), int(rpc(go_ports.rpc, "status")["sync_info"]["latest_block_height"]))
                 >= height, f"both nodes at height {height}", rust.processes + [(oracle, log)])

    try:
        info = wait_for(lambda: rpc(go_ports.rpc, "status"), "Go Tendermint RPC", [(oracle, log)])
        peer = SimpleNamespace(p2p=go_ports.p2p, node_id=info["node_info"]["id"],
                               rpc=lambda method, **args: rpc(go_ports.rpc, method, **args))
        rust.launch(peer, "initial", use_rust=True, app_args=[*rust_flags, "--keep-iavl-versions", str(keep)])
        wait_height(first + 1)
        start_root = signed_root(rust, peer, first + 1)

        heights = {}
        hashes = {}
        broadcasts = []
        failed_hash = "Mt" + hashlib.sha256(bytes.fromhex(fixture["transactions"]["duplicate-ticker"])).hexdigest()
        for name, transport, ports in (
            ("transfer", "POST", rust_ports),
            ("multisig-batch", "gRPC", go_ports),
            ("create-token", "gRPC", rust_ports),
            ("new-token-send", "GET", rust_ports),
            ("mint-token", "gRPC", go_ports),
            ("create-coin", "POST", rust_ports),
        ):
            tx = fixture["transactions"][name]
            tx_hash = hashlib.sha256(bytes.fromhex(tx)).hexdigest()
            hashes[name] = "Mt" + tx_hash
            request = {"Method": "SendTransaction", "Tx": "0x" + tx}
            if transport == "gRPC":
                requests = [request]
                if name == "create-token":
                    # Both pass CheckTx against the same state. The second ticker
                    # creation must fail when it is executed in the proposal.
                    requests.append({"Method": "SendTransaction", "Tx": "0x" + fixture["transactions"]["duplicate-ticker"]})
                answer, *competing = grpc(args.oracle, ports.grpc, requests, args.output / "broadcast.json")
                require(answer["code"] == 0, answer)
                if competing:
                    require(competing == [{"code": 0, "body": {"code": "0", "log": "", "hash": failed_hash}}],
                            competing)
            elif transport == "POST":
                answer = http_request(ports.http, "/v2/send_transaction", "POST",
                                      {"Content-Type": "application/json"},
                                      json.dumps({"tx": request["Tx"], "ignored": True}).encode())
                require(answer["status"] == 200, answer)
            else:
                answer = get(ports.http, url(request))
                require(answer["status"] == 200, answer)
            require(answer["body"] == {"code": "0", "log": "", "hash": hashes[name]}, answer)
            result = wait_for(lambda: rpc(go_ports.rpc, "tx", hash="0x" + tx_hash),
                              f"{name} indexed on Go", rust.processes + [(oracle, log)])
            require(result["tx_result"].get("code", 0) == 0, f"{name} execution failed: {result}")
            heights[name] = int(result["height"])
            wait_height(heights[name])
            if name == "create-token":
                failed = wait_for(lambda: rpc(go_ports.rpc, "tx", hash="0x" + failed_hash[2:]),
                                  "competing creation indexed on Go", rust.processes + [(oracle, log)])
                require(int(failed["height"]) == heights[name] and failed["tx_result"].get("code", 0) != 0,
                        f"expected the competing ticker creation to fail in the same block: {failed}")
            broadcasts.append({"transaction": name, "transport": transport,
                               "node": "Rust" if ports is rust_ports else "Go", "result": answer["body"],
                               "height": heights[name]})
        changed = heights["create-coin"]
        wait_height(changed + 1)
        final_root = signed_root(rust, peer, changed + 1)

        def case(method, code=0, **fields):
            return {"Method": method, "Code": code, **fields}

        cases = [
            case("Address", Address=sender, Height=first),
            case("Address", Address=sender, Height=heights["transfer"]),
            case("Address", Address=multisig, Height=heights["multisig-batch"]),
            case("Address", Address=staker, Delegated=True, Height=changed),
            case("Address", Address=creator, Delegated=True),
            case("Address", Address="mx01", Height=changed),
            case("Address", 3, Address="invalid"),
            case("CoinInfo", Symbol="BIP"),
            case("CoinInfo", Symbol="ABCT", Height=heights["create-token"]),
            case("CoinInfo", Symbol="ABCT-0", Height=changed),
            case("CoinInfo", Symbol="COINB"),
            case("CoinInfo", 5, Symbol="ABCT", Height=first),
            case("CoinInfo", 5, Symbol="ABCT-1"),
            case("CoinInfo", 5, Symbol="ABCT-65536x"),
            case("CoinInfo", 5, Symbol="\u0000BIP"),
            case("CoinInfo", 5, Symbol="", Http=False),
            case("CoinInfoById", ID=0),
            case("CoinInfoById", ID=1, Height=changed),
            case("CoinInfoById", ID=2),
            case("CoinInfoById", ID=4294967297),
            case("CoinInfoById", 5, ID=999),
            case("CoinInfoById", 5, ID=18446744073709551615),
            case("CoinInfoById", 5, ID=0, Height=999999999),
        ]
        queries = verify_queries(args, cases, go_ports, rust_ports, "initial")
        chain_cases = [
            *(case("Transaction", Hash=hashes[name]) for name in heights),
            case("Transaction", Hash=failed_hash),
            case("Transaction", Hash="0x" + hashes["transfer"][2:].upper()),
            case("Transaction", 3, Hash="Mt"),
            case("Transaction", 3, Hash="Mt1"),
            case("Transaction", 3, Hash="Mtzz"),
            case("Transaction", 9, Hash="Mt" + "00" * 32),
            case("Transaction", 9, Hash="Mt00"),
            case("Block", Height=heights["transfer"]),
            case("Block", Height=heights["multisig-batch"], Fields=[0, 3, 0], FailedTxs=True),
            case("Block", Height=changed, Fields=[2, 4, 5, 6]),
            case("Block", Height=heights["create-token"]),
            case("Block", Height=heights["create-token"], FailedTxs=True),
            case("Block", Height=first, Fields=[0, 2, 3, 6]),
            case("Block", 13, Height=first),
            case("Block", 5, Height=0),
            case("Block", 5, Height=999999999),
            case("Transactions", Query=f"tx.height >= {heights['transfer']}", Page=1, PerPage=2),
            case("Transactions", Query=f"tx.height >= {heights['transfer']}", Page=2, PerPage=2),
            case("Transactions", Query=f"tx.height >= {heights['transfer']}", Page=1),
            case("Transactions", Query=f"tags.tx.from = '{sender[2:]}'", Page=1, PerPage=1000),
            case("Transactions", Query="tx.height = 1", Page=1, PerPage=20),
            case("Transactions", 9, Query=f"tx.height >= {heights['transfer']}", Page=0, PerPage=2),
            case("Transactions", 9, Query=f"tx.height >= {heights['transfer']}", Page=1234, PerPage=2),
            case("Transactions", 9, Query="tx.height not a query", Page=1, PerPage=2),
            case("SendTransaction", 3, Tx="invalid"),
            case("SendTransaction", 3, Tx="0X1234"),
            case("SendTransaction", 3, Tx="0x1"),
            case("SendTransaction", 3, Tx="0x01"),
            case("SendTransaction", 3, Tx="0x" + fixture["transactions"]["wrong-nonce"]),
            case("SendTransaction", 3, Tx="0x" + fixture["transactions"]["wrong-chain"]),
            case("SendTransaction", 3, Tx="0x" + fixture["transactions"]["insufficient"]),
        ]
        chain_queries = verify_queries(args, chain_cases, go_ports, rust_ports, "chain")
        filtered, with_failures = (
            item["rustGrpc"]["body"] for item in chain_queries
            if item["query"]["Method"] == "Block" and item["query"]["Height"] == heights["create-token"]
        )
        require(filtered["transaction_count"] == with_failures["transaction_count"] == "2", filtered)
        require([tx["hash"] for tx in filtered["transactions"]] == [hashes["create-token"]] and
                [tx["hash"] for tx in with_failures["transactions"]] == [hashes["create-token"], failed_hash],
                with_failures)
        search = next(item for item in chain_queries if item["query"].get("PerPage") == 1000)
        require([tx["hash"] for tx in search["goGrpc"]["body"]["transactions"]] == [hashes["transfer"]],
                "sender index did not return the expected transaction")
        duplicate = {"Method": "SendTransaction", "Tx": "0x" + fixture["transactions"]["transfer"]}
        repeated, = grpc(args.oracle, rust_ports.grpc, [duplicate], args.output / "duplicate.json")
        require(repeated == {"code": 6, "message": "tx already exists in cache"}, repeated)
        require(get(rust_ports.http, url(duplicate)) == {
            "status": 409, "body": {"error": {"code": "409", "message": repeated["message"], "data": {}}},
        }, repeated)
        for tx in ("invalid", "0x01", "0x" + fixture["transactions"]["wrong-nonce"]):
            replies = [http_request(port.http, "/v2/send_transaction", "POST",
                                    {"Content-Type": "application/json"}, json.dumps({"tx": tx}).encode())
                       for port in (go_ports, rust_ports)]
            require(replies[0] == replies[1] and replies[0]["status"] == 400, replies)
            broadcasts.append({"transaction": tx, "transport": "POST", "error": replies[1]})

        invalid = []
        for path in (f"/v2/address/{sender}?height=-1", f"/v2/address/{sender}?height=1&height=2",
                     f"/v2/address/{sender}?delegated=invalid", "/v2/coin_info_by_id/not-a-number",
                     "/v2/coin_info/BIP?junk=%zz", "/v2/coin_info/BIP?unknown=1;bad=2",
                     "/v2/coin_info/BIP?height%5B1%5D=2", "/v2/coin_info/BIP?height.child=1",
                     "/v2/block/invalid", f"/v2/block/{first}?fields=99", f"/v2/block/{first}?failedTxs=bad",
                     "/v2/transactions?page=2147483648", "/v2/transactions?perPage=-2147483649"):
            go, current = (get(item.http, path) for item in (go_ports, rust_ports))
            require(go == current and go["status"] == 400, f"invalid HTTP query differs at {path}: {go} vs {current}")
            invalid.append({"path": path, "response": go})
        http_methods = []
        form = {"Content-Type": "application/x-www-form-urlencoded"}
        origin = {"Origin": "http://127.0.0.1:60001"}
        for path, method, headers, data, expected in (
            ("/v2/coin_info/BIP", "HEAD", {}, None, 501),
            ("/v2/coin_info/BIP", "POST", {"Content-Type": "application/json"}, b"{}", 501),
            (f"/v2/address/{staker}", "POST", form, f"height={changed}&delegated=true".encode(), 200),
            (f"/v2/address/{staker}?height={changed}&delegated=true", "POST",
             {**form, "X-HTTP-Method-Override": "GET"}, b"height=999999999", 200),
            ("/v2/coin_info/BIP", "GET", origin, None, 200),
            ("/v2/coin_info/BIP", "OPTIONS", {**origin, "Access-Control-Request-Method": "GET"}, None, 200),
        ):
            go, current = (http_request(item.http, path, method, headers, data)
                           for item in (go_ports, rust_ports))
            require(go == current and go["status"] == expected,
                    f"HTTP method compatibility differs at {method} {path}: {go} vs {current}")
            http_methods.append({"path": path, "method": method, "response": go})

        def aligned_status():
            go, current = (get(item.http, "/v2/status") for item in (go_ports, rust_ports))
            if go["status"] != 200 or current["status"] != 200:
                raise RuntimeError(f"Status not ready: {go} vs {current}")
            go, current = go["body"], current["body"]
            if go["latest_block_height"] != current["latest_block_height"] or current["catching_up"]:
                return None
            same = ("network", "initial_height", "latest_block_height", "latest_block_hash",
                    "latest_app_hash", "latest_block_time", "keep_last_states", "total_slashed", "current_emission")
            require(all(go[key] == current[key] for key in same), f"Status fields differ: {go} vs {current}")
            return go, current

        http_status = wait_for(aligned_status, "matching Go and Rust Status", rust.processes + [(oracle, log)])
        grpc_status = []
        for endpoints, version, body in ((go_ports, "go-oracle", http_status[0]),
                                          (rust_ports, "v0.1.0", http_status[1])):
            verify_status(body, endpoints.rpc, version, keep, first)
            reply, = grpc(args.oracle, endpoints.grpc, [{"Method": "Status"}], args.output / "status.json")
            require(reply["code"] == 0, reply)
            verify_status(reply["body"], endpoints.rpc, version, keep, first)
            grpc_status.append(reply["body"])

        wait_height(first + keep + 1)
        old = f"/v2/address/{sender}?height={first}"
        wait_for(lambda: all(get(item.http, old)["status"] == 404 for item in (go_ports, rust_ports)),
                 "Go and Rust prune the old version", rust.processes + [(oracle, log)])
        restart_at = rust.height()
        rust.close()
        rust.launch(peer, "restarted", use_rust=True, app_args=rust_flags)
        wait_height(restart_at + 1)
        retained = rust.height() - 2
        queries += verify_queries(args, [
            case("Address", 5, Address=sender, Height=first),
            case("CoinInfo", 5, Symbol="BIP", Height=first),
            case("CoinInfoById", 5, ID=0, Height=first),
            case("Address", Address=staker, Height=retained, Delegated=True),
            case("CoinInfoById", ID=2, Height=retained),
        ], go_ports, rust_ports, "restarted")
        chain_queries += verify_queries(args, [
            case("Block", Height=first, Fields=[0, 2, 3, 6]),
            case("Transaction", Hash=hashes["transfer"]),
            case("Transactions", Query=f"tx.height >= {heights['transfer']}", Page=1, PerPage=2),
        ], go_ports, rust_ports, "chain-restarted")
        wait_for(lambda: not rust.rpc("status")["sync_info"]["catching_up"], "Rust exits block sync",
                 rust.processes + [(oracle, log)])
        after = get(rust_ports.http, "/v2/status")["body"]
        verify_status(after, rust_ports.rpc, "v0.1.0", keep, first)
        result = {"os": platform.platform(), "firstSignedRoot": start_root, "afterTransactions": final_root,
                  "transactionHeights": heights, "keep": keep, "prunedHeight": first,
                  "restartAt": restart_at, "retainedHeight": retained, "httpStatus": http_status,
                  "grpcStatus": grpc_status, "statusAfterRestart": after, "queries": queries,
                  "chainQueries": chain_queries, "broadcasts": broadcasts, "duplicate": repeated,
                  "invalidHttp": invalid, "httpMethods": http_methods}
        (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"os": result["os"], "queries": len(queries), "chainQueries": len(chain_queries), "prunedHeight": first,
                          "retainedHeight": retained, "signedRoot": final_root}))
    finally:
        rust.close()
        stop(oracle)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("daemon", "oracle", "tendermint", "fixture", "output"):
        parser.add_argument(name, type=Path)
    verify(parser.parse_args())
