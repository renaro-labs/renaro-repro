"""Measures repro's startup time (--help) and hash speed. Usage: measure.py <path-to-repro>.

Prints Markdown lines (used in the CI step summary). Standard library only.
"""
import os
import subprocess
import sys
import tempfile
import time

binary = os.path.abspath(sys.argv[1])

# Startup: median of 20 runs of --help.
times = []
for _ in range(20):
    t = time.perf_counter()
    subprocess.run([binary, "--help"], stdout=subprocess.DEVNULL, check=True)
    times.append((time.perf_counter() - t) * 1000)
times.sort()
startup = times[len(times) // 2]
print("- startup (`--help`, median of 20): %.1f ms%s" % (startup, "" if startup < 50 else " (over 50 ms!)"))

# Hash speed: 1 GiB in 4 files, hashed twice (the second run reads from the OS page cache).
with tempfile.TemporaryDirectory() as d:
    data = os.path.join(d, "data")
    os.makedirs(data)
    chunk = os.urandom(1 << 20)
    for i in range(4):
        with open(os.path.join(data, "part-%d.bin" % i), "wb") as f:
            for _ in range(256):
                f.write(chunk)
    for attempt in ("cold", "warm"):
        out = subprocess.run([binary, "hash", "data", "--out", "m.json"], cwd=d,
                             capture_output=True, text=True, check=True).stdout.strip()
        print("- hash 1 GiB (%s page cache): %s" % (attempt, out))
