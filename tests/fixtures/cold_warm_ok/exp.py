"""Uses --cache-dir: with --cache-mode cold-warm, run 2 is all hits."""
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

CACHE = a.cache_dir

def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()

os.makedirs(CACHE, exist_ok=True)
with open(os.path.join(a.out, "cache.jsonl"), "w", encoding="utf-8", newline="\n") as log,         open(os.path.join(a.out, "results.csv"), "w", encoding="utf-8", newline="\n") as out:
    out.write("state,score\n")
    for i in range(10):
        parts = {"state_hash": sha("s%d" % i)[:16], "expert": "qwen2.5-math-7b",
                 "schema_version": "v0", "seed": a.seed}
        key = sha(json.dumps(parts, sort_keys=True))
        path = os.path.join(CACHE, key + ".json")
        if os.path.exists(path):
            with open(path, encoding="utf-8") as f:
                text = f.read()
            log.write(json.dumps({"event": "hit", "key": key, "key_parts": parts,
                                  "value_blake3": sha(text)}) + "\n")
        else:
            log.write(json.dumps({"event": "miss", "key": key, "key_parts": parts,
                                  "value_blake3": None}) + "\n")
            text = json.dumps({"score": int(key[:8], 16) % 1000 / 1000})
            with open(path, "w", encoding="utf-8", newline="\n") as f:
                f.write(text)
            log.write(json.dumps({"event": "store", "key": key, "key_parts": parts,
                                  "value_blake3": sha(text)}) + "\n")
        out.write("%d,%s\n" % (i, json.loads(text)["score"]))
