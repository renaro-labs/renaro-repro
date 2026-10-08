//! Comparing file sets and localizing differences (byte, line, record, key, Parquet).

use crate::normalize::{self, Normalizer};
use crate::util;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Normalized copies above this size are not built in memory for localization.
pub const MEM_CAP: u64 = 64 << 20;
const LINE_CAP: usize = 64 * 1024;
const MAX_DIFF_LINES: usize = 40;
const MAX_SCAN_LINES: u64 = 1_000_000;
const MAX_MULTISET_LINES: usize = 2_000_000;

/// One file as seen in a run, a directory or a manifest.
#[derive(Debug, Clone)]
pub struct FileRec {
    pub path: String,
    pub size: u64,
    pub blake3: String,
    pub normalized_blake3: Option<String>,
    /// Indices of `[[normalize]]` rules that apply.
    pub rules: Vec<usize>,
    /// Location on disk (None for manifest entries).
    pub abs: Option<PathBuf>,
}

impl FileRec {
    pub fn cmp_hash(&self) -> &str {
        self.normalized_blake3.as_deref().unwrap_or(&self.blake3)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Identical,
    IdenticalAfterNormalization,
    Different,
    OnlyInA,
    OnlyInB,
}

impl FileStatus {
    pub fn is_match(self) -> bool {
        matches!(
            self,
            FileStatus::Identical | FileStatus::IdenticalAfterNormalization
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            FileStatus::Identical => "identical",
            FileStatus::IdenticalAfterNormalization => "identical after normalization",
            FileStatus::Different => "different",
            FileStatus::OnlyInA => "only in A",
            FileStatus::OnlyInB => "only in B",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileCmp {
    pub path: String,
    pub status: FileStatus,
    pub size_a: Option<u64>,
    pub size_b: Option<u64>,
    pub blake3_a: Option<String>,
    pub blake3_b: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localization: Option<Localization>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KeyDiff {
    pub key: String,
    pub a: Option<String>,
    pub b: Option<String>,
}

/// Per-field statistics over numeric cells that differ (CSV column, JSON key path).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NumericField {
    pub field: String,
    pub differing: u64,
    pub max_abs_diff: f64,
    pub max_rel_diff: f64,
    pub non_numeric_diffs: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ParquetDiff {
    pub rows_a: i64,
    pub rows_b: i64,
    pub schema_differs: bool,
    pub columns: Vec<String>,
    pub row_groups: Vec<usize>,
    pub first_row: Option<u64>,
    pub first_row_a: Option<String>,
    pub first_row_b: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Localization {
    pub kind: String,
    /// The diff below is of the normalized content.
    pub normalized: bool,
    pub byte_offset: Option<u64>,
    pub context_start: Option<u64>,
    pub context_a: Option<String>,
    pub context_b: Option<String>,
    /// 1-based number of the first differing line.
    pub line: Option<u64>,
    pub line_a: Option<String>,
    pub line_b: Option<String>,
    pub line_ending_only: bool,
    pub unified_diff: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub record_diff: Vec<KeyDiff>,
    pub json_path: Option<String>,
    /// The differing JSON values parse to the same data (key order / formatting differ).
    pub parsed_equal: Option<bool>,
    pub same_records_different_order: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub numeric: Vec<NumericField>,
    pub parquet: Option<ParquetDiff>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Content to localize: a file on disk or a normalized copy in memory.
pub enum Src {
    File(PathBuf),
    Mem(Vec<u8>),
}

impl Src {
    fn reader(&self) -> Result<Box<dyn BufRead + '_>> {
        Ok(match self {
            Src::File(p) => Box::new(BufReader::with_capacity(
                1 << 16,
                File::open(p).with_context(|| format!("cannot open {}", p.display()))?,
            )),
            Src::Mem(b) => Box::new(Cursor::new(b.as_slice())),
        })
    }

    fn read_range(&self, start: u64, len: usize) -> Result<Vec<u8>> {
        match self {
            Src::File(p) => {
                let mut f = File::open(p)?;
                f.seek(SeekFrom::Start(start))?;
                let mut out = Vec::with_capacity(len);
                f.take(len as u64).read_to_end(&mut out)?;
                Ok(out)
            }
            Src::Mem(b) => {
                let s = (start as usize).min(b.len());
                let e = (s + len).min(b.len());
                Ok(b[s..e].to_vec())
            }
        }
    }

    fn head(&self, n: usize) -> Vec<u8> {
        self.read_range(0, n).unwrap_or_default()
    }

    fn path(&self) -> Option<&PathBuf> {
        match self {
            Src::File(p) => Some(p),
            Src::Mem(_) => None,
        }
    }
}

/// Merge-compares two file lists by path. Results are sorted by path.
pub fn compare_sets(a: &[FileRec], b: &[FileRec]) -> Vec<FileCmp> {
    let am: BTreeMap<&str, &FileRec> = a.iter().map(|f| (f.path.as_str(), f)).collect();
    let bm: BTreeMap<&str, &FileRec> = b.iter().map(|f| (f.path.as_str(), f)).collect();
    let paths: BTreeSet<&str> = am.keys().chain(bm.keys()).copied().collect();
    paths
        .into_iter()
        .map(|p| {
            let fa = am.get(p);
            let fb = bm.get(p);
            let status = match (fa, fb) {
                (Some(x), Some(y)) => {
                    if x.blake3 == y.blake3 {
                        FileStatus::Identical
                    } else if x.cmp_hash() == y.cmp_hash() {
                        FileStatus::IdenticalAfterNormalization
                    } else {
                        FileStatus::Different
                    }
                }
                (Some(_), None) => FileStatus::OnlyInA,
                _ => FileStatus::OnlyInB,
            };
            let rules = fa.or(fb).map(|f| f.rules.clone()).unwrap_or_default();
            FileCmp {
                path: p.to_string(),
                status,
                size_a: fa.map(|f| f.size),
                size_b: fb.map(|f| f.size),
                blake3_a: fa.map(|f| f.blake3.clone()),
                blake3_b: fb.map(|f| f.blake3.clone()),
                rules,
                localization: None,
            }
        })
        .collect()
}

/// Compares and localizes the first `max_localize` differing files that exist on disk.
pub fn compare_and_localize(
    a: &[FileRec],
    b: &[FileRec],
    normalizer: Option<&Normalizer>,
    max_localize: usize,
) -> Vec<FileCmp> {
    let mut cmps = compare_sets(a, b);
    let am: BTreeMap<&str, &FileRec> = a.iter().map(|f| (f.path.as_str(), f)).collect();
    let bm: BTreeMap<&str, &FileRec> = b.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut done = 0;
    for c in cmps.iter_mut() {
        if c.status != FileStatus::Different || done >= max_localize {
            continue;
        }
        let (Some(fa), Some(fb)) = (am.get(c.path.as_str()), bm.get(c.path.as_str())) else {
            continue;
        };
        let (Some(pa), Some(pb)) = (&fa.abs, &fb.abs) else {
            continue;
        };
        done += 1;
        let mut notes = Vec::new();
        let (sa, sb, normalized) = match normalizer {
            Some(n) if !fa.rules.is_empty() => {
                let na = n.normalized_bytes(pa, &c.path, &fa.rules, MEM_CAP);
                let nb = n.normalized_bytes(pb, &c.path, &fb.rules, MEM_CAP);
                match (na, nb) {
                    (Ok(Some(x)), Ok(Some(y))) => (Src::Mem(x), Src::Mem(y), true),
                    _ => {
                        notes.push(
                            "normalized copy too large to localize; showing the raw difference"
                                .to_string(),
                        );
                        (Src::File(pa.clone()), Src::File(pb.clone()), false)
                    }
                }
            }
            _ => (Src::File(pa.clone()), Src::File(pb.clone()), false),
        };
        let mut loc = localize(&c.path, &sa, &sb);
        loc.normalized = normalized;
        notes.append(&mut loc.notes);
        loc.notes = notes;
        c.localization = Some(loc);
    }
    cmps
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileKind {
    Binary,
    Text,
    Csv(u8),
    Jsonl,
    Json,
    Parquet,
}

impl FileKind {
    fn name(self) -> &'static str {
        match self {
            FileKind::Binary => "binary",
            FileKind::Text => "text",
            FileKind::Csv(_) => "csv",
            FileKind::Jsonl => "jsonl",
            FileKind::Json => "json",
            FileKind::Parquet => "parquet",
        }
    }
}

const TEXT_EXT: [&str; 16] = [
    "txt", "log", "md", "yaml", "yml", "toml", "xml", "html", "htm", "py", "ini", "cfg", "tex",
    "rst", "sh", "out",
];

fn detect_kind(rel: &str, a: &Src) -> FileKind {
    let lower = rel.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    if ext == "parquet" || ext == "pq" {
        return FileKind::Parquet;
    }
    match normalize::kind_of(rel) {
        normalize::Kind::Jsonl => return FileKind::Jsonl,
        normalize::Kind::Json => return FileKind::Json,
        normalize::Kind::Csv(d) => return FileKind::Csv(d),
        normalize::Kind::Text => {}
    }
    if TEXT_EXT.contains(&ext) {
        return FileKind::Text;
    }
    let head = a.head(8192);
    if !head.contains(&0) && std::str::from_utf8(&head).is_ok() {
        FileKind::Text
    } else {
        FileKind::Binary
    }
}

/// Localizes the first difference between two versions of one file.
pub fn localize(rel: &str, a: &Src, b: &Src) -> Localization {
    let kind = detect_kind(rel, a);
    let mut loc = Localization {
        kind: kind.name().to_string(),
        ..Default::default()
    };
    if let Err(e) = localize_into(rel, kind, a, b, &mut loc) {
        loc.notes.push(format!("localization stopped: {e:#}"));
    }
    loc
}

fn localize_into(
    _rel: &str,
    kind: FileKind,
    a: &Src,
    b: &Src,
    loc: &mut Localization,
) -> Result<()> {
    if let Some(off) = first_diff_offset(a, b)? {
        let start = off.saturating_sub(16);
        loc.byte_offset = Some(off);
        loc.context_start = Some(start);
        loc.context_a = Some(hex_context(&a.read_range(start, 32)?));
        loc.context_b = Some(hex_context(&b.read_range(start, 32)?));
    }
    match kind {
        FileKind::Binary => {}
        FileKind::Parquet => parquet_localize(a, b, loc),
        FileKind::Text | FileKind::Csv(_) | FileKind::Jsonl | FileKind::Json => {
            text_localize(kind, a, b, loc)?
        }
    }
    Ok(())
}

#[cfg(feature = "parquet")]
fn parquet_localize(a: &Src, b: &Src, loc: &mut Localization) {
    match (a.path(), b.path()) {
        (Some(pa), Some(pb)) => match parquet_diff(pa, pb) {
            Ok(d) => loc.parquet = Some(d),
            Err(e) => loc.notes.push(format!("cannot read Parquet: {e:#}")),
        },
        _ => loc
            .notes
            .push("Parquet diff needs the files on disk".to_string()),
    }
}

#[cfg(not(feature = "parquet"))]
fn parquet_localize(a: &Src, b: &Src, loc: &mut Localization) {
    let _ = (a.path(), b.path());
    loc.notes.push(
        "built without the `parquet` feature: only the byte offset is reported \
         (build with `--features parquet` for column, row-group and row diffs)"
            .to_string(),
    );
}

fn first_diff_offset(a: &Src, b: &Src) -> Result<Option<u64>> {
    let mut ra = a.reader()?;
    let mut rb = b.reader()?;
    let mut off = 0u64;
    loop {
        let ba = ra.fill_buf()?;
        let bb = rb.fill_buf()?;
        if ba.is_empty() && bb.is_empty() {
            return Ok(None);
        }
        if ba.is_empty() || bb.is_empty() {
            return Ok(Some(off));
        }
        let n = ba.len().min(bb.len());
        if let Some(i) = ba[..n].iter().zip(&bb[..n]).position(|(x, y)| x != y) {
            return Ok(Some(off + i as u64));
        }
        ra.consume(n);
        rb.consume(n);
        off += n as u64;
    }
}

/// `48 65 6c 6c 6f  |Hello|` style dump.
pub fn hex_context(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{}  |{}|", hex.join(" "), ascii)
}

/// A line with its terminator; very long lines keep a prefix plus a hash of the rest.
#[derive(PartialEq, Clone)]
struct Line {
    head: Vec<u8>,
    rest: Option<blake3::Hash>,
}

fn read_line(r: &mut dyn BufRead) -> Result<Option<Line>> {
    let mut head = Vec::new();
    let mut rest: Option<blake3::Hasher> = None;
    let mut any = false;
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        any = true;
        let (chunk, done) = match buf.iter().position(|&c| c == b'\n') {
            Some(i) => (&buf[..=i], true),
            None => (buf, false),
        };
        let room = LINE_CAP.saturating_sub(head.len());
        if chunk.len() <= room {
            head.extend_from_slice(chunk);
        } else {
            head.extend_from_slice(&chunk[..room]);
            rest.get_or_insert_with(blake3::Hasher::new)
                .update(&chunk[room..]);
        }
        let n = chunk.len();
        r.consume(n);
        if done {
            break;
        }
    }
    Ok(any.then(|| Line {
        head,
        rest: rest.map(|h| h.finalize()),
    }))
}

/// Renders a line for display: `\r` made visible, missing final newline marked.
fn render_line(l: &[u8]) -> String {
    let s = String::from_utf8_lossy(l);
    let (body, nl) = match s.strip_suffix('\n') {
        Some(t) => (t, true),
        None => (&s[..], false),
    };
    let (body, cr) = match body.strip_suffix('\r') {
        Some(t) => (t, "\\r"),
        None => (body, ""),
    };
    let marker = if nl {
        ""
    } else {
        " [no newline at end of file]"
    };
    format!("{}{cr}{marker}\n", util::truncate(body, 300))
}

fn render_lines(ls: &[&Line]) -> String {
    ls.iter().map(|l| render_line(&l.head)).collect()
}

/// The first differing line plus up to 29 following lines.
fn collect_after(first: Option<Line>, r: &mut dyn BufRead) -> Result<Vec<Line>> {
    let mut v: Vec<Line> = first.into_iter().collect();
    while v.len() < 30 {
        match read_line(r)? {
            Some(l) => v.push(l),
            None => break,
        }
    }
    Ok(v)
}

fn content_of(l: &[u8]) -> &[u8] {
    normalize::split_terminator(l).0
}

fn text_localize(kind: FileKind, a: &Src, b: &Src, loc: &mut Localization) -> Result<()> {
    let mut ra = a.reader()?;
    let mut rb = b.reader()?;
    let mut before: VecDeque<Line> = VecDeque::new();
    let mut n = 0u64;
    let (la, lb) = loop {
        let la = read_line(ra.as_mut())?;
        let lb = read_line(rb.as_mut())?;
        n += 1;
        if la.is_none() && lb.is_none() {
            return Ok(());
        }
        if la == lb {
            if let Some(l) = la {
                before.push_back(l);
                if before.len() > 3 {
                    before.pop_front();
                }
            }
            continue;
        }
        break (la, lb);
    };
    loc.line = Some(n);
    let raw_a = la.as_ref().map(|l| l.head.clone());
    let raw_b = lb.as_ref().map(|l| l.head.clone());
    loc.line_a = raw_a
        .as_deref()
        .map(|l| util::truncate(&render_line(l), 4000));
    loc.line_b = raw_b
        .as_deref()
        .map(|l| util::truncate(&render_line(l), 4000));
    if let (Some(x), Some(y)) = (&raw_a, &raw_b) {
        loc.line_ending_only = x != y && content_of(x) == content_of(y);
    }

    let after_a = collect_after(la, ra.as_mut())?;
    let after_b = collect_after(lb, rb.as_mut())?;
    let start_line = n - before.len() as u64;
    let old = render_lines(&before.iter().chain(after_a.iter()).collect::<Vec<_>>());
    let new = render_lines(&before.iter().chain(after_b.iter()).collect::<Vec<_>>());
    let diff = similar::TextDiff::from_lines(&old, &new);
    let ha = format!("run A (from line {start_line})");
    let hb = format!("run B (from line {start_line})");
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(&ha, &hb)
        .to_string();
    let lines: Vec<&str> = text.lines().collect();
    let mut shown = lines[..lines.len().min(MAX_DIFF_LINES)].join("\n");
    if lines.len() > MAX_DIFF_LINES {
        shown.push_str(&format!(
            "\n… ({} more diff lines)",
            lines.len() - MAX_DIFF_LINES
        ));
    }
    loc.unified_diff = Some(shown);

    // JSONL: per-key diff of the first differing record.
    if kind == FileKind::Jsonl {
        if let (Some(x), Some(y)) = (&raw_a, &raw_b) {
            let pa = serde_json::from_slice::<Value>(content_of(x));
            let pb = serde_json::from_slice::<Value>(content_of(y));
            if let (Ok(va), Ok(vb)) = (pa, pb) {
                loc.parsed_equal = Some(va == vb);
                loc.record_diff = record_diff(&va, &vb);
            }
        }
    }
    // JSON: path of the first differing key.
    if kind == FileKind::Json {
        let ba = a.read_range(0, MEM_CAP as usize)?;
        let bb = b.read_range(0, MEM_CAP as usize)?;
        if let (Ok(va), Ok(vb)) = (
            serde_json::from_slice::<Value>(&ba),
            serde_json::from_slice::<Value>(&bb),
        ) {
            loc.parsed_equal = Some(va == vb);
            loc.json_path = first_json_diff(&va, &vb, "$");
        }
    }
    if matches!(kind, FileKind::Text | FileKind::Csv(_) | FileKind::Jsonl) {
        loc.same_records_different_order = same_multiset(a, b)?;
    }
    if matches!(kind, FileKind::Csv(_) | FileKind::Jsonl | FileKind::Json) {
        loc.numeric = numeric_scan(kind, a, b)?;
    }
    Ok(())
}

fn compact(v: &Value) -> String {
    util::truncate(&v.to_string(), 200)
}

pub fn record_diff(a: &Value, b: &Value) -> Vec<KeyDiff> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            keys.into_iter()
                .filter(|k| x.get(*k) != y.get(*k))
                .map(|k| KeyDiff {
                    key: k.clone(),
                    a: x.get(k).map(compact),
                    b: y.get(k).map(compact),
                })
                .collect()
        }
        _ if a == b => Vec::new(),
        _ => vec![KeyDiff {
            key: "$".to_string(),
            a: Some(compact(a)),
            b: Some(compact(b)),
        }],
    }
}

/// Path (`$.a.b[3]`) of the first differing value, keys visited in sorted order.
pub fn first_json_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let p = format!("{path}.{k}");
                match (x.get(k), y.get(k)) {
                    (Some(va), Some(vb)) => {
                        if va != vb {
                            return first_json_diff(va, vb, &p);
                        }
                    }
                    _ => return Some(p),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            for i in 0..x.len().max(y.len()) {
                let p = format!("{path}[{i}]");
                match (x.get(i), y.get(i)) {
                    (Some(va), Some(vb)) => {
                        if va != vb {
                            return first_json_diff(va, vb, &p);
                        }
                    }
                    _ => return Some(p),
                }
            }
            None
        }
        _ => (a != b).then(|| path.to_string()),
    }
}

/// `Some(true)` when both files hold the same multiset of lines (only the order differs).
fn same_multiset(a: &Src, b: &Src) -> Result<Option<bool>> {
    let hashes = |s: &Src| -> Result<Option<Vec<[u8; 32]>>> {
        let mut r = s.reader()?;
        let mut v = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            // Ignore a missing final newline so the last record can move.
            let line = if buf.ends_with(b"\n") {
                &buf[..]
            } else {
                buf.push(b'\n');
                &buf[..]
            };
            v.push(*blake3::hash(line).as_bytes());
            if v.len() > MAX_MULTISET_LINES {
                return Ok(None);
            }
        }
        v.sort_unstable();
        Ok(Some(v))
    };
    match (hashes(a)?, hashes(b)?) {
        (Some(x), Some(y)) => Ok(Some(x == y)),
        _ => Ok(None),
    }
}

#[derive(Default)]
struct NumAcc {
    differing: u64,
    max_abs: f64,
    max_rel: f64,
    non_numeric: u64,
}

impl NumAcc {
    fn add(&mut self, a: &str, b: &str) {
        match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            (Ok(x), Ok(y)) if x.is_finite() && y.is_finite() => {
                let d = (x - y).abs();
                let scale = x.abs().max(y.abs());
                let rel = if scale > 0.0 { d / scale } else { 0.0 };
                self.differing += 1;
                self.max_abs = self.max_abs.max(d);
                self.max_rel = self.max_rel.max(rel);
            }
            _ => self.non_numeric += 1,
        }
    }
}

fn leaf_diffs(a: &Value, b: &Value, path: &str, acc: &mut BTreeMap<String, NumAcc>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match (x.get(k), y.get(k)) {
                    (Some(va), Some(vb)) => leaf_diffs(va, vb, &p, acc),
                    _ => acc.entry(p).or_default().non_numeric += 1,
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            let p = format!("{path}[]");
            for (va, vb) in x.iter().zip(y) {
                leaf_diffs(va, vb, &p, acc);
            }
        }
        _ if a == b => {}
        (Value::Number(x), Value::Number(y)) => {
            acc.entry(path.to_string())
                .or_default()
                .add(&x.to_string(), &y.to_string());
        }
        _ => acc.entry(path.to_string()).or_default().non_numeric += 1,
    }
}

/// Per-field numeric difference statistics for CSV columns or JSON key paths.
fn numeric_scan(kind: FileKind, a: &Src, b: &Src) -> Result<Vec<NumericField>> {
    let mut acc: BTreeMap<String, NumAcc> = BTreeMap::new();
    match kind {
        FileKind::Json => {
            let ba = a.read_range(0, MEM_CAP as usize)?;
            let bb = b.read_range(0, MEM_CAP as usize)?;
            if let (Ok(va), Ok(vb)) = (
                serde_json::from_slice::<Value>(&ba),
                serde_json::from_slice::<Value>(&bb),
            ) {
                leaf_diffs(&va, &vb, "", &mut acc);
            }
        }
        FileKind::Csv(_) | FileKind::Jsonl => {
            let mut ra = a.reader()?;
            let mut rb = b.reader()?;
            let mut header: Vec<String> = Vec::new();
            let mut n = 0u64;
            while n < MAX_SCAN_LINES {
                let (Some(la), Some(lb)) = (read_line(ra.as_mut())?, read_line(rb.as_mut())?)
                else {
                    break;
                };
                n += 1;
                let (ca, cb) = (content_of(&la.head), content_of(&lb.head));
                if let FileKind::Csv(d) = kind {
                    let fa = normalize::split_csv(ca, d);
                    let fb = normalize::split_csv(cb, d);
                    if n == 1 {
                        header = fa.iter().map(|f| normalize::unquote(f)).collect();
                        continue;
                    }
                    for (i, (x, y)) in fa.iter().zip(fb.iter()).enumerate() {
                        if x != y {
                            let name = header.get(i).cloned().unwrap_or_else(|| format!("col{i}"));
                            acc.entry(name)
                                .or_default()
                                .add(&normalize::unquote(x), &normalize::unquote(y));
                        }
                    }
                } else if ca != cb {
                    if let (Ok(va), Ok(vb)) = (
                        serde_json::from_slice::<Value>(ca),
                        serde_json::from_slice::<Value>(cb),
                    ) {
                        leaf_diffs(&va, &vb, "", &mut acc);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(acc
        .into_iter()
        .filter(|(_, v)| v.differing > 0 || v.non_numeric > 0)
        .map(|(field, v)| NumericField {
            field,
            differing: v.differing,
            max_abs_diff: v.max_abs,
            max_rel_diff: v.max_rel,
            non_numeric_diffs: v.non_numeric,
        })
        .collect())
}

/// Column / row-group / first-row diff of two Parquet files.
#[cfg(feature = "parquet")]
pub fn parquet_diff(a: &std::path::Path, b: &std::path::Path) -> Result<ParquetDiff> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    let ra = SerializedFileReader::new(File::open(a)?)?;
    let rb = SerializedFileReader::new(File::open(b)?)?;
    let ma = ra.metadata();
    let mb = rb.metadata();
    let mut d = ParquetDiff {
        rows_a: ma.file_metadata().num_rows(),
        rows_b: mb.file_metadata().num_rows(),
        schema_differs: ma.file_metadata().schema() != mb.file_metadata().schema(),
        ..Default::default()
    };
    // Row-group boundaries of A, as cumulative row counts.
    let mut bounds = Vec::new();
    let mut total = 0i64;
    for rg in ma.row_groups() {
        total += rg.num_rows();
        bounds.push(total);
    }
    let group_of = |row: u64| {
        bounds
            .iter()
            .position(|&end| (row as i64) < end)
            .unwrap_or(0)
    };
    let mut columns = BTreeSet::new();
    let mut groups = BTreeSet::new();
    let mut ia = ra.get_row_iter(None)?;
    let mut ib = rb.get_row_iter(None)?;
    let mut row = 0u64;
    while row < MAX_SCAN_LINES * 10 {
        match (ia.next(), ib.next()) {
            (None, None) => break,
            (Some(x), Some(y)) => {
                let (x, y) = (x?, y?);
                if x != y {
                    for ((na, fa), (nb, fb)) in x.get_column_iter().zip(y.get_column_iter()) {
                        if na != nb || fa != fb {
                            columns.insert(na.clone());
                        }
                    }
                    groups.insert(group_of(row));
                    if d.first_row.is_none() {
                        d.first_row = Some(row);
                        d.first_row_a = Some(util::truncate(&x.to_string(), 500));
                        d.first_row_b = Some(util::truncate(&y.to_string(), 500));
                    }
                }
            }
            (x, y) => {
                groups.insert(group_of(row));
                if d.first_row.is_none() {
                    d.first_row = Some(row);
                    d.first_row_a = x.and_then(|r| r.ok()).map(|r| r.to_string());
                    d.first_row_b = y.and_then(|r| r.ok()).map(|r| r.to_string());
                }
                break;
            }
        }
        row += 1;
    }
    d.columns = columns.into_iter().collect();
    d.row_groups = groups.into_iter().collect();
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, h: &str, n: Option<&str>) -> FileRec {
        FileRec {
            path: path.into(),
            size: 1,
            blake3: h.into(),
            normalized_blake3: n.map(|s| s.to_string()),
            rules: Vec::new(),
            abs: None,
        }
    }

    #[test]
    fn set_comparison_statuses() {
        let a = vec![
            rec("a", "1", None),
            rec("b", "2", None),
            rec("c", "3", Some("n")),
        ];
        let b = vec![
            rec("b", "x", None),
            rec("c", "4", Some("n")),
            rec("d", "5", None),
        ];
        let r = compare_sets(&a, &b);
        let st: Vec<(&str, FileStatus)> = r.iter().map(|c| (c.path.as_str(), c.status)).collect();
        assert_eq!(
            st,
            vec![
                ("a", FileStatus::OnlyInA),
                ("b", FileStatus::Different),
                ("c", FileStatus::IdenticalAfterNormalization),
                ("d", FileStatus::OnlyInB),
            ]
        );
    }

    #[test]
    fn localizes_bytes_and_lines() {
        let a = Src::Mem(b"acc=0.81\nts=2026-10-08T15:01:02\nend\n".to_vec());
        let b = Src::Mem(b"acc=0.81\nts=2026-10-08T15:01:09\nend\n".to_vec());
        let loc = localize("result.txt", &a, &b);
        assert_eq!(loc.kind, "text");
        assert_eq!(loc.byte_offset, Some(9 + 3 + 18));
        assert_eq!(loc.line, Some(2));
        assert!(loc
            .unified_diff
            .unwrap()
            .contains("+ts=2026-10-08T15:01:09"));
    }

    #[test]
    fn detects_crlf_only_difference() {
        let a = Src::Mem(b"x\ny\n".to_vec());
        let b = Src::Mem(b"x\ny\r\n".to_vec());
        let loc = localize("a.txt", &a, &b);
        assert!(loc.line_ending_only);
        assert_eq!(loc.line, Some(2));
    }

    #[test]
    fn jsonl_record_diff_and_order() {
        let a = Src::Mem(b"{\"id\":1,\"v\":0.5}\n{\"id\":2,\"v\":0.25}\n".to_vec());
        let b = Src::Mem(b"{\"id\":2,\"v\":0.25}\n{\"id\":1,\"v\":0.5}\n".to_vec());
        let loc = localize("r.jsonl", &a, &b);
        assert_eq!(loc.same_records_different_order, Some(true));
        assert_eq!(loc.record_diff.len(), 2);
        assert_eq!(loc.record_diff[0].key, "id");
    }

    #[test]
    fn json_first_differing_key() {
        let a: Value = serde_json::from_str(r#"{"a":{"b":[1,2,{"c":3}]},"z":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"b":[1,2,{"c":4}]},"z":2}"#).unwrap();
        assert_eq!(first_json_diff(&a, &b, "$").as_deref(), Some("$.a.b[2].c"));
    }

    #[test]
    fn csv_numeric_closeness() {
        let a = Src::Mem(b"step,loss\n1,0.123456789\n2,0.5\n".to_vec());
        let b = Src::Mem(b"step,loss\n1,0.123456788\n2,0.5\n".to_vec());
        let loc = localize("m.csv", &a, &b);
        assert_eq!(loc.numeric.len(), 1);
        assert_eq!(loc.numeric[0].field, "loss");
        assert!(loc.numeric[0].max_rel_diff < 1e-6);
        assert_eq!(loc.numeric[0].non_numeric_diffs, 0);
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn parquet_reports_column_and_row() {
        use parquet::data_type::{DoubleType, Int64Type};
        use parquet::file::properties::WriterProperties;
        use parquet::file::writer::SerializedFileWriter;
        use parquet::schema::parser::parse_message_type;
        use std::sync::Arc;

        fn write(path: &std::path::Path, scores: &[f64]) {
            let schema = Arc::new(
                parse_message_type("message s { required int64 id; required double score; }")
                    .unwrap(),
            );
            let props = Arc::new(WriterProperties::builder().build());
            let file = File::create(path).unwrap();
            let mut w = SerializedFileWriter::new(file, schema, props).unwrap();
            let mut rg = w.next_row_group().unwrap();
            let ids: Vec<i64> = (0..scores.len() as i64).collect();
            if let Some(mut c) = rg.next_column().unwrap() {
                c.typed::<Int64Type>()
                    .write_batch(&ids[..], None, None)
                    .unwrap();
                c.close().unwrap();
            }
            if let Some(mut c) = rg.next_column().unwrap() {
                c.typed::<DoubleType>()
                    .write_batch(scores, None, None)
                    .unwrap();
                c.close().unwrap();
            }
            rg.close().unwrap();
            w.close().unwrap();
        }

        let dir = std::env::temp_dir().join(format!("repro-pq-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pa = dir.join("a.parquet");
        let pb = dir.join("b.parquet");
        write(&pa, &[0.1, 0.2, 0.3, 0.4]);
        write(&pb, &[0.1, 0.2, 0.35, 0.4]);
        let d = parquet_diff(&pa, &pb).unwrap();
        assert_eq!(d.columns, vec!["score".to_string()]);
        assert_eq!(d.first_row, Some(2));
        assert_eq!(d.row_groups, vec![0]);
        assert!(!d.schema_differs);
        let loc = localize("a.parquet", &Src::File(pa), &Src::File(pb));
        assert!(loc.parquet.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
