//! Spawns the user's command with its env, a timeout, captured output and peak memory.

use crate::hashing::HashWriter;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Lines of stdout/stderr kept for the report.
const TAIL_LINES: usize = 200;
const MAX_LINE: usize = 2000;

/// Variables kept even with `--strict-env`, because programs cannot start without them.
pub const ESSENTIAL_ENV: [&str; 12] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "LANG",
];

pub struct RunSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Extra variables (the `REPRO_*` ones).
    pub env: Vec<(String, String)>,
    /// `Some(allowlist)`: clear the environment and keep only these (plus essentials).
    pub strict_env: Option<Vec<String>>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub wall_ms: u64,
    pub peak_rss_mb: Option<u64>,
    pub pid: u32,
    pub stdout_blake3: String,
    pub stderr_blake3: String,
    pub stdout_tail: Vec<String>,
    pub stderr_tail: Vec<String>,
}

impl RunOutcome {
    pub fn ok(&self) -> bool {
        self.spawn_error.is_none() && !self.timed_out && self.exit_code == Some(0)
    }

    pub fn failure_text(&self) -> Option<String> {
        if let Some(e) = &self.spawn_error {
            return Some(format!("could not start command: {e}"));
        }
        if self.timed_out {
            return Some(format!(
                "timed out after {} and was killed",
                crate::util::fmt_secs(self.wall_ms)
            ));
        }
        if let Some(sig) = self.signal {
            return Some(format!("killed by signal {sig}"));
        }
        match self.exit_code {
            Some(0) => None,
            Some(c) => Some(format!("exited with code {c}")),
            None => Some("exited without a status code".to_string()),
        }
    }
}

struct Captured {
    blake3: String,
    tail: Vec<String>,
}

fn capture<R: Read + Send + 'static>(mut r: R) -> JoinHandle<Captured> {
    std::thread::spawn(move || {
        let mut hasher = HashWriter::new();
        let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL_LINES + 1);
        let mut cur: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        let push_line = |line: &[u8], tail: &mut VecDeque<String>| {
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end_matches(['\r', '\n']);
            tail.push_back(crate::util::truncate(text, MAX_LINE));
            if tail.len() > TAIL_LINES {
                tail.pop_front();
            }
        };
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let _ = hasher.write_all(&buf[..n]);
            for &b in &buf[..n] {
                if b == b'\n' {
                    push_line(&cur, &mut tail);
                    cur.clear();
                } else if cur.len() < MAX_LINE * 4 {
                    cur.push(b);
                }
            }
        }
        if !cur.is_empty() {
            push_line(&cur, &mut tail);
        }
        Captured {
            blake3: hasher.finish(),
            tail: tail.into_iter().collect(),
        }
    })
}

pub fn run(spec: &RunSpec) -> RunOutcome {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(allow) = &spec.strict_env {
        cmd.env_clear();
        for (k, v) in std::env::vars_os() {
            let name = k.to_string_lossy().to_string();
            let keep = ESSENTIAL_ENV.iter().any(|e| e.eq_ignore_ascii_case(&name))
                || allow.contains(&name);
            if keep {
                cmd.env(k, v);
            }
        }
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::NotFound {
                format!("{e} ({:?} not found on PATH)", spec.program)
            } else {
                e.to_string()
            };
            return RunOutcome {
                spawn_error: Some(hint),
                ..Default::default()
            };
        }
    };
    let pid = child.id();
    let out_h = child.stdout.take().map(capture);
    let err_h = child.stderr.take().map(capture);

    let waited = sys::wait(&mut child, spec.timeout, start);
    let wall_ms = start.elapsed().as_millis() as u64;

    let join = |h: Option<JoinHandle<Captured>>| {
        h.and_then(|h| h.join().ok()).unwrap_or(Captured {
            blake3: crate::hashing::hash_bytes(b""),
            tail: Vec::new(),
        })
    };
    let out = join(out_h);
    let err = join(err_h);

    RunOutcome {
        exit_code: waited.exit_code,
        signal: waited.signal,
        timed_out: waited.timed_out,
        spawn_error: None,
        wall_ms,
        peak_rss_mb: waited.peak_bytes.map(|b| b / (1024 * 1024)),
        pid,
        stdout_blake3: out.blake3,
        stderr_blake3: err.blake3,
        stdout_tail: out.tail,
        stderr_tail: err.tail,
    }
}

struct Waited {
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    peak_bytes: Option<u64>,
}

fn poll_sleep(start: Instant) {
    // Short sleeps at first so fast commands are not slowed down.
    let ms = if start.elapsed() < Duration::from_millis(200) {
        5
    } else {
        25
    };
    std::thread::sleep(Duration::from_millis(ms));
}

fn timed_out(start: Instant, timeout: Option<Duration>) -> bool {
    timeout.is_some_and(|t| start.elapsed() >= t)
}

/// Linux: reap the child with `wait4`, which also reports its peak RSS (`ru_maxrss`, KiB).
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod sys {
    use super::{poll_sleep, timed_out, Waited};
    use std::process::Child;
    use std::time::{Duration, Instant};

    #[repr(C)]
    struct Rusage {
        ru_utime: [i64; 2],
        ru_stime: [i64; 2],
        ru_maxrss: i64,
        rest: [i64; 13],
    }

    extern "C" {
        fn wait4(pid: i32, status: *mut i32, options: i32, rusage: *mut Rusage) -> i32;
    }

    const WNOHANG: i32 = 1;
    const EINTR: i32 = 4;

    fn wait4_once(pid: i32, options: i32) -> Option<Option<(i32, i64)>> {
        let mut status = 0i32;
        let mut ru = Rusage {
            ru_utime: [0; 2],
            ru_stime: [0; 2],
            ru_maxrss: 0,
            rest: [0; 13],
        };
        // SAFETY: valid pointers to locals; `pid` is our own child.
        let r = unsafe { wait4(pid, &mut status, options, &mut ru) };
        match r {
            0 => Some(None),
            r if r < 0 => {
                let interrupted = std::io::Error::last_os_error().raw_os_error() == Some(EINTR);
                interrupted.then_some(None)
            }
            _ => {
                let _ = (ru.ru_utime, ru.ru_stime, ru.rest);
                Some(Some((status, ru.ru_maxrss)))
            }
        }
    }

    fn decode(status: i32, maxrss_kib: i64, timed_out: bool) -> Waited {
        let sig = status & 0x7f;
        let (exit_code, signal) = if sig == 0 {
            (Some((status >> 8) & 0xff), None)
        } else {
            (Some(128 + sig), Some(sig))
        };
        Waited {
            exit_code,
            signal: if timed_out { None } else { signal },
            timed_out,
            peak_bytes: (maxrss_kib > 0).then_some(maxrss_kib as u64 * 1024),
        }
    }

    pub(super) fn wait(child: &mut Child, timeout: Option<Duration>, start: Instant) -> Waited {
        let pid = child.id() as i32;
        loop {
            match wait4_once(pid, WNOHANG) {
                Some(Some((status, rss))) => return decode(status, rss, false),
                Some(None) => {}
                None => return fallback(child, timeout, start),
            }
            if timed_out(start, timeout) {
                let _ = child.kill();
                loop {
                    match wait4_once(pid, 0) {
                        Some(Some((status, rss))) => return decode(status, rss, true),
                        Some(None) => continue,
                        None => {
                            return Waited {
                                exit_code: None,
                                signal: None,
                                timed_out: true,
                                peak_bytes: None,
                            }
                        }
                    }
                }
            }
            poll_sleep(start);
        }
    }

    fn fallback(child: &mut Child, timeout: Option<Duration>, start: Instant) -> Waited {
        super::generic_wait(child, timeout, start, |_| None)
    }
}

/// Windows: poll the process handle; peak working set via `K32GetProcessMemoryInfo`.
#[cfg(windows)]
mod sys {
    use super::Waited;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::time::{Duration, Instant};

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    fn peak(child: &Child) -> Option<u64> {
        let mut c = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        let cb = c.cb;
        // SAFETY: the handle is owned by `child` and still open; `c` is a valid out-pointer.
        let ok = unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &mut c, cb) };
        let _ = (
            c.page_fault_count,
            c.working_set_size,
            c.quota_peak_paged_pool_usage,
            c.quota_paged_pool_usage,
            c.quota_peak_non_paged_pool_usage,
            c.quota_non_paged_pool_usage,
            c.pagefile_usage,
            c.peak_pagefile_usage,
        );
        (ok != 0 && c.peak_working_set_size > 0).then_some(c.peak_working_set_size as u64)
    }

    pub(super) fn wait(child: &mut Child, timeout: Option<Duration>, start: Instant) -> Waited {
        super::generic_wait(child, timeout, start, peak)
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_pointer_width = "64"))))]
mod sys {
    use super::Waited;
    use std::process::Child;
    use std::time::{Duration, Instant};

    pub(super) fn wait(child: &mut Child, timeout: Option<Duration>, start: Instant) -> Waited {
        super::generic_wait(child, timeout, start, |_| None)
    }
}

/// Portable wait loop with timeout. `sample` reads peak memory from the live/exited process.
#[allow(dead_code)]
fn generic_wait(
    child: &mut Child,
    timeout: Option<Duration>,
    start: Instant,
    sample: fn(&Child) -> Option<u64>,
) -> Waited {
    let mut peak: Option<u64> = None;
    let take = |child: &Child, peak: &mut Option<u64>| {
        if let Some(p) = sample(child) {
            *peak = Some(peak.map_or(p, |q| q.max(p)));
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                take(child, &mut peak);
                return Waited {
                    exit_code: status.code(),
                    signal: None,
                    timed_out: false,
                    peak_bytes: peak,
                };
            }
            Ok(None) => {}
            Err(_) => {
                return Waited {
                    exit_code: None,
                    signal: None,
                    timed_out: false,
                    peak_bytes: peak,
                }
            }
        }
        take(child, &mut peak);
        if timed_out(start, timeout) {
            let _ = child.kill();
            let status = child.wait().ok();
            take(child, &mut peak);
            return Waited {
                exit_code: status.and_then(|s| s.code()),
                signal: None,
                timed_out: true,
                peak_bytes: peak,
            };
        }
        poll_sleep(start);
    }
}

/// Replaces `{run_dir}`, `{seed}`, `{run_id}`, `{cache_dir}` and `{config}` in one argument.
pub fn expand(arg: &str, vars: &[(&str, String)]) -> String {
    let mut out = arg.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_expand() {
        let vars = [
            ("run_dir", "/w/run-1".to_string()),
            ("seed", "7".to_string()),
        ];
        assert_eq!(expand("--out={run_dir}", &vars), "--out=/w/run-1");
        assert_eq!(expand("{seed}{seed}", &vars), "77");
        assert_eq!(expand("{unknown}", &vars), "{unknown}");
    }
}
