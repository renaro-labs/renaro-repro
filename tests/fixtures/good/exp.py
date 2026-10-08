"""Deterministic, seeded toy experiment.

Writes a JSONL log with ts/run_id/wall_ms (normalized by repro.toml), a CSV, a run_meta.json
with the config hash, and a cache log. A toy "expert" cache lives in --cache-dir.
value_blake3 holds a SHA-256 here because the standard library has no BLAKE3; repro only
compares these strings for equality.
"""
import argparse
import csv
import hashlib
import json
import os
import random
import time
from datetime import datetime, timezone

p = argparse.ArgumentParser()
p.add_argument("--out", required=True)
p.add_argument("--seed", type=int, required=True)
p.add_argument("--cache-dir", required=True)
p.add_argument("--config", default="")
a = p.parse_args()

run_id = os.environ.get("REPRO_RUN_ID", "")
rng = random.Random(a.seed)
os.makedirs(a.out, exist_ok=True)
os.makedirs(a.cache_dir, exist_ok=True)

lr = 0.0
if a.config:
    with open(a.config, encoding="utf-8") as f:
        for line in f:
            if line.strip().startswith("lr:"):
                lr = float(line.split(":", 1)[1].split("#")[0])


def now():
    return datetime.now(timezone.utc).isoformat()


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


cache_log = open(os.path.join(a.out, "cache.jsonl"), "w", encoding="utf-8", newline="\n")


def log_event(event, key, parts, value_hash):
    rec = {"ts": now(), "run_id": run_id, "event": event, "key": key,
           "key_parts": parts, "value_blake3": value_hash}
    cache_log.write(json.dumps(rec, sort_keys=True) + "\n")


def consult(state, expert):
    parts = {"state_hash": sha(state)[:16], "expert": expert,
             "schema_version": "v0", "seed": a.seed}
    key = sha(json.dumps(parts, sort_keys=True))
    path = os.path.join(a.cache_dir, key + ".json")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            text = f.read()
        log_event("hit", key, parts, sha(text))
        return json.loads(text)
    log_event("miss", key, parts, None)
    value = round(random.Random(key).random(), 6)  # a pure function of the key
    text = json.dumps({"score": value})
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    log_event("store", key, parts, sha(text))
    return {"score": value}


experts = ["qwen2.5-math-7b", "llama-3-8b"]
rows = []
with open(os.path.join(a.out, "train.log.jsonl"), "w", encoding="utf-8", newline="\n") as log:
    for step in range(20):
        start = time.perf_counter()
        expert = experts[step % 2]
        score = consult("state-%d" % (step % 10), expert)["score"]
        loss = round(1.0 / (step + 1) + rng.random() * 0.01 + lr, 6)
        rows.append((step, expert, score, loss))
        wall_ms = round((time.perf_counter() - start) * 1000, 3)
        log.write(json.dumps({"ts": now(), "run_id": run_id, "wall_ms": wall_ms,
                              "step": step, "loss": loss}) + "\n")

with open(os.path.join(a.out, "metrics.csv"), "w", encoding="utf-8", newline="") as f:
    w = csv.writer(f, lineterminator="\n")
    w.writerow(["step", "expert", "score", "loss"])
    w.writerows(rows)

with open(os.path.join(a.out, "run_meta.json"), "w", encoding="utf-8", newline="\n") as f:
    json.dump({"config_hash": os.environ.get("REPRO_CONFIG_HASH", ""), "seed": a.seed,
               "experts": experts}, f, sort_keys=True)
    f.write("\n")

cache_log.close()
print("done: %d steps" % len(rows))
