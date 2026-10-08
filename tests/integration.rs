//! End-to-end tests: run the real `repro` binary on the toy experiments in tests/fixtures.
//! Fixture tests need Python 3 on PATH and are skipped with a message when it is missing.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_repro");
static NEXT_TMP_ID: AtomicUsize = AtomicUsize::new(0);

fn python() -> Option<String> {
    for cand in ["python3", "python", "py"] {
        let ok = Command::new(cand)
            .args(["-c", "import sys; print(sys.version_info[0])"])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "3")
            .unwrap_or(false);
        if ok {
            return Some(cand.to_string());
        }
    }
    None
}

macro_rules! need_python {
    () => {
        match python() {
            Some(p) => p,
            None => {
                eprintln!("SKIPPED: Python 3 not found on PATH (tried python3, python, py)");
                return;
            }
        }
    };
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() != "__pycache__" {
                copy_dir(&entry.path(), &to);
            }
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// A fresh copy of a fixture in the target tmp dir, so tests never write into the source tree.
fn fixture(name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let dst = unique_tmp_dir(&format!("fixture-{name}"));
    copy_dir(&src, &dst);
    dst
}

fn scratch(name: &str) -> PathBuf {
    let d = unique_tmp_dir(&format!("scratch-{name}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn unique_tmp_dir(prefix: &str) -> PathBuf {
    let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{prefix}-{id}"))
}

struct Res {
    code: i32,
    report: Value,
    text: String,
}

fn repro(dir: &Path, args: &[&str]) -> Res {
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("PYTHONHASHSEED")
        .output()
        .expect("run repro");
    let report = std::fs::read(dir.join("repro-report").join("report.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    Res {
        code: out.status.code().unwrap_or(-1),
        report,
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// `repro check [flags] -- <python> exp.py ...` in a fresh copy of the fixture.
fn check(name: &str, flags: &[&str]) -> Option<(PathBuf, Res)> {
    let py = python()?;
    let dir = fixture(name);
    let mut args: Vec<&str> = vec!["check"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&[
        "--",
        py.as_str(),
        "exp.py",
        "--out",
        "{run_dir}",
        "--seed",
        "{seed}",
        "--cache-dir",
        "{cache_dir}",
        "--config",
        "{config}",
    ]);
    let res = repro(&dir, &args);
    Some((dir, res))
}

fn str_of<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

fn hint_ids(r: &Value) -> Vec<String> {
    r["hints"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| h["id"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn cache_checks(r: &Value) -> Vec<String> {
    r.pointer("/cache/issues")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| i["check"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn good_fixture_is_identical_after_one_normalization() {
    let _py = need_python!();
    let (dir, r) = check("good", &[]).unwrap();
    assert_eq!(r.code, 0, "{}", r.text);
    let label = str_of(&r.report, "/verdict/label");
    assert!(
        label.starts_with("IDENTICAL (after 1 declared normalization"),
        "{label}"
    );
    assert!(r
        .text
        .contains("VERDICT: IDENTICAL (after 1 declared normalization"));
    assert_eq!(str_of(&r.report, "/config_hash_check/status"), "ok");
    assert_eq!(
        r.report.pointer("/cache/same_sequence"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        r.report.pointer("/cache/runs/0/hits"),
        Some(&Value::from(10))
    );
    assert_eq!(r.report["repro_schema"], Value::from(1));
    // Run directories are removed when identical.
    assert!(!dir.join(".repro-work").exists());
    // All three reports exist; the HTML makes no external requests.
    let html = std::fs::read_to_string(dir.join("repro-report").join("report.html")).unwrap();
    assert!(!html.to_ascii_lowercase().contains("http"));
    assert!(html.contains("<svg"));
    assert!(dir.join("repro-report").join("report.md").is_file());
    let m: Value = serde_json::from_slice(
        &std::fs::read(
            dir.join("repro-report")
                .join("manifests")
                .join("run-1.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        m["run_id"].as_str().unwrap().len(),
        "repro-12345678-1".len()
    );
    assert!(m["files"].as_array().unwrap().len() >= 3);
}

#[test]
fn fixture_paths_are_unique_per_call() {
    let a = fixture("good");
    let b = fixture("good");
    assert_ne!(a, b);
}

#[test]
fn timestamp_leak_is_different_with_hint_and_offset() {
    let _py = need_python!();
    let (dir, r) = check("timestamp_leak", &[]).unwrap();
    assert_eq!(r.code, 1, "{}", r.text);
    assert_eq!(str_of(&r.report, "/verdict/status"), "DIFFERENT");
    assert!(
        hint_ids(&r.report).contains(&"timestamp".to_string()),
        "{}",
        r.text
    );
    let m = &r.report["comparisons"][0]["mismatches"][0];
    assert_eq!(m["path"], "result.txt");
    let off = m["localization"]["byte_offset"].as_u64().unwrap();
    let prefix = "accuracy=0.8123\nwritten_at=".len() as u64;
    assert!(
        (prefix..prefix + 26).contains(&off),
        "offset {off} not inside the timestamp"
    );
    assert_eq!(m["localization"]["line"], 2);
    // Run directories are kept when the verdict is not identical.
    assert!(dir.join(".repro-work").join("run-1").is_dir());
}

#[test]
fn seed_ignored_fails_seed_sensitivity() {
    let _py = need_python!();
    let (_dir, r) = check("seed_ignored", &["--seed-sensitivity"]).unwrap();
    assert_eq!(r.code, 1, "{}", r.text);
    assert_eq!(
        r.report.pointer("/seed_check/ok"),
        Some(&Value::Bool(false))
    );
    let failures = r.report["verdict"]["failures"].to_string();
    assert!(failures.contains("seed has no effect"), "{failures}");
}

#[test]
fn good_fixture_passes_seed_sensitivity() {
    let _py = need_python!();
    let (_dir, r) = check("good", &["--seed-sensitivity"]).unwrap();
    assert_eq!(r.code, 0, "{}", r.text);
    assert_eq!(r.report.pointer("/seed_check/ok"), Some(&Value::Bool(true)));
}

#[test]
fn unordered_records_get_order_hint() {
    let _py = need_python!();
    let (_dir, r) = check("unordered", &[]).unwrap();
    assert_eq!(r.code, 1, "{}", r.text);
    assert!(
        hint_ids(&r.report).contains(&"order".to_string()),
        "{}",
        r.text
    );
    assert_eq!(
        r.report["comparisons"][0]["mismatches"][0]["localization"]["same_records_different_order"],
        Value::Bool(true)
    );
}

#[test]
fn cache_poisoning_is_reported() {
    let _py = need_python!();
    let (_dir, r) = check("cache_poison", &[]).unwrap();
    assert_eq!(r.code, 1, "{}", r.text);
    assert!(
        cache_checks(&r.report).contains(&"poisoning".to_string()),
        "{}",
        r.text
    );
    assert!(r.text.contains("cache poisoning"));
}

#[test]
fn cold_warm_ok_is_all_hits() {
    let _py = need_python!();
    let (_dir, r) = check("cold_warm_ok", &["--cache-mode", "cold-warm"]).unwrap();
    assert_eq!(r.code, 0, "{}", r.text);
    assert_eq!(
        r.report.pointer("/cache/runs/1/misses"),
        Some(&Value::from(0))
    );
    assert_eq!(
        r.report.pointer("/cache/runs/1/hits"),
        Some(&Value::from(10))
    );
}

#[test]
fn cold_warm_bad_misses_are_reported() {
    let _py = need_python!();
    let (_dir, r) = check("cold_warm_bad", &["--cache-mode", "cold-warm"]).unwrap();
    assert_eq!(r.code, 1, "{}", r.text);
    assert_eq!(
        cache_checks(&r.report),
        vec!["mode".to_string()],
        "{}",
        r.text
    );
    // Outputs themselves are still identical.
    assert_eq!(
        r.report.pointer("/outputs/different"),
        Some(&Value::from(0))
    );
}

#[test]
fn crash_in_run_two_exits_3() {
    let _py = need_python!();
    let (_dir, r) = check("crash", &[]).unwrap();
    assert_eq!(r.code, 3, "{}", r.text);
    assert_eq!(str_of(&r.report, "/verdict/status"), "ERROR");
    assert!(r.text.contains("run 2"), "{}", r.text);
    assert_eq!(r.report["runs"].as_array().unwrap().len(), 2);
}

#[test]
fn refuses_foreign_out_dir_without_force() {
    let dir = fixture("good");
    std::fs::create_dir_all(dir.join("repro-report")).unwrap();
    std::fs::write(dir.join("repro-report").join("notes.txt"), "mine").unwrap();
    let r = repro(&dir, &["check", "--", "python", "exp.py"]);
    assert_eq!(r.code, 2, "{}", r.text);
    assert!(r.text.contains("--force"), "{}", r.text);
    assert_eq!(
        std::fs::read_to_string(dir.join("repro-report").join("notes.txt")).unwrap(),
        "mine"
    );
}

#[test]
fn usage_errors_exit_2_and_help_exits_0() {
    let dir = scratch("usage");
    assert_eq!(repro(&dir, &["--help"]).code, 0);
    assert_eq!(repro(&dir, &["--version"]).code, 0);
    assert_eq!(repro(&dir, &["no-such-command"]).code, 2);
    let r = repro(&dir, &["check"]);
    assert_eq!(r.code, 2);
    assert!(r.text.contains("repro init"), "{}", r.text);
}

#[test]
fn init_writes_sample_once() {
    let dir = scratch("init");
    assert_eq!(repro(&dir, &["init"]).code, 0);
    let text = std::fs::read_to_string(dir.join("repro.toml")).unwrap();
    assert!(text.contains("[[normalize]]"));
    assert_eq!(repro(&dir, &["init"]).code, 2);
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn json_out(dir: &Path, args: &[&str]) -> (i32, Value) {
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let v = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out.status.code().unwrap_or(-1), v)
}

#[test]
fn hash_verify_and_diff_directories() {
    let dir = scratch("verify");
    write(&dir.join("data/test/a.jsonl"), "{\"q\":1}\n");
    write(&dir.join("data/test/sub/b.csv"), "x,y\n1,2\n");
    write(&dir.join("data/test/gone.txt"), "bye\n");

    let r = repro(
        &dir,
        &["hash", "data/test", "--out", "docs/test_hashes.json"],
    );
    assert_eq!(r.code, 0, "{}", r.text);
    let m: Value =
        serde_json::from_slice(&std::fs::read(dir.join("docs/test_hashes.json")).unwrap()).unwrap();
    let paths: Vec<&str> = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec![
            "data/test/a.jsonl",
            "data/test/gone.txt",
            "data/test/sub/b.csv"
        ]
    );
    assert_eq!(m["files"][0]["blake3"].as_str().unwrap().len(), 64);

    let (code, v) = json_out(
        &dir,
        &[
            "verify",
            "data/test",
            "--against",
            "docs/test_hashes.json",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["verdict"], "IDENTICAL");

    // Keep a copy, then change, remove and add files.
    copy_dir(&dir.join("data"), &dir.join("data_before"));
    write(&dir.join("data/test/sub/b.csv"), "x,y\n1,3\n");
    std::fs::remove_file(dir.join("data/test/gone.txt")).unwrap();
    write(&dir.join("data/test/new.txt"), "hi\n");

    let (code, v) = json_out(
        &dir,
        &[
            "verify",
            "data/test",
            "--against",
            "docs/test_hashes.json",
            "--json",
        ],
    );
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["changed"], 1);
    assert_eq!(v["only_in_a"], 1, "missing file");
    assert_eq!(v["only_in_b"], 1, "extra file");

    let (code, v) = json_out(&dir, &["diff", "data_before", "data", "--json"]);
    assert_eq!(code, 1, "{v}");
    let changed = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["status"] == "different")
        .unwrap();
    assert_eq!(changed["path"], "test/sub/b.csv");
    assert_eq!(changed["localization"]["line"], 2);
    assert_eq!(changed["localization"]["byte_offset"], 6);

    let (code, _) = json_out(
        &dir,
        &[
            "diff",
            "docs/test_hashes.json",
            "docs/test_hashes.json",
            "--json",
        ],
    );
    assert_eq!(code, 0);
}

#[test]
fn hash_refuses_to_overwrite_non_manifest() {
    let dir = scratch("overwrite");
    write(&dir.join("d/a.txt"), "a");
    write(&dir.join("precious.json"), "{\"keep\":true}");
    let r = repro(&dir, &["hash", "d", "--out", "precious.json"]);
    assert_eq!(r.code, 2, "{}", r.text);
    assert_eq!(
        std::fs::read_to_string(dir.join("precious.json")).unwrap(),
        "{\"keep\":true}"
    );
}

#[test]
fn cache_log_command_detects_poisoning() {
    let dir = scratch("cachelog");
    let ev = |event: &str, value: &str| {
        format!(
            "{{\"event\":\"{event}\",\"key\":\"k1\",\"key_parts\":{{\"state_hash\":\"s\",\"expert\":\"e\",\"schema_version\":\"v0\",\"seed\":1}},\"value_blake3\":{value}}}\n"
        )
    };
    write(
        &dir.join("run1.jsonl"),
        &(ev("miss", "null") + &ev("store", "\"aaa\"")),
    );
    write(
        &dir.join("run2.jsonl"),
        &(ev("miss", "null") + &ev("store", "\"bbb\"")),
    );
    let (code, v) = json_out(&dir, &["cache-log", "run1.jsonl", "run2.jsonl", "--json"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["issues"][0]["check"], "poisoning");
    let (code, v) = json_out(&dir, &["cache-log", "run1.jsonl", "--json"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["runs"][0]["misses"], 1);
}
