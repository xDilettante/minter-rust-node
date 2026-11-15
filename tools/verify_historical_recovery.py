"""Verify before/after Commit recovery at selected real continuation blocks."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from verify_send_devnet import HIDE_WINDOW, require


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("tools", "daemon", "cli", "reference", "output"):
        parser.add_argument(name, type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    args.output.mkdir()
    scenarios = [(10523676,"swap-rejected",303),(10524240,"reward-payout",None),
                 (10529166,"nonce-rejected",101),(10543520,"range-end",None)]
    results = []
    for height, name, expected_code in scenarios:
        output = args.output/name
        command = [sys.executable,"-B",str(args.tools/"verify_early_checkpoint.py"),
                   str(args.tools),str(args.daemon),str(args.cli),str(args.reference),
                   str(args.reference/str(height-1)),str(output),"--through",str(height),
                   "--vote-target","10523500"]
        with (args.output/(name+".log")).open("xb") as log:
            subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True,
                           timeout=300,creationflags=HIDE_WINDOW)
        result = json.loads((output/"result.json").read_bytes())
        require(result["final"]["height"]==height and len(result["roots"])==1,"wrong recovery range")
        if expected_code is not None:
            require(any(expected_code in row["codes"] for row in result["transactionCodes"]),
                    f"expected real failure absent at {height}")
        history = json.loads(subprocess.check_output([args.cli,"iavl","history","--db",output/"state"],
                             timeout=30,creationflags=HIDE_WINDOW))
        require(history["head"]==result["final"] and history["keepIavlVersions"]==2
                and history["earliestHeight"]==height-1,"wrong retained roots")
        (output/"retention.json").write_text(json.dumps(history,indent=2)+"\n",encoding="utf-8")
        results.append({"case":name,"height":height,"final":result["final"],"forcedStops":2,
                        "transactionCodes":result["transactionCodes"],"retention":history})
        # Only this completed, reproducible test DB is removed; retain the public
        # inputs, responses, API readings and independently checked final roots.
        state = (output/"state").resolve()
        require(not (output/"state").is_symlink() and state.parent==output.resolve()
                and state.is_relative_to(args.output) and state.name=="state","unexpected cleanup target")
        shutil.rmtree(state)
    report = {"scenarios":results,"forcedStops":sum(row["forcedStops"] for row in results)}
    (args.output/"result.json").write_text(json.dumps(report,indent=2)+"\n",encoding="utf-8")
    print(json.dumps(report),flush=True)


if __name__ == "__main__":
    main()
