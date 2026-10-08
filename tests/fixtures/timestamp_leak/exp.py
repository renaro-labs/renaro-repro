"""Writes the current time into an output: never reproducible."""
import argparse
import os

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--seed", type=int, required=True)
p.add_argument("--cache-dir", default="")
p.add_argument("--config", default="")
a = p.parse_args()
os.makedirs(a.out, exist_ok=True)

from datetime import datetime

with open(os.path.join(a.out, "result.txt"), "w", encoding="utf-8", newline="\n") as f:
    f.write("accuracy=0.8123\n")
    f.write("written_at=%s\n" % datetime.now().isoformat())
