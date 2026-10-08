//! `repro init`, `repro hash`, `repro verify`, `repro diff`, `repro cache-log`.

use crate::cachelog;
use crate::check::{EXIT_DIFFERENT, EXIT_IDENTICAL};
use crate::cli::{CacheLogArgs, DiffArgs, HashArgs, VerifyArgs};
use crate::compare::{self, FileCmp, FileRec, FileStatus};
use crate::config;
use crate::hashing::HashCache;
use crate::manifest::{self, FileEntry, Manifest};
use crate::provenance;
use crate::report;
use crate::util::{self, Selector, Style};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SAMPLE_CONFIG: &str = include_str!("../repro.toml.example");

pub fn init() -> Result<u8> {
    let p = Path::new(config::FILE_NAME);
    if p.exists() {
        bail!(
            "{} already exists; repro init never overwrites it (move it away first)",
            config::FILE_NAME
        );
    }
    std::fs::write(p, SAMPLE_CONFIG)
        .with_context(|| format!("cannot write {}", config::FILE_NAME))?;
    println!(
        "wrote {}: edit [project] and [run].command, then run `repro check`",
        config::FILE_NAME
    );
    Ok(EXIT_IDENTICAL)
}

/// Hash cache only inside a project (a directory with repro.toml), so `repro hash`
/// never creates `.repro-cache/` in arbitrary directories.
fn project_hash_cache() -> HashCache {
    if Path::new(config::FILE_NAME).is_file() {
        HashCache::open(Path::new("."))
    } else {
        HashCache::disabled()
    }
}

pub fn hash(args: HashArgs) -> Result<u8> {
    let out_abs = util::absolute(&args.out)?;
    if out_abs.exists() && !Manifest::sniff(&out_abs) {
        bail!(
            "{} exists and is not a repro manifest; refusing to overwrite it",
            args.out.display()
        );
    }
    let sel = Selector::new(&args.include, &args.exclude)?;
    let found: Vec<manifest::Found> = manifest::collect_paths(&args.paths, &args.root, &sel)?
        .into_iter()
        .filter(|f| f.abs != out_abs)
        .collect();
    let cache = project_hash_cache();
    let start = Instant::now();
    let files = manifest::hash_found(&found, &cache)?;
    let secs = start.elapsed().as_secs_f64();
    cache.save();
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut m = Manifest::new(files);
    if args.provenance {
        m.provenance = Some(provenance::gather(Path::new("."), &[]));
    }
    m.save(&args.out)?;
    let speed = if secs > 0.0 {
        format!(", {:.0} MiB/s", total as f64 / (1024.0 * 1024.0) / secs)
    } else {
        String::new()
    };
    println!(
        "hashed {} file(s), {} in {:.2} s{speed} -> {}",
        m.files.len(),
        util::fmt_bytes(total),
        secs,
        args.out.display()
    );
    Ok(EXIT_IDENTICAL)
}

#[derive(Serialize)]
struct DiffReport {
    repro_schema: u32,
    tool_version: String,
    kind: String,
    a: String,
    b: String,
    total: usize,
    identical: usize,
    changed: usize,
    only_in_a: usize,
    only_in_b: usize,
    verdict: String,
    /// Only the files that do not match.
    files: Vec<FileCmp>,
}

fn entries_to_recs(entries: &[FileEntry]) -> Vec<FileRec> {
    entries
        .iter()
        .map(|e| FileRec {
            path: e.path.clone(),
            size: e.size,
            blake3: e.blake3.clone(),
            normalized_blake3: None,
            rules: Vec::new(),
            abs: None,
        })
        .collect()
}

fn hash_dir(dir: &Path, cache: &HashCache) -> Result<Vec<FileRec>> {
    let found = manifest::walk_files(dir, &Selector::new(&[], &[])?, &[])?;
    let entries = manifest::hash_found(&found, cache)?;
    Ok(entries
        .into_iter()
        .zip(found)
        .map(|(e, f)| FileRec {
            path: e.path,
            size: e.size,
            blake3: e.blake3,
            normalized_blake3: None,
            rules: Vec::new(),
            abs: Some(f.abs),
        })
        .collect())
}

pub fn verify(args: VerifyArgs) -> Result<u8> {
    let golden = Manifest::load(&args.against)?;
    let sel = Selector::new(&[], &[])?;
    let against_abs = util::absolute(&args.against)?;
    let found: Vec<manifest::Found> = manifest::collect_paths(&args.paths, &args.root, &sel)?
        .into_iter()
        .filter(|f| f.abs != against_abs)
        .collect();
    let cache = project_hash_cache();
    let entries = manifest::hash_found(&found, &cache)?;
    cache.save();
    let cmps = compare::compare_sets(&entries_to_recs(&golden.files), &entries_to_recs(&entries));
    let a = args.against.display().to_string();
    let b = args
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    finish_diff("verify", a, b, cmps, args.json, ("missing", "extra"))
}

enum Side {
    Dir(PathBuf),
    Manifest(Box<Manifest>),
}

fn side(p: &Path) -> Result<Side> {
    if p.is_dir() {
        Ok(Side::Dir(p.to_path_buf()))
    } else if Manifest::sniff(p) {
        Ok(Side::Manifest(Box::new(Manifest::load(p)?)))
    } else if !p.exists() {
        bail!("{} does not exist", p.display())
    } else {
        bail!(
            "{} is neither a directory nor a repro manifest",
            p.display()
        )
    }
}

pub fn diff(args: DiffArgs) -> Result<u8> {
    let cache = HashCache::disabled();
    let recs = |s: Side| -> Result<Vec<FileRec>> {
        match s {
            Side::Dir(d) => hash_dir(&d, &cache),
            Side::Manifest(m) => Ok(entries_to_recs(&m.files)),
        }
    };
    let ra = recs(side(&args.a)?)?;
    let rb = recs(side(&args.b)?)?;
    let cmps = compare::compare_and_localize(&ra, &rb, None, 10);
    finish_diff(
        "diff",
        args.a.display().to_string(),
        args.b.display().to_string(),
        cmps,
        args.json,
        ("only in A", "only in B"),
    )
}

fn finish_diff(
    kind: &str,
    a: String,
    b: String,
    cmps: Vec<FileCmp>,
    json: bool,
    (only_a, only_b): (&str, &str),
) -> Result<u8> {
    let count = |s: FileStatus| cmps.iter().filter(|c| c.status == s).count();
    let changed = count(FileStatus::Different);
    let (n_a, n_b) = (count(FileStatus::OnlyInA), count(FileStatus::OnlyInB));
    let identical = cmps.len() - changed - n_a - n_b;
    let same = changed + n_a + n_b == 0;
    let report = DiffReport {
        repro_schema: manifest::SCHEMA,
        tool_version: manifest::TOOL_VERSION.to_string(),
        kind: kind.to_string(),
        a,
        b,
        total: cmps.len(),
        identical,
        changed,
        only_in_a: n_a,
        only_in_b: n_b,
        verdict: if same { "IDENTICAL" } else { "DIFFERENT" }.to_string(),
        files: cmps.into_iter().filter(|c| !c.status.is_match()).collect(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let st = Style::for_stdout();
        println!("{} {} vs {}", st.bold(kind), report.a, report.b);
        for f in report.files.iter().take(50) {
            let what = match f.status {
                FileStatus::Different => "changed",
                FileStatus::OnlyInA => only_a,
                _ => only_b,
            };
            let sizes = match (f.size_a, f.size_b) {
                (Some(x), Some(y)) => {
                    format!("  ({} -> {})", util::fmt_bytes(x), util::fmt_bytes(y))
                }
                _ => String::new(),
            };
            println!("  {:<10} {}{sizes}", st.red(what), f.path);
        }
        if report.files.len() > 50 {
            println!("  … and {} more", report.files.len() - 50);
        }
        for f in report
            .files
            .iter()
            .filter(|f| f.localization.is_some())
            .take(3)
        {
            println!("{}", st.bold(&format!("first difference in {}", f.path)));
            if let Some(loc) = &f.localization {
                for l in report::describe_localization(loc) {
                    println!("  {l}");
                }
                if let Some(d) = &loc.unified_diff {
                    for l in d.lines() {
                        println!("    {l}");
                    }
                }
            }
        }
        let summary = format!(
            "{}/{} file(s) identical ({} changed, {} {}, {} {})",
            identical, report.total, changed, n_a, only_a, n_b, only_b
        );
        println!("{summary}");
        let v = format!("VERDICT: {}", report.verdict);
        println!(
            "{}",
            if same {
                st.bold(&st.green(&v))
            } else {
                st.bold(&st.red(&v))
            }
        );
    }
    Ok(if same { EXIT_IDENTICAL } else { EXIT_DIFFERENT })
}

pub fn cache_log(args: CacheLogArgs) -> Result<u8> {
    let required = match &args.required_key_parts {
        Some(parts) => parts.clone(),
        None => config::load(Path::new(config::FILE_NAME))
            .ok()
            .flatten()
            .and_then(|c| c.cache)
            .map(|c| c.required_key_parts)
            .unwrap_or_else(config::default_key_parts),
    };
    let logs: Vec<cachelog::CacheLog> = args
        .files
        .iter()
        .map(|p| cachelog::load(&p.display().to_string(), p))
        .collect();
    let report = cachelog::analyze(&logs, &required, args.mode);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let st = Style::for_stdout();
        println!(
            "{} mode {}  required key parts: {}",
            st.bold("cache-log"),
            report.mode,
            report.required_key_parts.join(", ")
        );
        for s in &report.runs {
            let top: Vec<String> = s
                .top_missed_experts
                .iter()
                .map(|e| format!("{} ({})", e.expert, e.misses))
                .collect();
            println!(
                "  {}: {} events ({} hit / {} miss / {} store, hit rate {:.1}%){}",
                s.label,
                s.events,
                s.hits,
                s.misses,
                s.stores,
                s.hit_rate * 100.0,
                if top.is_empty() {
                    String::new()
                } else {
                    format!("  top missed: {}", top.join(", "))
                }
            );
        }
        if report.same_sequence == Some(true) {
            println!("  same (event, key, value) sequence in every log");
        }
        for i in &report.issues {
            println!("  {} [{}] {}", st.red("✗"), i.check, i.message);
            for d in i.details.iter().take(5) {
                println!("      {d}");
            }
        }
        let v = if report.ok() {
            st.bold(&st.green("VERDICT: cache logs OK"))
        } else {
            st.bold(&st.red(&format!("VERDICT: {} cache issue(s)", report.issues.len())))
        };
        println!("{v}");
    }
    Ok(if report.ok() {
        EXIT_IDENTICAL
    } else {
        EXIT_DIFFERENT
    })
}
