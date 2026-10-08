//! Best-effort provenance: git state, OS, CPU, GPUs, allowlisted env vars, input hashes.
//! Nothing here may fail the run: missing tools simply omit their fields.

use crate::hashing::HashCache;
use crate::util::{self, Selector};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InputHash {
    pub path: String,
    pub size: u64,
    pub blake3: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Provenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<bool>,
    pub os: String,
    pub cpu_threads: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpus: Option<Vec<String>>,
    #[serde(default)]
    pub inputs: Vec<InputHash>,
    /// Only allowlisted variables are ever recorded (never secrets).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

const TOOL_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs a helper program with a 2 s timeout; `None` on any failure.
pub fn run_quiet(program: &str, args: &[&str], cwd: &Path) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if start.elapsed() < TOOL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Collects machine and repository provenance. `env_allow` names the only env vars recorded.
pub fn gather(root: &Path, env_allow: &[String]) -> Provenance {
    let (git, gpus, os) = std::thread::scope(|s| {
        let git = s.spawn(|| git_state(root));
        let gpus = s.spawn(gpu_names);
        let os = s.spawn(os_name);
        (
            git.join().unwrap_or((None, None)),
            gpus.join().unwrap_or(None),
            os.join()
                .unwrap_or_else(|_| std::env::consts::OS.to_string()),
        )
    });
    Provenance {
        git_commit: git.0,
        git_dirty: git.1,
        os,
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        gpus,
        inputs: Vec::new(),
        env: allowed_env(env_allow),
    }
}

pub fn allowed_env(env_allow: &[String]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in env_allow {
        if let Ok(v) = std::env::var(name) {
            env.insert(name.clone(), v);
        }
    }
    env
}

fn git_state(root: &Path) -> (Option<String>, Option<bool>) {
    let Some(commit) = run_quiet("git", &["rev-parse", "HEAD"], root) else {
        return (None, None);
    };
    let commit = commit.trim().to_string();
    if commit.is_empty() {
        return (None, None);
    }
    // Untracked files (like repro's own work dir) do not count as dirty.
    let dirty = run_quiet(
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
        root,
    )
    .map(|s| !s.trim().is_empty());
    (Some(commit), dirty)
}

fn gpu_names() -> Option<Vec<String>> {
    let out = run_quiet(
        "nvidia-smi",
        &["--query-gpu=name", "--format=csv,noheader"],
        Path::new("."),
    )?;
    let names: Vec<String> = out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    (!names.is_empty()).then_some(names)
}

#[cfg(windows)]
fn os_name() -> String {
    // `ver` prints e.g. "Microsoft Windows [Version 10.0.22631.4317]"; build >= 22000 is Windows 11.
    let build = run_quiet("cmd", &["/C", "ver"], Path::new(".")).and_then(|s| {
        let v = s.split("Version").nth(1)?;
        let v = v.trim().trim_end_matches(']').trim();
        v.split('.').nth(2)?.parse::<u32>().ok()
    });
    match build {
        Some(b) if b >= 22000 => "windows-11".to_string(),
        Some(_) => "windows-10".to_string(),
        None => "windows".to_string(),
    }
}

#[cfg(not(windows))]
fn os_name() -> String {
    let base = std::env::consts::OS.to_string();
    let Ok(text) = std::fs::read_to_string("/etc/os-release") else {
        return base;
    };
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.trim_matches('"').to_string())
    };
    match (field("ID="), field("VERSION_ID=")) {
        (Some(id), Some(ver)) => format!("{base}-{id}-{ver}"),
        (Some(id), None) => format!("{base}-{id}"),
        _ => base,
    }
}

/// Host name for the "hostname leaked into output" hint. Empty when unknown.
pub fn hostname() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(h) = std::env::var(var) {
            if !h.trim().is_empty() {
                return h.trim().to_string();
            }
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Hashes the `[inputs]` files (globs relative to `root`), sorted by path.
pub fn hash_inputs(
    root: &Path,
    patterns: &[String],
    skip_dirs: &[String],
    cache: &HashCache,
) -> Result<Vec<InputHash>> {
    let mut found = Vec::new();
    for pat in patterns {
        let has_glob = pat.contains(['*', '?', '[', '{']);
        if !has_glob {
            let rel = util::slash_str(pat);
            let p = root.join(&rel);
            if p.is_file() {
                found.push(crate::manifest::Found { rel, abs: p });
                continue;
            }
            if p.is_dir() {
                let sel = Selector::new(&[], &[])?;
                for f in crate::manifest::walk_files(&p, &sel, &[])? {
                    found.push(crate::manifest::Found {
                        rel: format!("{rel}/{}", f.rel),
                        abs: f.abs,
                    });
                }
            }
            continue;
        }
        let base = util::glob_base(pat);
        let base_dir = if base.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&base)
        };
        if !base_dir.is_dir() {
            continue;
        }
        let sel = Selector::new(std::slice::from_ref(pat), &[])?;
        let all = Selector::new(&[], &[])?;
        for f in crate::manifest::walk_files(&base_dir, &all, skip_dirs)? {
            let rel = if base.is_empty() {
                f.rel.clone()
            } else {
                format!("{base}/{}", f.rel)
            };
            if sel.matches(&rel) {
                found.push(crate::manifest::Found { rel, abs: f.abs });
            }
        }
    }
    found.sort_by(|a, b| a.rel.cmp(&b.rel));
    found.dedup_by(|a, b| a.rel == b.rel);
    let entries = crate::manifest::hash_found(&found, cache)?;
    Ok(entries
        .into_iter()
        .map(|e| InputHash {
            path: e.path,
            size: e.size,
            blake3: e.blake3,
        })
        .collect())
}
