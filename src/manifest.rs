//! Versioned manifest: every output file of a run (path, size, BLAKE3) plus provenance.

use crate::hashing::{self, HashCache};
use crate::provenance::Provenance;
use crate::util::{self, Selector};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SCHEMA: u32 = 1;
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    /// Relative path with `/` separators.
    pub path: String,
    pub size: u64,
    pub blake3: String,
    /// Hash after the declared `[[normalize]]` rules, when any rule applies to the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_blake3: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub repro_schema: u32,
    pub tool_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_blake3: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_blake3: Option<String>,
    pub files: Vec<FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

impl Manifest {
    pub fn new(files: Vec<FileEntry>) -> Self {
        Self {
            repro_schema: SCHEMA,
            tool_version: TOOL_VERSION.to_string(),
            run_id: None,
            config_hash: None,
            seed: None,
            command: None,
            exit_code: None,
            wall_ms: None,
            peak_rss_mb: None,
            stdout_blake3: None,
            stderr_blake3: None,
            files,
            provenance: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("cannot read manifest {}", path.display()))?;
        let m: Manifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a repro manifest", path.display()))?;
        if m.repro_schema > SCHEMA {
            bail!(
                "{} uses manifest schema {} but this repro understands up to {}; upgrade repro",
                path.display(),
                m.repro_schema,
                SCHEMA
            );
        }
        for f in &m.files {
            check_rel_path(&f.path)
                .with_context(|| format!("bad entry in manifest {}", path.display()))?;
        }
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
        }
        std::fs::write(path, json).with_context(|| format!("cannot write {}", path.display()))
    }

    /// True when the file looks like a manifest (used by `repro diff` to pick a mode).
    pub fn sniff(path: &Path) -> bool {
        path.is_file()
            && std::fs::read(path)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .is_some_and(|v| v.get("repro_schema").is_some() && v.get("files").is_some())
    }
}

/// Manifest paths must be relative, `/`-separated and stay inside their root.
pub fn check_rel_path(p: &str) -> Result<()> {
    if p.is_empty()
        || p.starts_with('/')
        || p.contains('\\')
        || p.split('/').any(|c| c == ".." || c.is_empty())
        || (p.len() >= 2 && p.as_bytes()[1] == b':')
    {
        bail!("invalid manifest path {p:?}: must be relative, use '/', and not contain '..'");
    }
    Ok(())
}

/// A file found on disk: its manifest path and where it lives.
#[derive(Debug, Clone)]
pub struct Found {
    pub rel: String,
    pub abs: PathBuf,
}

/// Lists files under `root` (recursively) whose relative path matches `sel`, sorted by path.
/// Directories named in `skip_dirs` (relative paths) are not entered.
pub fn walk_files(root: &Path, sel: &Selector, skip_dirs: &[String]) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let rel = e
                .path()
                .strip_prefix(root)
                .map(util::to_slash)
                .unwrap_or_default();
            !skip_dirs.iter().any(|s| *s == rel)
        });
    for entry in walker {
        let entry = entry.with_context(|| format!("cannot list {}", root.display()))?;
        let is_file = entry.file_type().is_file()
            || (entry.file_type().is_symlink() && entry.path().is_file());
        if !is_file {
            continue;
        }
        let rel = util::to_slash(entry.path().strip_prefix(root).unwrap_or(entry.path()));
        if sel.matches(&rel) {
            out.push(Found {
                rel,
                abs: entry.path().to_path_buf(),
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Hashes found files in parallel into manifest entries (sorted by path).
pub fn hash_found(found: &[Found], cache: &HashCache) -> Result<Vec<FileEntry>> {
    let results = hashing::parallel_map(found, |f| {
        cache.hash(&f.abs).map(|(size, blake3)| FileEntry {
            path: f.rel.clone(),
            size,
            blake3,
            normalized_blake3: None,
        })
    });
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        out.push(r?);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Collects files for `repro hash` / `repro verify`: each argument is a file or directory,
/// and manifest paths are relative to `root`.
pub fn collect_paths(paths: &[PathBuf], root: &Path, sel: &Selector) -> Result<Vec<Found>> {
    let root_abs = util::absolute(root)?;
    let mut out: Vec<Found> = Vec::new();
    for p in paths {
        let abs = util::absolute(p)?;
        if !abs.exists() {
            bail!("{} does not exist", p.display());
        }
        let Ok(rel_base) = abs.strip_prefix(&root_abs) else {
            bail!(
                "{} is outside {}; run repro from a directory that contains it or pass --root",
                p.display(),
                root_abs.display()
            );
        };
        let rel_base = util::to_slash(rel_base);
        if abs.is_dir() {
            for f in walk_files(&abs, &Selector::new(&[], &[])?, &[])? {
                let rel = if rel_base.is_empty() {
                    f.rel.clone()
                } else {
                    format!("{rel_base}/{}", f.rel)
                };
                if sel.matches(&rel) {
                    out.push(Found { rel, abs: f.abs });
                }
            }
        } else if sel.matches(&rel_base) {
            out.push(Found { rel: rel_base, abs });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out.dedup_by(|a, b| a.rel == b.rel);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip() {
        let mut m = Manifest::new(vec![FileEntry {
            path: "a/metrics.csv".into(),
            size: 12,
            blake3: "ab".repeat(32),
            normalized_blake3: None,
        }]);
        m.run_id = Some("repro-3fa91c02-1".into());
        m.seed = Some(1234);
        let dir = std::env::temp_dir().join(format!("repro-manifest-test-{}", std::process::id()));
        let p = dir.join("m.json");
        m.save(&p).unwrap();
        let back = Manifest::load(&p).unwrap();
        assert_eq!(m, back);
        assert!(Manifest::sniff(&p));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("normalized_blake3"));
        assert!(!text.contains("provenance"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(check_rel_path("a/b.txt").is_ok());
        assert!(check_rel_path("../x").is_err());
        assert!(check_rel_path("/etc/passwd").is_err());
        assert!(check_rel_path(r"a\b").is_err());
        assert!(check_rel_path("C:/x").is_err());
        assert!(check_rel_path("a//b").is_err());
    }
}
