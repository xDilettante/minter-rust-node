"""Check the historical corpus verifier with deliberately changed inputs."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

from verify_send_devnet import HIDE_WINDOW, require


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("verifier", "corpus", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir()
    names = ["recreation-blocks.pb"] + [
        f"monster-{case}-{kind}.json"
        for case in ("legal", "interstake", "bazercash")
        for kind in ("blocks", "validators", "successor-validators")
    ]
    before = {name: hashlib.sha256((args.corpus / name).read_bytes()).hexdigest() for name in names}
    verifier_hash = hashlib.sha256(args.verifier.read_bytes()).hexdigest()

    def run(name, corpus, success):
        result_path = args.output / (name + ".json")
        command = [str(args.verifier), str(corpus), str(result_path)]
        result = subprocess.run(command, capture_output=True, timeout=25, creationflags=HIDE_WINDOW)
        (args.output / (name + ".stdout.log")).write_bytes(result.stdout)
        (args.output / (name + ".stderr.log")).write_bytes(result.stderr)
        require((result.returncode == 0) == success, f"unexpected verifier status for {name}")
        require(result_path.exists() == success, f"unexpected output publication for {name}")
        if name.endswith("commit-signature"):
            phase = b"successor" if name.startswith("changed-successor") else b"target"
            require(b"panic: " + phase + b" commit verification" in result.stderr
                    and b"wrong signature" in result.stderr,
                    f"did not reach the expected {phase.decode()} commit signature verification")
        return {"case": name, "exitCode": result.returncode, "accepted": success}

    results = [run("unchanged", args.corpus, True)]
    raw = (args.corpus / names[0]).read_bytes()
    mutations = {
        "changed-chain": raw.replace(b"minter-mainnet-5", b"minter-mainnet-6"),
        "changed-commit-signature": None,
        "changed-successor-commit-signature": None,
        "truncated-block": raw[:-1],
        "unfinished-message": raw + b"\x01",
        "duplicate-block": raw + raw,
        "changed-voting-power": None,
        "changed-successor-voting-power": None,
        "changed-api-amount": None,
    }
    api_fields = {
        "changed-api-nonce": "nonce",
        "changed-api-gas-price": "gas_price",
        "changed-api-gas-coin": "gas_coin",
        "changed-api-type": "type_hex",
        "changed-api-height": "height",
        "changed-api-index": "index",
    }
    mutations.update(dict.fromkeys(api_fields))
    omissions = {
        f"{mode}-api-{field}": (mode, field)
        for mode in ("missing", "null")
        for field in ("index", "gas_coin", "code", "mintable", "burnable")
    }
    mutations.update(dict.fromkeys(omissions))
    for name, mutated in mutations.items():
        corpus = args.output / name
        corpus.mkdir()
        for filename in names:
            shutil.copyfile(args.corpus / filename, corpus / filename)
        if mutated is not None:
            require(mutated != raw, f"mutation did not change {name}")
            (corpus / names[0]).write_bytes(mutated)
        elif name.endswith("commit-signature"):
            option = ("--make-invalid-successor-commit" if name.startswith("changed-successor")
                      else "--make-invalid-commit")
            preparation = subprocess.run(
                [str(args.verifier), option, str(corpus)],
                capture_output=True, timeout=25, creationflags=HIDE_WINDOW,
            )
            (args.output / (name + ".prepare.stdout.log")).write_bytes(preparation.stdout)
            (args.output / (name + ".prepare.stderr.log")).write_bytes(preparation.stderr)
            require(preparation.returncode == 0, "could not prepare the changed commit")
        else:
            suffix = ("successor-validators" if name == "changed-successor-voting-power"
                      else "validators" if name == "changed-voting-power" else "blocks")
            # Burnable is false in INTERSTAKE; distinguish a missing false
            # value from LEGAL's true value, which already mismatches None.
            case = "interstake" if name.endswith("api-burnable") else "legal"
            path = corpus / f"monster-{case}-{suffix}.json"
            value = json.loads(path.read_bytes())
            if suffix.endswith("validators"):
                validator = value["validators"][0]
                validator["voting_power"] = str(int(validator["voting_power"]) + 1)
            elif name in omissions:
                mode, field = omissions[name]
                target = value["result"][0]["transactions"][0]
                if field in ("mintable", "burnable"):
                    target = target["data"]
                if mode == "missing":
                    del target[field]
                else:
                    target[field] = None
            elif name in api_fields:
                transaction = value["result"][0]["transactions"][0]
                field = api_fields[name]
                transaction[field] = "0x10" if field == "type_hex" else transaction[field] + 1
            else:
                value["result"][0]["transactions"][0]["data"]["initial_amount"] = "1"
            path.write_text(json.dumps(value) + "\n", encoding="utf-8")
        results.append(run(name, corpus, False))
    require(before == {name: hashlib.sha256((args.corpus / name).read_bytes()).hexdigest()
                       for name in names}, "source corpus changed")
    require(verifier_hash == hashlib.sha256(args.verifier.read_bytes()).hexdigest(),
            "verifier changed during the run")
    report = {"verifierSha256": verifier_hash, "inputSha256": before, "results": results}
    (args.output / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
