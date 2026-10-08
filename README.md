# repro: reproducibility checker

`repro` runs **your** experiment command several times with the same seed, each in a fresh
directory, and proves that every declared output is **byte-identical**. When it is not, it
shows the first differing byte, line, record or key, and names the likely cause.
One static binary, no runtime, no network, works the same on Windows and Linux.

It checks the Renaro Labs project rules: YAML configs with a recorded **config hash**, one
global **seed** (and seeds that actually matter), Parquet/JSONL outputs, a **cache** keyed by
`state_hash + expert + schema_version + seed` with logged hits and misses, and a **run id**
on every log line. It also locks test sets by hash (W1-RL-3) and proves that a clean checkout
or a fresh machine reproduces key results (W8-MO-1, W10-MO-1).

## Install

Download `repro.exe` (Windows) or `repro` (Linux, static musl) from the latest CI run
(*Actions → ci → Artifacts → `repro-exe-windows-x64` / `repro-exe-linux-x64`*) or from a GitHub release, and put it on `PATH`. On Windows,
double-click `add-to-path.bat` next to `repro.exe` (or run `.\add-to-path.ps1`; `-Remove` undoes it)
to add that folder to your user PATH, then open a new terminal.
Or build it: `cargo install --path .` (add `--features parquet` for Parquet row diffs).

## Quickstart (5 minutes)

```sh
cd tests/fixtures/good          # a tiny seeded toy experiment (Python, stdlib only)
repro check                     # runs [run].command twice, compares everything
```
Output has this shape (illustrative, not a recorded run):
```
repro 0.1.0  good  config <hash>  seed 1234  runs 2  mode cold-cold
run 1   ok     <t> s   <n> files   <m> MB peak
run 2   ok     <t> s   <n> files   <m> MB peak
outputs        <n>/<n> identical (1 after normalization)
config hash    recorded and matches
cache log      run1 <e> ev (<h> hit / <m> miss)   run2 <e> ev (<h> hit / <m> miss)   same sequence
VERDICT: IDENTICAL (after 1 declared normalization: *.log.jsonl drops ts,run_id,wall_ms)
report: repro-report/report.html
```
Then try `repro check --seed-sensitivity` and the broken fixtures next to it
(`timestamp_leak`, `unordered`, `seed_ignored`, `cache_poison`, `cold_warm_bad`, `crash`).
For your project: `repro init` writes a commented `repro.toml`; edit it and run `repro check`.

## Commands

```
repro init                                 # write a commented sample repro.toml
repro check [flags] -- <cmd...>            # run N times and compare (cmd defaults to [run].command)
repro hash <paths...> --out m.json         # manifest of files/dirs (path, size, BLAKE3)
repro verify <paths...> --against m.json   # compare files/dirs to a golden manifest
repro diff <A> <B>                         # two manifests or two directories
repro cache-log <file.jsonl...>            # analyze cache logs on their own (--mode cold-warm)
```
`check` flags: `--runs N` (2), `--seed S`, `--cache-mode cold-cold|cold-warm`,
`--seed-sensitivity`, `--float-tol X` (information only), `--strict-env`, `--keep`,
`--out DIR` (`repro-report/`), `--json`, `--timeout S`, `--work-dir`, `--force`.
`-C DIR` runs any command as if started in `DIR`.

The command gets placeholders `{run_dir} {seed} {run_id} {cache_dir} {config}` and env vars
`REPRO_RUN_DIR REPRO_RUN_ID REPRO_SEED REPRO_CACHE_DIR REPRO_CONFIG_HASH`. It must write
its outputs into `REPRO_RUN_DIR`.

## `repro.toml` reference

See [`repro.toml.example`](repro.toml.example) (also written by `repro init`).

| Key | Meaning |
|---|---|
| `[project] name, config, seed, timeout_s` | name; experiment YAML (hashed canonically: comments, key order and quoting do not matter); global seed; per-run timeout |
| `[run] command, env_allow` | default command; env vars recorded in provenance and the only ones passed with `--strict-env` |
| `[outputs] include, exclude` | globs over paths in the run dir (`*` stays in one directory, `**` crosses them) |
| `[[normalize]] files, drop_json_fields, drop_csv_columns, sort_lines, reason` | declared exceptions; `reason` is required and every use is listed in the verdict |
| `[cache] log, required_key_parts` | cache log path (placeholders allowed) and the key parts every event must carry |
| `[inputs] files` | globs hashed and recorded as provenance (data, lock files, serving configs) |

## Cache-log contract

Your cache layer appends one JSON object per line to `[cache].log`:
```json
{"ts":"2026-10-08T15:01:02Z","run_id":"repro-3fa91c02-1","event":"miss","key":"9c1e...","key_parts":{"state_hash":"ab12...","expert":"qwen2.5-math-7b","schema_version":"v0","seed":1234},"value_blake3":null}
```
`event` is `hit`, `miss` or `store`; on `store` and `hit`, `value_blake3` is the hash of the
cached value. repro checks: the log exists and parses; every event has all required key
parts; the same key never maps to two values (cache poisoning / nondeterminism); `key_parts`
↔ `key` is consistent both ways; no hit without an earlier store; in `cold-cold` every run
starts with misses and has the same `(event, key, value)` sequence; in `cold-warm` run 2+
has 0 misses. Logs are compared semantically, never byte for byte, and are excluded from
the output comparison.

## Verdicts and exit codes

| Verdict | Exit | Meaning |
|---|---|---|
| `IDENTICAL (strict)` | 0 | every output matches byte for byte, no normalization used |
| `IDENTICAL (after N declared normalizations: …)` | 0 | matches only after the listed `[[normalize]]` rules |
| `DIFFERENT` | 1 | an output differs, or a check failed (cache, seed, config hash) |
| `ERROR` | 2 | tool, usage or config error (also: no outputs matched) |
| `ERROR` | 3 | your command failed or timed out in some run |

Reports land in `--out`: `report.json` (stable schema, `repro_schema`), `report.md`
(paste into a PR or Jira) and `report.html` (one offline file, light/dark), plus a manifest
per run in `manifests/`. Run directories stay in `.repro-work/` unless the verdict is identical.

## Locking test sets

```sh
repro hash data/test --out docs/test_hashes.json      # commit this file
repro verify data/test --against docs/test_hashes.json  # exit 0 iff nothing changed
```
`verify` lists changed, missing and extra files. `repro diff dirA dirB` also localizes
the first difference in each changed file. Manifest paths are relative to `--root` (default `.`).

## Using repro in CI

```yaml
- run: ./repro check --runs 2 --seed-sensitivity --json > repro-summary.json
- uses: actions/upload-artifact@v4
  if: always()
  with: { name: repro-report, path: repro-report }
```
The exit code fails the job. For the clean-checkout / fresh-machine proofs, commit a golden
manifest of the key results (`repro hash results/ --out docs/golden.json`) and run
`repro verify results/ --against docs/golden.json` after the experiment in a fresh job.
This repository's own CI (`.github/workflows/ci.yml`) builds both binaries, fails if one is
over 5 MB, and writes the measured size, `--help` startup time and hash speed to the job summary.

## Why is my run not identical?

- **Timestamps, UUIDs, PIDs, hostnames, absolute paths in outputs**: move them to logs or
  stderr, or declare a `[[normalize]]` rule with a reason.
- **Same records, different order**: sort before writing; set `PYTHONHASHSEED=0`; never
  write in set/dict iteration order.
- **Floats differ in the last digits**: thread count or GPU reduction order; set
  `OMP_NUM_THREADS=1`, `torch.use_deterministic_algorithms(True)`,
  `CUBLAS_WORKSPACE_CONFIG=:4096:8`, `num_workers=0`. `--float-tol` shows how close they are.
- **CRLF vs LF**: open files with `newline="\n"`; repro never hides line-ending differences.
- **Cache poisoning**: a cached value depends on something not in its key (often an unseeded RNG).
- **Seed ignored** (`--seed-sensitivity` fails): the seed never reaches `random`/`numpy`/`torch`.

## Design notes

Decisions and simplifications are listed one per line in [`DECISIONS.md`](DECISIONS.md).
Dependencies are only those allowed by the spec (plus the optional `parquet` crate behind the
`parquet` feature); colour output uses plain ANSI codes, so no colour crate is needed.
