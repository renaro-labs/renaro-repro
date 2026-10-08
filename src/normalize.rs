//! Declared normalizations (`[[normalize]]`). They are applied to a normalized stream used
//! only for comparison; raw hashes are always kept. Line endings are never touched.

use crate::config::NormalizeRule;
use crate::hashing::HashWriter;
use crate::util;
use anyhow::{Context, Result};
use globset::GlobMatcher;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

pub struct Normalizer {
    rules: Vec<NormalizeRule>,
    matchers: Vec<GlobMatcher>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Json,
    Jsonl,
    Csv(u8),
    Text,
}

pub fn kind_of(rel: &str) -> Kind {
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".jsonl") || lower.ends_with(".ndjson") {
        Kind::Jsonl
    } else if lower.ends_with(".json") {
        Kind::Json
    } else if lower.ends_with(".csv") {
        Kind::Csv(b',')
    } else if lower.ends_with(".tsv") {
        Kind::Csv(b'\t')
    } else {
        Kind::Text
    }
}

impl Normalizer {
    pub fn new(rules: &[NormalizeRule]) -> Result<Self> {
        let mut matchers = Vec::with_capacity(rules.len());
        for r in rules {
            matchers.push(util::build_glob(&r.files)?.compile_matcher());
        }
        Ok(Self {
            rules: rules.to_vec(),
            matchers,
        })
    }

    pub fn rules(&self) -> &[NormalizeRule] {
        &self.rules
    }

    /// Indices of the rules that apply to a relative path, in declaration order.
    pub fn rules_for(&self, rel: &str) -> Vec<usize> {
        self.matchers
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_match(rel))
            .map(|(i, _)| i)
            .collect()
    }

    /// Hash of the normalized content.
    pub fn normalized_hash(&self, path: &Path, rel: &str, idx: &[usize]) -> Result<String> {
        let mut w = HashWriter::new();
        self.normalize_to(path, rel, idx, &mut w)?;
        Ok(w.finish())
    }

    /// Normalized content in memory, or `None` when the file is larger than `cap` bytes.
    pub fn normalized_bytes(
        &self,
        path: &Path,
        rel: &str,
        idx: &[usize],
        cap: u64,
    ) -> Result<Option<Vec<u8>>> {
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot stat {}", path.display()))?
            .len();
        if size > cap {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(size as usize);
        self.normalize_to(path, rel, idx, &mut out)?;
        Ok(Some(out))
    }

    pub fn normalize_to(
        &self,
        path: &Path,
        rel: &str,
        idx: &[usize],
        w: &mut dyn Write,
    ) -> Result<()> {
        let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let rules: Vec<&NormalizeRule> = idx.iter().map(|&i| &self.rules[i]).collect();
        normalize_reader(
            BufReader::with_capacity(1 << 16, f),
            kind_of(rel),
            &rules,
            w,
        )
        .with_context(|| format!("cannot normalize {}", path.display()))
    }
}

/// Core normalization over any reader (separate from files for testing).
pub fn normalize_reader(
    mut r: impl BufRead,
    kind: Kind,
    rules: &[&NormalizeRule],
    w: &mut dyn Write,
) -> Result<()> {
    let drop_fields: Vec<&str> = rules
        .iter()
        .flat_map(|r| r.drop_json_fields.iter().map(|s| s.as_str()))
        .collect();
    let drop_cols: Vec<&str> = rules
        .iter()
        .flat_map(|r| r.drop_csv_columns.iter().map(|s| s.as_str()))
        .collect();
    let sort = rules.iter().any(|r| r.sort_lines);

    if kind == Kind::Json {
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes)?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut v) if !drop_fields.is_empty() => {
                for f in &drop_fields {
                    drop_path(&mut v, f);
                }
                serde_json::to_writer(&mut *w, &v)?;
            }
            _ => w.write_all(&bytes)?,
        }
        return Ok(());
    }

    let mut sorted: Vec<Vec<u8>> = Vec::new();
    let mut drop_idx: Option<Vec<usize>> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let (content, term) = split_terminator(&line);
        let mut out: Vec<u8> = match kind {
            Kind::Jsonl if !drop_fields.is_empty() => {
                match serde_json::from_slice::<Value>(content) {
                    Ok(mut v) => {
                        for f in &drop_fields {
                            drop_path(&mut v, f);
                        }
                        serde_json::to_vec(&v)?
                    }
                    Err(_) => content.to_vec(),
                }
            }
            Kind::Csv(d) if !drop_cols.is_empty() => {
                let fields = split_csv(content, d);
                let idx = drop_idx.get_or_insert_with(|| {
                    fields
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| drop_cols.contains(&unquote(f).as_str()))
                        .map(|(i, _)| i)
                        .collect()
                });
                let kept: Vec<&[u8]> = fields
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !idx.contains(i))
                    .map(|(_, f)| *f)
                    .collect();
                kept.join(&[d][..])
            }
            _ => content.to_vec(),
        };
        out.extend_from_slice(term);
        if sort {
            sorted.push(out);
        } else {
            w.write_all(&out)?;
        }
    }
    if sort {
        sorted.sort();
        for l in &sorted {
            w.write_all(l)?;
        }
    }
    Ok(())
}

/// Splits a line into content and its exact terminator (`\r\n`, `\n` or nothing).
pub fn split_terminator(line: &[u8]) -> (&[u8], &[u8]) {
    if line.ends_with(b"\r\n") {
        line.split_at(line.len() - 2)
    } else if line.ends_with(b"\n") {
        line.split_at(line.len() - 1)
    } else {
        (line, &[])
    }
}

/// Minimal CSV field splitter: honours double quotes, not embedded newlines.
pub fn split_csv(line: &[u8], delim: u8) -> Vec<&[u8]> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, &b) in line.iter().enumerate() {
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if b == delim && !in_quotes {
            fields.push(&line[start..i]);
            start = i + 1;
        }
    }
    fields.push(&line[start..]);
    fields
}

pub fn unquote(field: &[u8]) -> String {
    let s = String::from_utf8_lossy(field);
    let t = s.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(t);
    t.replace("\"\"", "\"")
}

/// Removes `a.b.c` from a JSON value (no-op when the path does not exist).
pub fn drop_path(v: &mut Value, path: &str) {
    let mut parts = path.split('.').peekable();
    let mut cur = v;
    while let Some(p) = parts.next() {
        let Value::Object(map) = cur else { return };
        if parts.peek().is_none() {
            map.remove(p);
            return;
        }
        match map.get_mut(p) {
            Some(next) => cur = next,
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(f: &[&str], cols: &[&str], sort: bool) -> NormalizeRule {
        NormalizeRule {
            files: "**/*".into(),
            drop_json_fields: f.iter().map(|s| s.to_string()).collect(),
            drop_csv_columns: cols.iter().map(|s| s.to_string()).collect(),
            sort_lines: sort,
            reason: "test".into(),
        }
    }

    fn norm(input: &str, kind: Kind, r: &NormalizeRule) -> String {
        let mut out = Vec::new();
        normalize_reader(input.as_bytes(), kind, &[r], &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn jsonl_drops_fields_and_keeps_line_endings() {
        let r = rule(&["ts", "run_id"], &[], false);
        let a = "{\"ts\":\"2026-01-01\",\"loss\":0.5,\"run_id\":\"r1\"}\r\nnot json\n{\"loss\":1}";
        let out = norm(a, Kind::Jsonl, &r);
        assert_eq!(out, "{\"loss\":0.5}\r\nnot json\n{\"loss\":1}");
    }

    #[test]
    fn crlf_difference_survives_normalization() {
        let r = rule(&["ts"], &[], false);
        let a = norm("{\"x\":1,\"ts\":1}\n", Kind::Jsonl, &r);
        let b = norm("{\"x\":1,\"ts\":2}\r\n", Kind::Jsonl, &r);
        assert_ne!(a, b);
    }

    #[test]
    fn nested_json_field_is_dropped() {
        let r = rule(&["meta.host"], &[], false);
        let out = norm(
            "{\"meta\":{\"host\":\"a\",\"k\":1},\"v\":2}",
            Kind::Json,
            &r,
        );
        assert_eq!(out, "{\"meta\":{\"k\":1},\"v\":2}");
    }

    #[test]
    fn csv_column_is_dropped() {
        let r = rule(&[], &["wall_ms"], false);
        let out = norm(
            "step,wall_ms,loss\n1,123,0.5\n2,\"4,5\",0.4\n",
            Kind::Csv(b','),
            &r,
        );
        assert_eq!(out, "step,loss\n1,0.5\n2,0.4\n");
    }

    #[test]
    fn sort_lines_ignores_order() {
        let r = rule(&[], &[], true);
        assert_eq!(
            norm("b\na\nc\n", Kind::Text, &r),
            norm("c\nb\na\n", Kind::Text, &r)
        );
    }

    #[test]
    fn rules_match_by_glob() {
        let rules = vec![NormalizeRule {
            files: "**/*.log.jsonl".into(),
            drop_json_fields: vec!["ts".into()],
            reason: "r".into(),
            ..Default::default()
        }];
        let n = Normalizer::new(&rules).unwrap();
        assert_eq!(n.rules_for("train.log.jsonl"), vec![0]);
        assert_eq!(n.rules_for("a/b/eval.log.jsonl"), vec![0]);
        assert!(n.rules_for("cache.jsonl").is_empty());
    }
}
