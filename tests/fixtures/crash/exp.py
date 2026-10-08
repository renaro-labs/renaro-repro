"""Fails in run 2 (exit 1)."""
import argparse
import os

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--seed", type=int, required=True)
p.add_argument("--cache-dir", default="")
p.add_argument("--config", default="")
a = p.parse_args()
os.makedirs(a.out, exist_ok=True)

import sys

with open(os.path.join(a.out, "result.txt"), "w", encoding="utf-8", newline="\n") as f:
    f.write("ok\n")
if os.environ.get("REPRO_RUN_ID", "").endswith("-2"):
    print("simulated crash in run 2", file=sys.stderr)
    sys.exit(1)
