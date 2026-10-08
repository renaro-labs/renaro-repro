//! Likely causes of a difference, derived only from evidence in the differing bytes/files.

use crate::compare::{FileCmp, FileStatus, Localization};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hint {
    pub id: String,
    pub cause: String,
    pub fix: String,
    pub evidence: String,
}

/// What the hints may compare the differing content against.
#[derive(Default)]
pub struct HintContext {
    /// Run directories of side A and side B (absolute paths).
    pub run_dirs: [String; 2],
    pub run_ids: [String; 2],
    pub pids: [u32; 2],
    pub hostname: String,
}

/// Order in which hints are reported (most specific first).
const ORDER: [&str; 13] = [
    "run_dir",
    "timestamp",
    "uuid",
    "pid",
    "run_id",
    "hostname",
    "abs_path",
    "order",
    "json_key_order",
    "float_last_digits",
    "line_ending",
    "file_set",
    "other",
];

pub fn derive(files: &[FileCmp], ctx: &HintContext) -> Vec<Hint> {
    let mut hints: Vec<Hint> = Vec::new();
    let mut add = |id: &str, cause: String, fix: &str, evidence: String| {
        if !hints.iter().any(|h| h.id == id) {
            hints.push(Hint {
                id: id.to_string(),
                cause,
                fix: fix.to_string(),
                evidence,
            });
        }
    };

    let one_sided: Vec<&FileCmp> = files
        .iter()
        .filter(|f| matches!(f.status, FileStatus::OnlyInA | FileStatus::OnlyInB))
        .collect();
    if !one_sided.is_empty() {
        add(
            "file_set",
            "output file names differ between runs".into(),
            "do not put timestamps, PIDs or random ids in file names",
            format!(
                "{} file(s) exist in one run only, e.g. {}",
                one_sided.len(),
                one_sided[0].path
            ),
        );
    }

    for f in files.iter().filter(|f| f.status == FileStatus::Different) {
        let Some(loc) = &f.localization else { continue };
        let (a, b) = texts(loc);
        let at = |what: &str| format!("{} line {}: {what}", f.path, loc.line.unwrap_or(0));

        if !ctx.run_dirs[0].is_empty()
            && (contains_path(&a, &ctx.run_dirs[0]) || contains_path(&b, &ctx.run_dirs[1]))
        {
            add(
                "run_dir",
                "the run directory path is written into an output".into(),
                "write paths relative to REPRO_RUN_DIR (or drop them)",
                at("contains the run directory"),
            );
        }
        let (ta, tb) = (timestamps(&a), timestamps(&b));
        if ta != tb {
            let ex = ta.first().or(tb.first()).cloned().unwrap_or_default();
            add(
                "timestamp",
                "a timestamp is written into an output".into(),
                "remove timestamps from outputs (log them to stderr), or declare a [[normalize]] rule for that field",
                at(&format!("{ex:?}")),
            );
        }
        let (ua, ub) = (uuids(&a), uuids(&b));
        if ua != ub {
            add(
                "uuid",
                "random UUIDs are written into an output".into(),
                "derive ids from the seed, e.g. uuid.UUID(int=random.Random(seed).getrandbits(128))",
                at(ua.first().or(ub.first()).map_or("", |s| s.as_str())),
            );
        }
        if ctx.pids[0] != 0
            && has_number(&a, ctx.pids[0] as u64)
            && has_number(&b, ctx.pids[1] as u64)
        {
            add(
                "pid",
                "a process id is written into an output".into(),
                "do not write os.getpid() (or thread ids) into outputs",
                at(&format!("PIDs {} and {}", ctx.pids[0], ctx.pids[1])),
            );
        }
        if !ctx.run_ids[0].is_empty()
            && (a.contains(&ctx.run_ids[0]) || b.contains(&ctx.run_ids[1]))
        {
            add(
                "run_id",
                "the run id is written into an output".into(),
                "keep run ids in logs only, or drop the field with a [[normalize]] rule",
                at(&format!("contains {}", ctx.run_ids[0])),
            );
        }
        if ctx.hostname.len() >= 2
            && (a.to_lowercase().contains(&ctx.hostname.to_lowercase())
                || b.to_lowercase().contains(&ctx.hostname.to_lowercase()))
        {
            add(
                "hostname",
                "the host name is written into an output".into(),
                "do not write socket.gethostname() into outputs; it breaks fresh-machine reproduction",
                at(&format!("contains {:?}", ctx.hostname)),
            );
        }
        let (pa, pb) = (abs_paths(&a), abs_paths(&b));
        if pa != pb {
            add(
                "abs_path",
                "an absolute path is written into an output".into(),
                "write paths relative to the project or run directory",
                at(pa.first().or(pb.first()).map_or("", |s| s.as_str())),
            );
        }
        if loc.same_records_different_order == Some(true) {
            add(
                "order",
                "same records, different order (set/dict iteration or parallel workers)".into(),
                "sort records before writing; set PYTHONHASHSEED=0; never write in set or unordered-dict iteration order",
                format!("{}: identical multiset of lines in a different order", f.path),
            );
        } else if loc.parsed_equal == Some(true) {
            add(
                "json_key_order",
                "same JSON data, different key order or formatting".into(),
                "serialize with sorted keys (json.dump(..., sort_keys=True))",
                format!("{}: parsed values are equal", f.path),
            );
        }
        if let Some(n) = loc.numeric.iter().find(|n| {
            n.differing > 0
                && n.non_numeric_diffs == 0
                && n.max_rel_diff > 0.0
                && n.max_rel_diff < 1e-6
        }) {
            add(
                "float_last_digits",
                "floats differ only in the last digits (thread count, GPU kernels or reduction order)".into(),
                "set OMP_NUM_THREADS=1, torch.use_deterministic_algorithms(True), CUBLAS_WORKSPACE_CONFIG=:4096:8, DataLoader(num_workers=0)",
                format!(
                    "{}: field {} max relative diff {:.1e}",
                    f.path, n.field, n.max_rel_diff
                ),
            );
        }
        if loc.line_ending_only {
            add(
                "line_ending",
                "only the line ending differs (CRLF vs LF)".into(),
                "open output files with newline=\"\\n\" so Windows and Linux write the same bytes",
                at("CRLF vs LF"),
            );
        }
    }
    hints.sort_by_key(|h| ORDER.iter().position(|o| *o == h.id).unwrap_or(ORDER.len()));
    hints
}

/// Text of the first differing region on each side: the differing line, else the byte context.
fn texts(loc: &Localization) -> (String, String) {
    let ascii = |c: &Option<String>| {
        c.as_deref()
            .and_then(|s| s.split_once("  |"))
            .map(|(_, a)| a.trim_end_matches('|').to_string())
            .unwrap_or_default()
    };
    let a = loc.line_a.clone().unwrap_or_else(|| ascii(&loc.context_a));
    let b = loc.line_b.clone().unwrap_or_else(|| ascii(&loc.context_b));
    (a, b)
}

fn contains_path(text: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    // Also match JSON-escaped Windows paths (`C:\\x`) and forward-slash variants.
    text.contains(path)
        || text.contains(&path.replace('\\', "\\\\"))
        || text.contains(&path.replace('\\', "/"))
}

fn is_digit(b: Option<&u8>) -> bool {
    b.is_some_and(|c| c.is_ascii_digit())
}

/// ISO 8601 date-times (`2026-10-08T15:01:02`, `2026-10-08 15:01`) and epoch seconds/ms.
pub fn timestamps(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // ISO: dddd-dd-dd[T ]dd:dd
        if i + 16 <= b.len()
            && (i == 0 || !b[i - 1].is_ascii_digit())
            && b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'-'
            && b[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && b[i + 7] == b'-'
            && b[i + 8..i + 10].iter().all(u8::is_ascii_digit)
            && (b[i + 10] == b'T' || b[i + 10] == b' ')
            && b[i + 11..i + 13].iter().all(u8::is_ascii_digit)
            && b[i + 13] == b':'
            && b[i + 14..i + 16].iter().all(u8::is_ascii_digit)
        {
            let mut j = i + 16;
            while j < b.len() && (b[j].is_ascii_digit() || b":.+-Z".contains(&b[j])) {
                j += 1;
            }
            out.push(s[i..j].to_string());
            i = j;
            continue;
        }
        // Epoch seconds (10 digits) or milliseconds (13 digits) in the plausible range.
        if b[i].is_ascii_digit() && (i == 0 || !is_digit(b.get(i - 1))) {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let run = &s[i..j];
            let plausible = match run.len() {
                10 => run.as_bytes()[0] == b'1' || run.starts_with("20"),
                13 => run.as_bytes()[0] == b'1' || run.starts_with("20"),
                _ => false,
            };
            if plausible {
                out.push(run.to_string());
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// 8-4-4-4-12 hex UUIDs.
pub fn uuids(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    if b.len() < 36 {
        return out;
    }
    let mut i = 0;
    while i + 36 <= b.len() {
        let w = &b[i..i + 36];
        let ok = w.iter().enumerate().all(|(k, c)| {
            if [8, 13, 18, 23].contains(&k) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
        let bounded = (i == 0 || !b[i - 1].is_ascii_hexdigit())
            && (i + 36 == b.len() || !b[i + 36].is_ascii_hexdigit());
        if ok && bounded {
            out.push(s[i..i + 36].to_string());
            i += 36;
        } else {
            i += 1;
        }
    }
    out
}

/// Absolute paths: Unix roots like `/home/`, `/tmp/`, or Windows drive paths `C:\`.
pub fn abs_paths(s: &str) -> Vec<String> {
    const ROOTS: [&str; 8] = [
        "/home/",
        "/tmp/",
        "/Users/",
        "/root/",
        "/mnt/",
        "/var/",
        "/scratch/",
        "/data/",
    ];
    let b = s.as_bytes();
    let mut out = Vec::new();
    let end_of = |start: usize| {
        let mut j = start;
        while j < b.len() && !b" \t\"',;)]}\r\n".contains(&b[j]) {
            j += 1;
        }
        j
    };
    let mut i = 0;
    while i < b.len() {
        let prev_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if prev_ok && ROOTS.iter().any(|r| b[i..].starts_with(r.as_bytes())) {
            let j = end_of(i);
            out.push(s[i..j].to_string());
            i = j;
            continue;
        }
        if prev_ok
            && i + 3 <= b.len()
            && b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && (b[i + 2] == b'\\' || b[i + 2] == b'/')
        {
            let j = end_of(i);
            out.push(s[i..j].to_string());
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

fn has_number(s: &str, n: u64) -> bool {
    let needle = n.to_string();
    let b = s.as_bytes();
    let mut start = 0;
    while let Some(pos) = s[start..].find(&needle) {
        let i = start + pos;
        let j = i + needle.len();
        if (i == 0 || !b[i - 1].is_ascii_digit()) && (j == b.len() || !b[j].is_ascii_digit()) {
            return true;
        }
        start = i + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_timestamps() {
        assert_eq!(
            timestamps("written_at=2026-10-08T15:01:02.123456Z ok"),
            vec!["2026-10-08T15:01:02.123456Z"]
        );
        assert_eq!(timestamps("t 1728400000.5"), vec!["1728400000"]);
        assert_eq!(timestamps("ms=1728400000123"), vec!["1728400000123"]);
        assert!(timestamps("acc=0.8123 step=12").is_empty());
    }

    #[test]
    fn finds_uuids_and_paths() {
        assert_eq!(
            uuids("id=0f8fad5b-d9cb-469f-a165-70867728950e,"),
            vec!["0f8fad5b-d9cb-469f-a165-70867728950e"]
        );
        assert_eq!(
            abs_paths("{\"p\":\"/home/ana/run-1/x.csv\"}"),
            vec!["/home/ana/run-1/x.csv"]
        );
        assert_eq!(abs_paths(r"out=C:\work\run-2"), vec![r"C:\work\run-2"]);
        assert!(abs_paths("ratio 3/4").is_empty());
    }

    #[test]
    fn number_match_is_bounded() {
        assert!(has_number("pid=4242;", 4242));
        assert!(!has_number("pid=142424", 4242));
    }
}
