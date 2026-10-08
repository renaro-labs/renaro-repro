"""The cached value for a key comes from an unseeded RNG: same key, different value."""
import argparse
import os

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--seed", type=int, required=True)
p.add_argument("--cache-dir", default="")
p.add_argument("--config", default="")
a = p.parse_args()
os.makedirs(a.out, exist_ok=True)

import hashlib
import json
import random

def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()

with open(os.path.join(a.out, "cache.jsonl"), "w", encoding="utf-8", newline="\n") as log, \
        open(os.path.join(a.out, "results.csv"), "w", encoding="utf-8", newline="\n") as out:
    out.write("state,score\n")
    for i in range(5):
        parts = {"state_hash": sha("s%d" % i)[:16], "expert": "qwen2.5-math-7b",
                 "schema_version": "v0", "seed": a.seed}
        key = sha(json.dumps(parts, sort_keys=True))
        log.write(json.dumps({"event": "miss", "key": key, "key_parts": parts,
                              "value_blake3": None}) + "\n")
        value = random.random()  # BUG (on purpose): unseeded
        text = json.dumps({"score": value})
        log.write(json.dumps({"event": "store", "key": key, "key_parts": parts,
                              "value_blake3": sha(text)}) + "\n")
        out.write("%d,%r\n" % (i, value))
