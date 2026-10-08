//! Cache log (JSONL) loading and the cache checks: logging, key completeness, poisoning,
//! key derivation, impossible hits and cold/warm mode expectations.

use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMode {
    /// Every run starts with an empty cache.
    ColdCold,
    /// Run 2 onwards reuse run 1's cache and must be all hits.
    ColdWarm,
}

impl CacheMode {
    pub fn name(self) -> &'static str {
        match self {
            CacheMode::ColdCold => "cold-cold",
            CacheMode::ColdWarm => "cold-warm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Hit,
    Miss,
    Store,
}

impl EventKind {
    fn name(self) -> &'static str {
        match self {
            EventKind::Hit => "hit",
            EventKind::Miss => "miss",
            EventKind::Store => "store",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheEvent {
    pub line: usize,
    pub kind: EventKind,
    pub key: String,
    pub key_parts: Option<Map<String, Value>>,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CacheLog {
    pub label: String,
    pub path: String,
    pub exists: bool,
    pub events: Vec<CacheEvent>,
    pub parse_errors: Vec<String>,
    pub bad_lines: usize,
}

/// Reads a cache log. Never fails: problems are recorded for the "logged at all" check.
pub fn load(label: &str, path: &Path) -> CacheLog {
    let mut log = CacheLog {
        label: label.to_string(),
        path: path.display().to_string(),
        exists: path.is_file(),
        events: Vec::new(),
        parse_errors: Vec::new(),
        bad_lines: 0,
    };
    let text = match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return log,
    };
    parse_into(&text, &mut log);
    log
}

pub fn parse_into(text: &str, log: &mut CacheLog) {
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        if raw.trim().is_empty() {
            continue;
        }
        match parse_event(raw, line) {
            Ok(ev) => log.events.push(ev),
            Err(msg) => {
                log.bad_lines += 1;
                if log.parse_errors.len() < 20 {
                    log.parse_errors.push(format!("line {line}: {msg}"));
                }
            }
        }
    }
}

fn parse_event(raw: &str, line: usize) -> Result<CacheEvent, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("not JSON ({e})"))?;
    let Value::Object(obj) = v else {
        return Err("not a JSON object".to_string());
    };
    let kind = match obj.get("event").and_then(Value::as_str) {
        Some("hit") => EventKind::Hit,
        Some("miss") => EventKind::Miss,
        Some("store") => EventKind::Store,
        Some(other) => {
            return Err(format!(
                "unknown event {other:?} (expected hit, miss or store)"
            ))
        }
        None => return Err("missing \"event\"".to_string()),
    };
    let key = match obj.get("key") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Err("missing or empty \"key\"".to_string()),
    };
    let key_parts = match obj.get("key_parts") {
        Some(Value::Object(m)) => Some(m.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err("\"key_parts\" is not an object".to_string()),
    };
    let value = match obj.get("value_blake3") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err("\"value_blake3\" is not a string or null".to_string()),
    };
    Ok(CacheEvent {
        line,
        kind,
        key,
        key_parts,
        value,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExpertCount {
    pub expert: String,
    pub misses: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheRunSummary {
    pub label: String,
    pub path: String,
    pub events: u64,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub hit_rate: f64,
    pub top_missed_experts: Vec<ExpertCount>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CacheIssue {
    /// logged | key_completeness | poisoning | key_derivation | impossible_hit | mode
    pub check: String,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheReport {
    pub mode: String,
    pub required_key_parts: Vec<String>,
    pub runs: Vec<CacheRunSummary>,
    pub issues: Vec<CacheIssue>,
    /// Cold-cold only: all runs have the same `(event, key, value_blake3)` sequence.
    pub same_sequence: Option<bool>,
}

impl CacheReport {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

const MAX_DETAILS: usize = 10;

fn issue(check: &str, message: String, details: Vec<String>) -> CacheIssue {
    CacheIssue {
        check: check.to_string(),
        message,
        details,
    }
}

fn short(s: &str) -> &str {
    crate::util::short(s)
}

/// Runs every cache check over the logs (in run order).
pub fn analyze(logs: &[CacheLog], required: &[String], mode: Option<CacheMode>) -> CacheReport {
    let mut issues = Vec::new();

    // 1. Logged at all.
    for log in logs {
        if !log.exists {
            issues.push(issue(
                "logged",
                format!(
                    "{}: cache log {} not found; the cache layer must log every hit/miss/store",
                    log.label, log.path
                ),
                vec![],
            ));
        } else if log.events.is_empty() && log.bad_lines == 0 {
            issues.push(issue(
                "logged",
                format!("{}: cache log {} is empty", log.label, log.path),
                vec![],
            ));
        }
        if log.bad_lines > 0 {
            issues.push(issue(
                "logged",
                format!(
                    "{}: {} line(s) of {} do not parse",
                    log.label, log.bad_lines, log.path
                ),
                log.parse_errors.iter().take(MAX_DETAILS).cloned().collect(),
            ));
        }
    }

    // 2. Key completeness.
    for log in logs {
        let mut missing: BTreeSet<String> = BTreeSet::new();
        let mut count = 0u64;
        let mut examples = Vec::new();
        for ev in &log.events {
            let absent: Vec<&String> = required
                .iter()
                .filter(|p| ev.key_parts.as_ref().is_none_or(|m| !m.contains_key(*p)))
                .collect();
            if !absent.is_empty() {
                count += 1;
                missing.extend(absent.iter().map(|s| s.to_string()));
                if examples.len() < 5 {
                    examples.push(format!(
                        "line {}: key {} lacks {}",
                        ev.line,
                        short(&ev.key),
                        absent
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        if count > 0 {
            issues.push(issue(
                "key_completeness",
                format!(
                    "{}: {count} event(s) miss required key parts: {}",
                    log.label,
                    missing.into_iter().collect::<Vec<_>>().join(", ")
                ),
                examples,
            ));
        }
    }

    // 3. Cache poisoning / nondeterminism: same key, different value.
    let mut first_value: BTreeMap<&str, (&str, &str, usize)> = BTreeMap::new();
    let mut poisoned: BTreeMap<&str, String> = BTreeMap::new();
    for log in logs {
        for ev in &log.events {
            let Some(v) = ev.value.as_deref() else {
                continue;
            };
            match first_value.get(ev.key.as_str()) {
                None => {
                    first_value.insert(ev.key.as_str(), (v, log.label.as_str(), ev.line));
                }
                Some(&(v0, label0, line0)) if v0 != v => {
                    poisoned.entry(ev.key.as_str()).or_insert_with(|| {
                        format!(
                            "key {}: {} line {} value {} vs {} line {} value {}",
                            short(&ev.key),
                            label0,
                            line0,
                            short(v0),
                            log.label,
                            ev.line,
                            short(v)
                        )
                    });
                }
                Some(_) => {}
            }
        }
    }
    if !poisoned.is_empty() {
        issues.push(issue(
            "poisoning",
            format!(
                "cache poisoning / nondeterminism: {} key(s) map to different cached values",
                poisoned.len()
            ),
            poisoned.into_values().take(MAX_DETAILS).collect(),
        ));
    }

    // 4. Key derivation: parts -> key and key -> parts must both be functions.
    let mut key_of_parts: BTreeMap<String, &str> = BTreeMap::new();
    let mut parts_of_key: BTreeMap<&str, String> = BTreeMap::new();
    let mut derivation: BTreeSet<String> = BTreeSet::new();
    for log in logs {
        for ev in &log.events {
            let Some(parts) = &ev.key_parts else {
                continue;
            };
            let canon = Value::Object(parts.clone()).to_string();
            match key_of_parts.get(&canon) {
                Some(k) if *k != ev.key => {
                    derivation.insert(format!(
                        "same key_parts {} give keys {} and {}",
                        crate::util::truncate(&canon, 120),
                        short(k),
                        short(&ev.key)
                    ));
                }
                Some(_) => {}
                None => {
                    key_of_parts.insert(canon.clone(), ev.key.as_str());
                }
            }
            match parts_of_key.get(ev.key.as_str()) {
                Some(p) if *p != canon => {
                    derivation.insert(format!(
                        "key {} has different key_parts: {} vs {}",
                        short(&ev.key),
                        crate::util::truncate(p, 120),
                        crate::util::truncate(&canon, 120)
                    ));
                }
                Some(_) => {}
                None => {
                    parts_of_key.insert(ev.key.as_str(), canon);
                }
            }
        }
    }
    if !derivation.is_empty() {
        issues.push(issue(
            "key_derivation",
            format!(
                "key derivation bug: {} inconsistent key/key_parts pair(s)",
                derivation.len()
            ),
            derivation.into_iter().take(MAX_DETAILS).collect(),
        ));
    }

    // 5. Impossible hits: a hit needs an earlier store in this run or in the loaded cache.
    let mut known: BTreeSet<&str> = BTreeSet::new();
    for log in logs {
        if mode != Some(CacheMode::ColdWarm) {
            known.clear();
        }
        let mut bad = Vec::new();
        let mut count = 0u64;
        for ev in &log.events {
            match ev.kind {
                EventKind::Store => {
                    known.insert(ev.key.as_str());
                }
                EventKind::Hit if !known.contains(ev.key.as_str()) => {
                    count += 1;
                    if bad.len() < MAX_DETAILS {
                        bad.push(format!("line {}: hit for key {}", ev.line, short(&ev.key)));
                    }
                }
                _ => {}
            }
        }
        if count > 0 {
            issues.push(issue(
                "impossible_hit",
                format!(
                    "{}: {count} hit(s) for keys never stored in this run or the loaded cache",
                    log.label
                ),
                bad,
            ));
        }
    }

    // 6. Mode expectations.
    let mut same_sequence = None;
    match mode {
        Some(CacheMode::ColdCold) => {
            for log in logs {
                let mut looked_up: BTreeSet<&str> = BTreeSet::new();
                for ev in &log.events {
                    if ev.kind == EventKind::Store {
                        continue;
                    }
                    if looked_up.insert(ev.key.as_str()) && ev.kind == EventKind::Hit {
                        issues.push(issue(
                            "mode",
                            format!(
                                "{}: first lookup of key {} is a hit (line {}); cold-cold runs must start with an empty cache",
                                log.label,
                                short(&ev.key),
                                ev.line
                            ),
                            vec![],
                        ));
                        break;
                    }
                }
            }
            if let Some(first) = logs.first() {
                let seq0 = sequence(first);
                let mut all_same = true;
                for log in &logs[1..] {
                    let seq = sequence(log);
                    if let Some(i) = first_mismatch(&seq0, &seq) {
                        all_same = false;
                        let show = |s: &[(EventKind, &str, Option<&str>)]| {
                            s.get(i)
                                .map(|(k, key, _)| format!("{} {}", k.name(), short(key)))
                                .unwrap_or_else(|| "end of log".to_string())
                        };
                        issues.push(issue(
                            "mode",
                            format!(
                                "{}: cache event sequence differs from {} at event #{}: {} vs {}",
                                log.label,
                                first.label,
                                i + 1,
                                show(&seq0),
                                show(&seq)
                            ),
                            vec![],
                        ));
                    }
                }
                same_sequence = Some(all_same);
            }
        }
        Some(CacheMode::ColdWarm) => {
            for log in logs.iter().skip(1) {
                let misses = log
                    .events
                    .iter()
                    .filter(|e| e.kind == EventKind::Miss)
                    .count();
                if misses > 0 {
                    issues.push(issue(
                        "mode",
                        format!(
                            "{}: {misses} miss(es); cold-warm expects 0 (every lookup should hit run 1's cache)",
                            log.label
                        ),
                        vec![],
                    ));
                }
            }
        }
        None => {}
    }

    CacheReport {
        mode: mode.map_or("independent", |m| m.name()).to_string(),
        required_key_parts: required.to_vec(),
        runs: logs.iter().map(summarize).collect(),
        issues,
        same_sequence,
    }
}

fn sequence(log: &CacheLog) -> Vec<(EventKind, &str, Option<&str>)> {
    log.events
        .iter()
        .map(|e| (e.kind, e.key.as_str(), e.value.as_deref()))
        .collect()
}

fn first_mismatch<T: PartialEq>(a: &[T], b: &[T]) -> Option<usize> {
    let n = a.len().min(b.len());
    (0..n)
        .find(|&i| a[i] != b[i])
        .or((a.len() != b.len()).then_some(n))
}

fn summarize(log: &CacheLog) -> CacheRunSummary {
    let count = |k: EventKind| log.events.iter().filter(|e| e.kind == k).count() as u64;
    let (hits, misses, stores) = (
        count(EventKind::Hit),
        count(EventKind::Miss),
        count(EventKind::Store),
    );
    let mut by_expert: BTreeMap<String, u64> = BTreeMap::new();
    for e in log.events.iter().filter(|e| e.kind == EventKind::Miss) {
        let expert = e
            .key_parts
            .as_ref()
            .and_then(|m| m.get("expert"))
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "(none)".to_string());
        *by_expert.entry(expert).or_default() += 1;
    }
    let mut top: Vec<ExpertCount> = by_expert
        .into_iter()
        .map(|(expert, misses)| ExpertCount { expert, misses })
        .collect();
    // Most misses first; ties by name so output is deterministic.
    top.sort_by(|a, b| b.misses.cmp(&a.misses).then(a.expert.cmp(&b.expert)));
    top.truncate(5);
    let lookups = hits + misses;
    CacheRunSummary {
        label: log.label.clone(),
        path: log.path.clone(),
        events: log.events.len() as u64,
        hits,
        misses,
        stores,
        hit_rate: if lookups > 0 {
            hits as f64 / lookups as f64
        } else {
            0.0
        },
        top_missed_experts: top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(label: &str, lines: &[String]) -> CacheLog {
        let mut l = CacheLog {
            label: label.into(),
            path: format!("{label}.jsonl"),
            exists: true,
            events: Vec::new(),
            parse_errors: Vec::new(),
            bad_lines: 0,
        };
        parse_into(&lines.join("\n"), &mut l);
        l
    }

    fn ev(event: &str, key: &str, state: &str, value: Option<&str>) -> String {
        let v = value.map_or("null".to_string(), |v| format!("\"{v}\""));
        format!(
            "{{\"ts\":\"t\",\"run_id\":\"r\",\"event\":\"{event}\",\"key\":\"{key}\",\"key_parts\":{{\"state_hash\":\"{state}\",\"expert\":\"qwen\",\"schema_version\":\"v0\",\"seed\":1}},\"value_blake3\":{v}}}"
        )
    }

    fn req() -> Vec<String> {
        crate::config::default_key_parts()
    }

    fn checks(r: &CacheReport) -> Vec<&str> {
        r.issues.iter().map(|i| i.check.as_str()).collect()
    }

    #[test]
    fn clean_cold_cold_logs_pass() {
        let lines = vec![
            ev("miss", "k1", "s1", None),
            ev("store", "k1", "s1", Some("v1")),
            ev("hit", "k1", "s1", Some("v1")),
        ];
        let r = analyze(
            &[log("run 1", &lines), log("run 2", &lines)],
            &req(),
            Some(CacheMode::ColdCold),
        );
        assert!(r.ok(), "{:?}", r.issues);
        assert_eq!(r.same_sequence, Some(true));
        assert_eq!(r.runs[0].hits, 1);
        assert_eq!(r.runs[0].misses, 1);
        assert_eq!(r.runs[0].top_missed_experts[0].expert, "qwen");
    }

    #[test]
    fn detects_poisoning_across_runs() {
        let a = vec![
            ev("miss", "k1", "s1", None),
            ev("store", "k1", "s1", Some("v1")),
        ];
        let b = vec![
            ev("miss", "k1", "s1", None),
            ev("store", "k1", "s1", Some("v2")),
        ];
        let r = analyze(&[log("run 1", &a), log("run 2", &b)], &req(), None);
        assert_eq!(checks(&r), vec!["poisoning"]);
        assert!(r.issues[0].details[0].contains("run 1"));
        assert!(r.issues[0].details[0].contains("run 2"));
    }

    #[test]
    fn detects_key_derivation_bug() {
        let a = vec![
            ev("miss", "k1", "s1", None),
            ev("miss", "k2", "s1", None),
            ev("miss", "k3", "s3", None),
            ev("miss", "k3", "s4", None),
        ];
        let r = analyze(&[log("run 1", &a)], &req(), None);
        assert_eq!(checks(&r), vec!["key_derivation"]);
        assert_eq!(r.issues[0].details.len(), 2);
    }

    #[test]
    fn detects_impossible_hit_and_incomplete_keys() {
        let a = vec![
            ev("hit", "k1", "s1", Some("v1")),
            r#"{"event":"miss","key":"k2","key_parts":{"state_hash":"x"},"value_blake3":null}"#
                .to_string(),
        ];
        let r = analyze(&[log("run 1", &a)], &req(), None);
        assert_eq!(checks(&r), vec!["key_completeness", "impossible_hit"]);
    }

    #[test]
    fn cold_warm_requires_all_hits() {
        let a = vec![
            ev("miss", "k1", "s1", None),
            ev("store", "k1", "s1", Some("v1")),
        ];
        let good = vec![ev("hit", "k1", "s1", Some("v1"))];
        let bad = vec![
            ev("miss", "k1", "s1", None),
            ev("store", "k1", "s1", Some("v1")),
        ];
        let ok = analyze(
            &[log("run 1", &a), log("run 2", &good)],
            &req(),
            Some(CacheMode::ColdWarm),
        );
        assert!(ok.ok(), "{:?}", ok.issues);
        let r = analyze(
            &[log("run 1", &a), log("run 2", &bad)],
            &req(),
            Some(CacheMode::ColdWarm),
        );
        assert_eq!(checks(&r), vec!["mode"]);
    }

    #[test]
    fn cold_cold_sequence_must_match() {
        let a = vec![ev("miss", "k1", "s1", None), ev("miss", "k2", "s2", None)];
        let b = vec![ev("miss", "k2", "s2", None), ev("miss", "k1", "s1", None)];
        let r = analyze(
            &[log("run 1", &a), log("run 2", &b)],
            &req(),
            Some(CacheMode::ColdCold),
        );
        assert_eq!(checks(&r), vec!["mode"]);
        assert_eq!(r.same_sequence, Some(false));
    }

    #[test]
    fn unparsable_and_missing_logs_are_reported() {
        let r = analyze(&[log("run 1", &["{not json".to_string()])], &req(), None);
        assert_eq!(checks(&r), vec!["logged"]);
        let missing = load("run 1", Path::new("does/not/exist.jsonl"));
        let r = analyze(&[missing], &req(), None);
        assert_eq!(checks(&r), vec!["logged"]);
    }
}
