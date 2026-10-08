"""Ignores --seed: always uses 42, so seed+1 gives identical outputs."""
import argparse
import os

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--seed", type=int, required=True)
p.add_argument("--cache-dir", default="")
p.add_argument("--config", default="")
a = p.parse_args()
os.makedirs(a.out, exist_ok=True)

import random

rng = random.Random(42)  # BUG (on purpose): should be random.Random(a.seed)
with open(os.path.join(a.out, "results.csv"), "w", encoding="utf-8", newline="\n") as f:
    f.write("step,score\n")
    for step in range(10):
        f.write("%d,%.6f\n" % (step, rng.random()))
