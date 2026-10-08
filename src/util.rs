//! Small shared helpers: path normalization, glob matching, terminal colour, formatting.

use anyhow::{Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};

/// Turns a relative path into the `/`-separated form used in manifests and reports.
pub fn to_slash(rel: &Path) -> String {
    let mut out = String::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&s.to_string_lossy());
            }
            Component::ParentDir => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str("..");
            }
            _ => {}
        }
    }
    out
}

/// Normalizes a user-written relative path string (`a\b\c` or `./a/b`) to `a/b/c`.
pub fn slash_str(s: &str) -> String {
    let replaced = s.replace('\\', "/");
    let parts: Vec<&str> = replaced
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    parts.join("/")
}

/// Absolute form of `p` without resolving symlinks (canonicalize would add `\\?\` on Windows,
/// which many child programs cannot handle).
pub fn absolute(p: &Path) -> Result<PathBuf> {
    std::path::absolute(p).with_context(|| format!("cannot resolve path {}", p.display()))
}

/// Compiles one glob. Patterns always use `/`; a `\` is treated as a separator so that
/// Windows-style patterns work. `*` does not cross `/`; use `**` for any depth.
pub fn build_glob(pattern: &str) -> Result<Glob> {
    let norm = pattern.replace('\\', "/");
    GlobBuilder::new(&norm)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .with_context(|| format!("invalid glob pattern {pattern:?}"))
}

pub fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(build_glob(p)?);
    }
    b.build().context("cannot build glob set")
}

/// Include/exclude file selector over `/`-separated relative paths.
pub struct Selector {
    include: GlobSet,
    exclude: GlobSet,
}

impl Selector {
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        let include = if include.is_empty() {
            vec!["**".to_string()]
        } else {
            include.to_vec()
        };
        Ok(Self {
            include: build_globset(&include)?,
            exclude: build_globset(exclude)?,
        })
    }

    pub fn matches(&self, rel: &str) -> bool {
        self.include.is_match(rel) && !self.exclude.is_match(rel)
    }
}

/// The literal directory prefix of a glob (`data/test/**` -> `data/test`), used to avoid
/// walking the whole project when hashing inputs.
pub fn glob_base(pattern: &str) -> String {
    let norm = pattern.replace('\\', "/");
    let mut base: Vec<&str> = Vec::new();
    let parts: Vec<&str> = norm.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let is_last = i + 1 == parts.len();
        if part.contains(['*', '?', '[', '{']) || is_last {
            break;
        }
        if !part.is_empty() && *part != "." {
            base.push(part);
        }
    }
    base.join("/")
}

/// First 12 hex characters of a digest, as shown in terminal output.
pub fn short(h: &str) -> &str {
    &h[..h.len().min(12)]
}

pub fn fmt_secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

pub fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KiB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MiB", f / (K * K))
    } else {
        format!("{:.2} GiB", f / (K * K * K))
    }
}

/// Truncates a string to at most `max` characters, marking the cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// ANSI colour that is off when the stream is not a TTY or `NO_COLOR` is set.
#[derive(Clone, Copy)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn for_stdout() -> Self {
        Self::detect(std::io::stdout().is_terminal())
    }

    pub fn for_stderr() -> Self {
        Self::detect(std::io::stderr().is_terminal())
    }

    fn detect(is_tty: bool) -> Self {
        let no_color = match std::env::var_os("NO_COLOR") {
            Some(v) => !v.is_empty(),
            None => false,
        };
        Self {
            enabled: is_tty && !no_color && enable_vt(),
        }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
}

#[cfg(windows)]
fn enable_vt() -> bool {
    type Handle = *mut core::ffi::c_void;
    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    }
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    // SAFETY: plain Win32 console calls on the process's own stdout handle.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_style_paths_become_slash_paths() {
        assert_eq!(slash_str(r"data\test\a.jsonl"), "data/test/a.jsonl");
        assert_eq!(slash_str("./data//test/"), "data/test");
        let p: PathBuf = ["a", "b", "c.txt"].iter().collect();
        assert_eq!(to_slash(&p), "a/b/c.txt");
    }

    #[test]
    fn include_exclude_globs() {
        let s = Selector::new(
            &["**/*.parquet".into(), "**/*.jsonl".into()],
            &["**/tmp/**".into()],
        )
        .unwrap();
        assert!(s.matches("metrics.parquet"));
        assert!(s.matches("a/b/log.jsonl"));
        assert!(!s.matches("tmp/x.parquet"));
        assert!(!s.matches("a/tmp/x.jsonl"));
        assert!(!s.matches("notes.txt"));
        let all = Selector::new(&[], &[]).unwrap();
        assert!(all.matches("deep/dir/file.bin"));
    }

    #[test]
    fn star_does_not_cross_directories() {
        let s = Selector::new(&["serve/*.yaml".into()], &[]).unwrap();
        assert!(s.matches("serve/a.yaml"));
        assert!(!s.matches("serve/x/a.yaml"));
    }

    #[test]
    fn backslash_patterns_work() {
        let s = Selector::new(&[r"data\test\**".into()], &[]).unwrap();
        assert!(s.matches("data/test/x/y.jsonl"));
    }

    #[test]
    fn glob_base_is_literal_prefix() {
        assert_eq!(glob_base("data/test/**"), "data/test");
        assert_eq!(glob_base("serve/*.yaml"), "serve");
        assert_eq!(glob_base("uv.lock"), "");
        assert_eq!(glob_base("**/*.json"), "");
    }
}
