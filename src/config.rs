//! `repro.toml` loading and the canonical hash of the experiment YAML config.

use crate::util;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_norway::Value as Yaml;
use std::path::Path;

pub const FILE_NAME: &str = "repro.toml";

pub const DEFAULT_KEY_PARTS: [&str; 4] = ["state_hash", "expert", "schema_version", "seed"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReproConfig {
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub run: Run,
    #[serde(default)]
    pub outputs: Outputs,
    #[serde(default)]
    pub normalize: Vec<NormalizeRule>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    #[serde(default)]
    pub inputs: Inputs,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: Option<String>,
    /// Experiment YAML, hashed canonically.
    pub config: Option<String>,
    pub seed: Option<u64>,
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outputs {
    #[serde(default = "default_include")]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Default for Outputs {
    fn default() -> Self {
        Self {
            include: default_include(),
            exclude: Vec::new(),
        }
    }
}

fn default_include() -> Vec<String> {
    vec!["**".to_string()]
}

/// A declared, visible exception. Every use is listed in the verdict.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NormalizeRule {
    pub files: String,
    /// Top-level keys (or dotted paths like `meta.host`) removed from JSON / JSONL records.
    #[serde(default)]
    pub drop_json_fields: Vec<String>,
    /// CSV/TSV columns (by header name) removed before comparing.
    #[serde(default)]
    pub drop_csv_columns: Vec<String>,
    /// Compare the file's lines as a sorted list (order is ignored).
    #[serde(default)]
    pub sort_lines: bool,
    #[serde(default)]
    pub reason: String,
}

impl NormalizeRule {
    /// Short human description, e.g. `*.log.jsonl drops ts,run_id,wall_ms`.
    pub fn describe(&self) -> String {
        let files = self.files.strip_prefix("**/").unwrap_or(&self.files);
        let mut parts = Vec::new();
        if !self.drop_json_fields.is_empty() {
            parts.push(format!("drops {}", self.drop_json_fields.join(",")));
        }
        if !self.drop_csv_columns.is_empty() {
            parts.push(format!("drops columns {}", self.drop_csv_columns.join(",")));
        }
        if self.sort_lines {
            parts.push("sorts lines".to_string());
        }
        format!("{files} {}", parts.join(", "))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Path of the cache log, placeholders allowed (`{run_dir}/cache.jsonl`).
    pub log: String,
    #[serde(default = "default_key_parts")]
    pub required_key_parts: Vec<String>,
}

pub fn default_key_parts() -> Vec<String> {
    DEFAULT_KEY_PARTS.iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inputs {
    #[serde(default)]
    pub files: Vec<String>,
}

/// Loads `repro.toml` if it exists. `Ok(None)` means there is no file.
pub fn load(path: &Path) -> Result<Option<ReproConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let cfg = parse(&text).with_context(|| {
        format!(
            "{} is invalid (run `repro init` in an empty directory to see a commented example)",
            path.display()
        )
    })?;
    Ok(Some(cfg))
}

pub fn parse(text: &str) -> Result<ReproConfig> {
    let cfg: ReproConfig = toml::from_str(text)?;
    cfg.validate()?;
    Ok(cfg)
}

impl ReproConfig {
    fn validate(&self) -> Result<()> {
        util::Selector::new(&self.outputs.include, &self.outputs.exclude)?;
        for p in &self.inputs.files {
            util::build_glob(p)?;
        }
        for (i, r) in self.normalize.iter().enumerate() {
            let n = i + 1;
            util::build_glob(&r.files).with_context(|| format!("[[normalize]] #{n}"))?;
            if r.reason.trim().is_empty() {
                bail!(
                    "[[normalize]] #{n} ({}) has no `reason`; every exception must say why",
                    r.files
                );
            }
            if r.drop_json_fields.is_empty() && r.drop_csv_columns.is_empty() && !r.sort_lines {
                bail!(
                    "[[normalize]] #{n} ({}) does nothing; set drop_json_fields, drop_csv_columns or sort_lines",
                    r.files
                );
            }
        }
        if let Some(c) = &self.cache {
            if c.log.trim().is_empty() {
                bail!("[cache].log is empty; set it to e.g. \"{{run_dir}}/cache.jsonl\"");
            }
        }
        if self.project.timeout_s == Some(0) {
            bail!("[project].timeout_s must be > 0 (omit it for no timeout)");
        }
        Ok(())
    }
}

/// Canonical JSON of a YAML document: keys sorted, numbers and strings normalized,
/// no whitespace. Comments, key order and quoting style do not change the result.
pub fn canonical_json_from_yaml(text: &str) -> Result<String> {
    let v: Yaml = serde_norway::from_str(text).context("cannot parse YAML")?;
    let mut out = String::new();
    write_canonical(&v, &mut out)?;
    Ok(out)
}

/// BLAKE3 (64 hex) of the canonical JSON of a YAML file.
pub fn hash_yaml_file(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read experiment config {} (set [project].config in repro.toml)",
            path.display()
        )
    })?;
    let canon =
        canonical_json_from_yaml(&text).with_context(|| format!("in {}", path.display()))?;
    Ok(blake3::hash(canon.as_bytes()).to_hex().to_string())
}

fn write_canonical(v: &Yaml, out: &mut String) -> Result<()> {
    match v {
        Yaml::Null => out.push_str("null"),
        Yaml::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Yaml::Number(n) => out.push_str(&canonical_number(n)),
        Yaml::String(s) => out.push_str(&serde_json::to_string(s)?),
        Yaml::Sequence(seq) => {
            out.push('[');
            for (i, x) in seq.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out)?;
            }
            out.push(']');
        }
        Yaml::Mapping(m) => {
            let mut entries: Vec<(String, &Yaml)> = Vec::with_capacity(m.len());
            for (k, val) in m.iter() {
                entries.push((key_string(k)?, val));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for w in entries.windows(2) {
                if w[0].0 == w[1].0 {
                    bail!("duplicate key {:?}", w[0].0);
                }
            }
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k)?);
                out.push(':');
                write_canonical(val, out)?;
            }
            out.push('}');
        }
        Yaml::Tagged(t) => {
            out.push_str("{\"!tag\":");
            out.push_str(&serde_json::to_string(&t.tag.to_string())?);
            out.push_str(",\"value\":");
            write_canonical(&t.value, out)?;
            out.push('}');
        }
    }
    Ok(())
}

fn key_string(k: &Yaml) -> Result<String> {
    match k {
        Yaml::String(s) => Ok(s.clone()),
        other => {
            let mut s = String::new();
            write_canonical(other, &mut s)?;
            Ok(s)
        }
    }
}

fn canonical_number(n: &serde_norway::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    canonical_float(n.as_f64().unwrap_or(f64::NAN))
}

/// Shortest round-trip form; always contains `.` or `e` so ints and floats stay distinct.
pub fn canonical_float(f: f64) -> String {
    if f.is_nan() {
        "\"NaN\"".to_string()
    } else if f.is_infinite() {
        if f > 0.0 {
            "\"Infinity\"".to_string()
        } else {
            "\"-Infinity\"".to_string()
        }
    } else if f == 0.0 {
        "0.0".to_string()
    } else {
        format!("{f:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> String {
        blake3::hash(canonical_json_from_yaml(s).unwrap().as_bytes())
            .to_hex()
            .to_string()
    }

    #[test]
    fn key_order_and_comments_do_not_change_hash() {
        let a = "model: qwen\nlr: 0.5\nsplits:\n  train: 0.8\n  test: 0.2\n";
        let b = "# a comment\nsplits:\n  test: 0.2   # trailing\n  train: 0.8\nlr: 0.50\nmodel: \"qwen\"\n";
        assert_eq!(h(a), h(b));
    }

    #[test]
    fn value_change_changes_hash() {
        let a = "model: qwen\nlr: 0.5\n";
        let b = "model: qwen\nlr: 0.25\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn numbers_are_normalized_but_int_and_float_differ() {
        assert_eq!(h("x: 1e-3"), h("x: 0.001"));
        assert_ne!(h("x: 1"), h("x: 1.0"));
        assert_eq!(
            canonical_json_from_yaml("b: [1, 2.50, true, null]\na: s").unwrap(),
            r#"{"a":"s","b":[1,2.5,true,null]}"#
        );
    }

    #[test]
    fn parses_example_config() {
        let text = include_str!("../repro.toml.example");
        let cfg = parse(text).unwrap();
        assert_eq!(cfg.project.seed, Some(1234));
        assert_eq!(cfg.normalize.len(), 1);
        assert_eq!(
            cfg.normalize[0].describe(),
            "*.log.jsonl drops ts,run_id,wall_ms"
        );
        assert_eq!(cfg.cache.unwrap().required_key_parts.len(), 4);
    }

    #[test]
    fn rejects_normalize_without_reason() {
        let text = "[[normalize]]\nfiles = \"**/*.json\"\ndrop_json_fields = [\"ts\"]\n";
        let err = format!("{:#}", parse(text).unwrap_err());
        assert!(err.contains("reason"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(parse("[project]\nnmae = \"x\"\n").is_err());
    }
}
