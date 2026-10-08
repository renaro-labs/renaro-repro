"""Writes records in set-iteration order, which changes per process (hash randomization)."""
import argparse
import os

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--seed", type=int, required=True)
p.add_argument("--cache-dir", default="")
p.add_argument("--config", default="")
a = p.parse_args()
os.makedirs(a.out, exist_ok=True)

import json

items = {"item-%d" % i for i in range(50)}
with open(os.path.join(a.out, "records.jsonl"), "w", encoding="utf-8", newline="\n") as f:
    for it in items:  # BUG (on purpose): should be sorted(items)
        f.write(json.dumps({"item": it}) + "\n")
