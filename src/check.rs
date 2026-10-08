//! `repro check`: run the command N times, compare outputs, check cache logs and seeds,
//! derive hints, and write the reports.

use crate::cachelog::{self, CacheMode, CacheReport};
use crate::cli::CheckArgs;
use crate::compare::{self, FileCmp, FileRec, FileStatus};
use crate::config::{self, ReproConfig};
use crate::hashing::{self, HashCache};
use crate::hints::{self, Hint, HintContext};
use crate::manifest::{self, FileEntry, Manifest};
use crate::normalize::Normalizer;
use crate::provenance::{self, Provenance};
use crate::report;
use crate::runner::{self, RunOutcome, RunSpec};
use crate::util::{self, Selector, Style};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const OUT_MARKER: &str = ".repro-report";
pub const WORK_MARKER: &str = ".repro-work-dir";
const OWN_OUT_ENTRIES: [&str; 5] = [
    OUT_MARKER,
    "report.json",
    "report.md",
    "report.html",
    "manifests",
];
/// Mismatching files that get a full localization per comparison.
const MAX_LOCALIZE: usize = 10;

pub const EXIT_IDENTICAL: u8 = 0;
pub const EXIT_DIFFERENT: u8 = 1;
pub const EXIT_ERROR: u8 = 2;
pub const EXIT_COMMAND_FAILED: u8 = 3;

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// IDENTICAL | DIFFERENT | ERROR
    pub status: String,
    pub strict: bool,
    pub label: String,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    pub label: String,
    pub run_id: String,
    pub seed: u64,
    pub run_dir: String,
    pub cache_dir: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub error: Option<String>,
    pub wall_ms: u64,
    pub peak_rss_mb: Option<u64>,
    pub files: usize,
    pub stdout_blake3: String,
    pub stderr_blake3: String,
    pub stdout_tail: Vec<String>,
    pub stderr_tail: Vec<String>,
    pub manifest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRow {
    pub path: String,
    pub size: Option<u64>,
    pub blake3: Option<String>,
    /// Worst status over all comparisons against run 1.
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub a: String,
    pub b: String,
    pub total: usize,
    pub identical: usize,
    /// Only the files that do not match.
    pub mismatches: Vec<FileCmp>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutputsSummary {
    pub total: usize,
    pub identical: usize,
    pub identical_after_normalization: usize,
    pub different: usize,
    pub one_sided: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigHashCheck {
    /// ok | not_recorded | wrong | skipped
    pub status: String,
    pub message: String,
    pub file: Option<String>,
    pub recorded: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeedCheck {
    pub seed: u64,
    pub changed_files: usize,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FloatClose {
    pub comparison: String,
    pub path: String,
    pub field: String,
    pub cells: u64,
    pub max_abs_diff: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NormalizationUse {
    pub index: usize,
    pub files: String,
    pub description: String,
    pub reason: String,
    /// Files of run 1 the rule applies to.
    pub applied_to: Vec<String>,
    /// Files that match only because of this rule.
    pub needed_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub repro_schema: u32,
    pub tool_version: String,
    pub kind: String,
    pub project: Option<String>,
    pub config_path: Option<String>,
    pub config_hash: Option<String>,
    pub seed: u64,
    pub runs_requested: u32,
    pub cache_mode: String,
    pub command: Vec<String>,
    pub strict_env: bool,
    pub verdict: Verdict,
    pub exit_code: u8,
    pub runs: Vec<RunInfo>,
    pub seed_run: Option<RunInfo>,
    pub outputs: OutputsSummary,
    pub files: Vec<FileRow>,
    pub comparisons: Vec<Comparison>,
    pub config_hash_check: ConfigHashCheck,
    pub cache: Option<CacheReport>,
    pub seed_check: Option<SeedCheck>,
    pub float_tol: Option<f64>,
    pub float_close: Vec<FloatClose>,
    pub hints: Vec<Hint>,
    pub normalizations: Vec<NormalizationUse>,
    pub provenance: Provenance,
    pub warnings: Vec<String>,
    pub work_dir: String,
    pub run_dirs_kept: bool,
}

/// Everything a single run needs that does not change between runs.
struct Ctx {
    root: PathBuf,
    out_dir: PathBuf,
    command: Vec<String>,
    config_rel: Option<String>,
    config_hash: Option<String>,
    run_id_base: String,
    selector: Selector,
    normalizer: Normalizer,
    cache_log: Option<String>,
    env_allow: Vec<String>,
    strict_env: bool,
    timeout: Option<Duration>,
    provenance: Provenance,
}

struct RunData {
    info: RunInfo,
    recs: Vec<FileRec>,
    outcome: RunOutcome,
    run_dir: PathBuf,
    cache_log: Option<PathBuf>,
}

/// Terminal printer: stdout normally, stderr when `--json` owns stdout.
struct Term {
    style: Style,
    to_stderr: bool,
}

impl Term {
    fn line(&self, s: &str) {
        if self.to_stderr {
            let _ = writeln!(std::io::stderr(), "{s}");
        } else {
            let _ = writeln!(std::io::stdout(), "{s}");
        }
    }
}

pub fn run(args: CheckArgs) -> Result<u8> {
    let root = util::absolute(Path::new("."))?;
    let mut warnings: Vec<String> = Vec::new();
    let cfg = match config::load(&root.join(config::FILE_NAME))? {
        Some(c) => c,
        None => {
            if args.command.is_empty() {
                bail!(
                    "no {} in {} and no command given; run `repro init` or pass a command after `--`",
                    config::FILE_NAME,
                    root.display()
                );
            }
            warnings.push(
                "no repro.toml found; using defaults (every file in the run directory is an output)"
                    .to_string(),
            );
            ReproConfig::default()
        }
    };
    if args.runs < 2 {
        bail!("--runs must be at least 2 (got {})", args.runs);
    }
    if let Some(t) = args.float_tol {
        if !t.is_finite() || t <= 0.0 {
            bail!("--float-tol must be a positive number (got {t})");
        }
    }
    if args.timeout == Some(0) {
        bail!("--timeout must be > 0 seconds");
    }
    let command = if !args.command.is_empty() {
        args.command.clone()
    } else if !cfg.run.command.is_empty() {
        cfg.run.command.clone()
    } else {
        bail!("no command to run: pass one after `--` or set [run].command in repro.toml");
    };
    let seed = match args.seed.or(cfg.project.seed) {
        Some(s) => s,
        None => {
            warnings.push("no seed given (--seed or [project].seed); using 0".to_string());
            0
        }
    };
    let timeout = args
        .timeout
        .or(cfg.project.timeout_s)
        .map(Duration::from_secs);

    let (config_rel, config_hash) = match &cfg.project.config {
        Some(c) => (
            Some(c.clone()),
            Some(config::hash_yaml_file(&root.join(c))?),
        ),
        None => (None, None),
    };

    let out_dir = util::absolute(&args.out)?;
    let work = util::absolute(&args.work_dir)?;
    if out_dir.starts_with(&work) || work.starts_with(&out_dir) {
        bail!("--out and --work-dir must be separate directories");
    }
    if root.starts_with(&work) || root.starts_with(&out_dir) {
        bail!("--out and --work-dir must not contain the project directory");
    }
    prepare_out(&out_dir, args.force)?;
    prepare_work(&work)?;

    let term = Term {
        style: if args.json {
            Style::for_stderr()
        } else {
            Style::for_stdout()
        },
        to_stderr: args.json,
    };
    let st = term.style;

    // Provenance and input hashes are recorded once and copied into every manifest.
    let mut prov = provenance::gather(&root, &cfg.run.env_allow);
    let hash_cache = HashCache::open(&root);
    let skip = skip_dirs(&root, &[work.as_path(), out_dir.as_path()]);
    prov.inputs = provenance::hash_inputs(&root, &cfg.inputs.files, &skip, &hash_cache)?;
    hash_cache.save();

    let run_id_base = format!(
        "repro-{}",
        match &config_hash {
            Some(h) => h[..8].to_string(),
            None => hashing::hash_bytes(command.join("\u{0}").as_bytes())[..8].to_string(),
        }
    );
    let ctx = Ctx {
        root: root.clone(),
        out_dir: out_dir.clone(),
        command: command.clone(),
        config_rel: config_rel.clone(),
        config_hash: config_hash.clone(),
        run_id_base,
        selector: Selector::new(&cfg.outputs.include, &cfg.outputs.exclude)?,
        normalizer: Normalizer::new(&cfg.normalize)?,
        cache_log: cfg.cache.as_ref().map(|c| c.log.clone()),
        env_allow: cfg.run.env_allow.clone(),
        strict_env: args.strict_env,
        timeout,
        provenance: prov.clone(),
    };

    term.line(&format!(
        "{}  {}  config {}  seed {seed}  runs {}  mode {}",
        st.bold(&format!("repro {}", manifest::TOOL_VERSION)),
        cfg.project.name.as_deref().unwrap_or("-"),
        config_hash.as_deref().map_or("-", util::short),
        args.runs,
        args.cache_mode.name()
    ));

    // Runs 1..=N with the same seed.
    let mut runs: Vec<RunData> = Vec::new();
    for i in 1..=args.runs {
        let cache_dir = match args.cache_mode {
            CacheMode::ColdCold => work.join(format!("cache-{i}")),
            CacheMode::ColdWarm => work.join("cache"),
        };
        let rd = execute(
            &ctx,
            &format!("run {i}"),
            &i.to_string(),
            seed,
            &work.join(format!("run-{i}")),
            &cache_dir,
        )?;
        print_run(&term, &rd);
        let failed = !rd.outcome.ok();
        runs.push(rd);
        if failed {
            break;
        }
    }
    let mut command_failed = runs.iter().any(|r| !r.outcome.ok());

    // Seed sensitivity: one more run with seed + 1 and a fresh cache.
    let mut seed_run: Option<RunData> = None;
    if args.seed_sensitivity && !command_failed {
        let s2 = seed.wrapping_add(1);
        let rd = execute(
            &ctx,
            "seed+1",
            "seed",
            s2,
            &work.join("run-seed"),
            &work.join("cache-seed"),
        )?;
        print_run(&term, &rd);
        command_failed |= !rd.outcome.ok();
        seed_run = Some(rd);
    }

    let mut report = CheckReport {
        repro_schema: manifest::SCHEMA,
        tool_version: manifest::TOOL_VERSION.to_string(),
        kind: "check".to_string(),
        project: cfg.project.name.clone(),
        config_path: config_rel.clone(),
        config_hash: config_hash.clone(),
        seed,
        runs_requested: args.runs,
        cache_mode: args.cache_mode.name().to_string(),
        command: command.clone(),
        strict_env: args.strict_env,
        verdict: Verdict {
            status: String::new(),
            strict: false,
            label: String::new(),
            failures: Vec::new(),
        },
        exit_code: 0,
        runs: runs.iter().map(|r| r.info.clone()).collect(),
        seed_run: seed_run.as_ref().map(|r| r.info.clone()),
        outputs: OutputsSummary::default(),
        files: Vec::new(),
        comparisons: Vec::new(),
        config_hash_check: ConfigHashCheck {
            status: "skipped".to_string(),
            message: "no [project].config in repro.toml".to_string(),
            file: None,
            recorded: None,
        },
        cache: None,
        seed_check: None,
        float_tol: args.float_tol,
        float_close: Vec::new(),
        hints: Vec::new(),
        normalizations: Vec::new(),
        provenance: prov,
        warnings,
        work_dir: work.display().to_string(),
        run_dirs_kept: true,
    };

    if command_failed {
        let bad = runs
            .iter()
            .chain(seed_run.iter())
            .find(|r| !r.outcome.ok())
            .expect("a failed run exists");
        let why = bad.outcome.failure_text().unwrap_or_default();
        report.verdict = Verdict {
            status: "ERROR".to_string(),
            strict: false,
            label: "ERROR (the command failed)".to_string(),
            failures: vec![format!("{}: the command {why}", bad.info.label)],
        };
        report.exit_code = EXIT_COMMAND_FAILED;
        let tail = &bad.info.stderr_tail;
        if !tail.is_empty() {
            term.line(&st.dim("last stderr lines:"));
            for l in &tail[tail.len().saturating_sub(10)..] {
                term.line(&format!("  {l}"));
            }
        }
        return finish(&term, &args, &work, report, &ctx);
    }

    if runs[0].recs.is_empty() {
        report.verdict = Verdict {
            status: "ERROR".to_string(),
            strict: false,
            label: "ERROR (no outputs)".to_string(),
            failures: vec![format!(
                "no output files matched [outputs].include in {}; nothing was compared (the command must write into REPRO_RUN_DIR / {{run_dir}})",
                runs[0].run_dir.display()
            )],
        };
        report.exit_code = EXIT_ERROR;
        return finish(&term, &args, &work, report, &ctx);
    }

    // Compare run 1 against every other run.
    let mut failures: Vec<String> = Vec::new();
    let mut all_cmps: Vec<(usize, Vec<FileCmp>)> = Vec::new();
    for (k, r) in runs.iter().enumerate().skip(1) {
        let cmps = compare::compare_and_localize(
            &runs[0].recs,
            &r.recs,
            Some(&ctx.normalizer),
            MAX_LOCALIZE,
        );
        let bad: Vec<&FileCmp> = cmps.iter().filter(|c| !c.status.is_match()).collect();
        if let Some(first) = bad.first() {
            failures.push(format!(
                "{}: {} of {} output file(s) differ from run 1 (first: {})",
                r.info.label,
                bad.len(),
                cmps.len(),
                first.path
            ));
        }
        report.comparisons.push(Comparison {
            a: runs[0].info.label.clone(),
            b: r.info.label.clone(),
            total: cmps.len(),
            identical: cmps.len() - bad.len(),
            mismatches: bad.into_iter().cloned().collect(),
        });
        all_cmps.push((k, cmps));
    }

    // File table: worst status per path over all comparisons.
    let rank = |s: FileStatus| match s {
        FileStatus::Identical => 0,
        FileStatus::IdenticalAfterNormalization => 1,
        FileStatus::OnlyInA | FileStatus::OnlyInB => 2,
        FileStatus::Different => 3,
    };
    let mut rows: BTreeMap<String, FileRow> = BTreeMap::new();
    for (_, cmps) in &all_cmps {
        for c in cmps {
            let row = rows.entry(c.path.clone()).or_insert_with(|| FileRow {
                path: c.path.clone(),
                size: c.size_a,
                blake3: c.blake3_a.clone(),
                status: c.status,
                rules: c.rules.clone(),
            });
            if rank(c.status) > rank(row.status) {
                row.status = c.status;
            }
        }
    }
    report.files = rows.into_values().collect();
    for f in &report.files {
        report.outputs.total += 1;
        match f.status {
            FileStatus::Identical => report.outputs.identical += 1,
            FileStatus::IdenticalAfterNormalization => {
                report.outputs.identical_after_normalization += 1
            }
            FileStatus::Different => report.outputs.different += 1,
            FileStatus::OnlyInA | FileStatus::OnlyInB => report.outputs.one_sided += 1,
        }
    }

    // Declared normalizations and where they were needed.
    for (i, rule) in ctx.normalizer.rules().iter().enumerate() {
        let applied_to: Vec<String> = runs[0]
            .recs
            .iter()
            .filter(|r| r.rules.contains(&i))
            .map(|r| r.path.clone())
            .collect();
        let needed_for: Vec<String> = report
            .files
            .iter()
            .filter(|f| f.status == FileStatus::IdenticalAfterNormalization && f.rules.contains(&i))
            .map(|f| f.path.clone())
            .collect();
        report.normalizations.push(NormalizationUse {
            index: i,
            files: rule.files.clone(),
            description: rule.describe(),
            reason: rule.reason.clone(),
            applied_to,
            needed_for,
        });
    }

    // Recorded config hash.
    if let Some(h) = &config_hash {
        report.config_hash_check = check_config_hash(&runs, h);
        if report.config_hash_check.status == "wrong" {
            failures.push(report.config_hash_check.message.clone());
        }
    }

    // Cache logs.
    if let Some(cache_cfg) = &cfg.cache {
        let logs: Vec<cachelog::CacheLog> = runs
            .iter()
            .map(|r| match &r.cache_log {
                Some(p) => cachelog::load(&r.info.label, p),
                None => cachelog::load(&r.info.label, Path::new("")),
            })
            .collect();
        let cr = cachelog::analyze(&logs, &cache_cfg.required_key_parts, Some(args.cache_mode));
        for i in &cr.issues {
            failures.push(format!("cache: {}", i.message));
        }
        report.cache = Some(cr);
    } else if args.cache_mode == CacheMode::ColdWarm {
        report.warnings.push(
            "--cache-mode cold-warm without a [cache] section: cache checks skipped".to_string(),
        );
    }

    // Seed sensitivity.
    if let Some(sr) = &seed_run {
        let cmps = compare::compare_sets(&runs[0].recs, &sr.recs);
        let changed = cmps.iter().filter(|c| !c.status.is_match()).count();
        let ok = changed > 0;
        if !ok {
            failures.push(format!(
                "seed has no effect; the seed is ignored somewhere (seeds {seed} and {} gave identical outputs)",
                seed.wrapping_add(1)
            ));
        }
        report.seed_check = Some(SeedCheck {
            seed: seed.wrapping_add(1),
            changed_files: changed,
            ok,
        });
    }

    // Numerically close but not identical (information only).
    if let Some(tol) = args.float_tol {
        for c in &report.comparisons {
            for m in &c.mismatches {
                let Some(loc) = &m.localization else { continue };
                for n in &loc.numeric {
                    if n.differing > 0 && n.non_numeric_diffs == 0 && n.max_abs_diff < tol {
                        report.float_close.push(FloatClose {
                            comparison: format!("{} vs {}", c.a, c.b),
                            path: m.path.clone(),
                            field: n.field.clone(),
                            cells: n.differing,
                            max_abs_diff: n.max_abs_diff,
                        });
                    }
                }
            }
        }
    }

    // Verdict.
    let used: Vec<&NormalizationUse> = report
        .normalizations
        .iter()
        .filter(|n| !n.needed_for.is_empty())
        .collect();
    if failures.is_empty() {
        let label = if used.is_empty() {
            "IDENTICAL (strict)".to_string()
        } else {
            format!(
                "IDENTICAL (after {} declared normalization{}: {})",
                used.len(),
                if used.len() == 1 { "" } else { "s" },
                used.iter()
                    .map(|n| n.description.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        report.verdict = Verdict {
            status: "IDENTICAL".to_string(),
            strict: used.is_empty(),
            label,
            failures,
        };
        report.exit_code = EXIT_IDENTICAL;
    } else {
        report.verdict = Verdict {
            status: "DIFFERENT".to_string(),
            strict: false,
            label: "DIFFERENT".to_string(),
            failures,
        };
        report.exit_code = EXIT_DIFFERENT;
        report.hints = derive_hints(&runs, &all_cmps, &report);
    }

    print_summary(&term, &report);
    finish(&term, &args, &work, report, &ctx)
}

/// Writes reports, cleans up run directories when appropriate and prints the closing lines.
fn finish(
    term: &Term,
    args: &CheckArgs,
    work: &Path,
    mut report: CheckReport,
    ctx: &Ctx,
) -> Result<u8> {
    let st = term.style;
    let keep = args.keep || report.verdict.status != "IDENTICAL";
    report.run_dirs_kept = keep;
    if !keep && work.join(WORK_MARKER).is_file() {
        if let Err(e) = std::fs::remove_dir_all(work) {
            report.warnings.push(format!(
                "could not remove work directory {}: {e}",
                work.display()
            ));
        }
    }
    report::write_all(&report, &ctx.out_dir)?;

    if report.exit_code != EXIT_IDENTICAL || !report.verdict.failures.is_empty() {
        for f in &report.verdict.failures {
            term.line(&format!("  {} {f}", st.red("✗")));
        }
    }
    let verdict = format!("VERDICT: {}", report.verdict.label);
    term.line(&match report.verdict.status.as_str() {
        "IDENTICAL" => st.bold(&st.green(&verdict)),
        _ => st.bold(&st.red(&verdict)),
    });
    for w in &report.warnings {
        term.line(&st.yellow(&format!("warning: {w}")));
    }
    if keep {
        term.line(&st.dim(&format!("run directories kept in {}", work.display())));
    }
    term.line(&format!(
        "report: {}",
        args.out.join("report.html").display()
    ));
    if args.json {
        let json = report::json::render(&report)?;
        let mut out = std::io::stdout();
        out.write_all(json.as_bytes())?;
        out.write_all(b"\n")?;
    }
    Ok(report.exit_code)
}

fn execute(
    ctx: &Ctx,
    label: &str,
    tag: &str,
    seed: u64,
    run_dir: &Path,
    cache_dir: &Path,
) -> Result<RunData> {
    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("cannot create {}", run_dir.display()))?;
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("cannot create {}", cache_dir.display()))?;
    let run_id = format!("{}-{tag}", ctx.run_id_base);
    let config = ctx.config_rel.clone().unwrap_or_default();
    let vars = [
        ("run_dir", run_dir.display().to_string()),
        ("seed", seed.to_string()),
        ("run_id", run_id.clone()),
        ("cache_dir", cache_dir.display().to_string()),
        ("config", config.clone()),
    ];
    let argv: Vec<String> = ctx
        .command
        .iter()
        .map(|a| runner::expand(a, &vars))
        .collect();
    let mut env = vec![
        ("REPRO_RUN_DIR".to_string(), run_dir.display().to_string()),
        ("REPRO_RUN_ID".to_string(), run_id.clone()),
        ("REPRO_SEED".to_string(), seed.to_string()),
        (
            "REPRO_CACHE_DIR".to_string(),
            cache_dir.display().to_string(),
        ),
        (
            "REPRO_CONFIG_HASH".to_string(),
            ctx.config_hash.clone().unwrap_or_default(),
        ),
    ];
    if !config.is_empty() {
        env.push(("REPRO_CONFIG".to_string(), config));
    }
    let spec = RunSpec {
        program: argv[0].clone(),
        args: argv[1..].to_vec(),
        cwd: ctx.root.clone(),
        env,
        strict_env: ctx.strict_env.then(|| ctx.env_allow.clone()),
        timeout: ctx.timeout,
    };
    let outcome = runner::run(&spec);

    // The cache log is checked semantically, so it is not compared byte for byte.
    let cache_log = ctx.cache_log.as_ref().map(|t| {
        let p = PathBuf::from(runner::expand(t, &vars));
        if p.is_absolute() {
            p
        } else {
            ctx.root.join(p)
        }
    });
    let cache_rel = cache_log
        .as_ref()
        .and_then(|p| p.strip_prefix(run_dir).ok())
        .map(util::to_slash);
    let found: Vec<manifest::Found> = manifest::walk_files(run_dir, &ctx.selector, &[])?
        .into_iter()
        .filter(|f| cache_rel.as_deref() != Some(f.rel.as_str()))
        .collect();
    let hashed = hashing::parallel_map(&found, |f| -> Result<FileRec> {
        let (size, blake3) = hashing::hash_file(&f.abs)?;
        let rules = ctx.normalizer.rules_for(&f.rel);
        let normalized_blake3 = if rules.is_empty() {
            None
        } else {
            Some(ctx.normalizer.normalized_hash(&f.abs, &f.rel, &rules)?)
        };
        Ok(FileRec {
            path: f.rel.clone(),
            size,
            blake3,
            normalized_blake3,
            rules,
            abs: Some(f.abs.clone()),
        })
    });
    let mut recs = Vec::with_capacity(hashed.len());
    for r in hashed {
        recs.push(r?);
    }

    let manifest_name = format!("manifests/run-{tag}.json");
    let mut m = Manifest::new(
        recs.iter()
            .map(|r| FileEntry {
                path: r.path.clone(),
                size: r.size,
                blake3: r.blake3.clone(),
                normalized_blake3: r.normalized_blake3.clone(),
            })
            .collect(),
    );
    m.run_id = Some(run_id.clone());
    m.config_hash = ctx.config_hash.clone();
    m.seed = Some(seed);
    m.command = Some(argv.clone());
    m.exit_code = outcome.exit_code;
    m.wall_ms = Some(outcome.wall_ms);
    m.peak_rss_mb = outcome.peak_rss_mb;
    m.stdout_blake3 = Some(outcome.stdout_blake3.clone());
    m.stderr_blake3 = Some(outcome.stderr_blake3.clone());
    m.provenance = Some(ctx.provenance.clone());
    m.save(&ctx.out_dir.join(&manifest_name))?;

    let info = RunInfo {
        label: label.to_string(),
        run_id,
        seed,
        run_dir: run_dir.display().to_string(),
        cache_dir: cache_dir.display().to_string(),
        ok: outcome.ok(),
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        error: outcome.failure_text(),
        wall_ms: outcome.wall_ms,
        peak_rss_mb: outcome.peak_rss_mb,
        files: recs.len(),
        stdout_blake3: outcome.stdout_blake3.clone(),
        stderr_blake3: outcome.stderr_blake3.clone(),
        stdout_tail: outcome.stdout_tail.clone(),
        stderr_tail: outcome.stderr_tail.clone(),
        manifest: manifest_name,
    };
    Ok(RunData {
        info,
        recs,
        outcome,
        run_dir: run_dir.to_path_buf(),
        cache_log,
    })
}

/// Looks for a top-level `config_hash` in the runs' JSON outputs (run_meta.json first).
fn check_config_hash(runs: &[RunData], expected: &str) -> ConfigHashCheck {
    for r in runs {
        let mut candidates: Vec<&FileRec> = r
            .recs
            .iter()
            .filter(|f| f.path.to_ascii_lowercase().ends_with(".json") && f.size < (1 << 20))
            .collect();
        candidates.sort_by_key(|f| (!f.path.ends_with("run_meta.json"), f.path.clone()));
        let found = candidates.iter().find_map(|f| {
            let bytes = std::fs::read(f.abs.as_ref()?).ok()?;
            let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let h = v.get("config_hash")?.as_str()?.to_string();
            Some((f.path.clone(), h))
        });
        let Some((file, recorded)) = found else {
            return ConfigHashCheck {
                status: "not_recorded".to_string(),
                message: format!(
                    "config hash not recorded: no JSON output of {} has a top-level \"config_hash\" (write REPRO_CONFIG_HASH into run_meta.json)",
                    r.info.label
                ),
                file: None,
                recorded: None,
            };
        };
        let rec_l = recorded.to_ascii_lowercase();
        if rec_l.len() < 8 || !expected.starts_with(&rec_l) {
            return ConfigHashCheck {
                status: "wrong".to_string(),
                message: format!(
                    "config hash not recorded or wrong: {} in {} records {:?}, expected {}",
                    file,
                    r.info.label,
                    util::truncate(&recorded, 70),
                    util::short(expected)
                ),
                file: Some(file),
                recorded: Some(recorded),
            };
        }
    }
    ConfigHashCheck {
        status: "ok".to_string(),
        message: "recorded and matches".to_string(),
        file: None,
        recorded: None,
    }
}

fn derive_hints(
    runs: &[RunData],
    all: &[(usize, Vec<FileCmp>)],
    report: &CheckReport,
) -> Vec<Hint> {
    let mut hints = Vec::new();
    if let Some((k, cmps)) = all
        .iter()
        .find(|(_, c)| c.iter().any(|f| !f.status.is_match()))
    {
        let (a, b) = (&runs[0], &runs[*k]);
        let ctx = HintContext {
            run_dirs: [
                a.run_dir.display().to_string(),
                b.run_dir.display().to_string(),
            ],
            run_ids: [a.info.run_id.clone(), b.info.run_id.clone()],
            pids: [a.outcome.pid, b.outcome.pid],
            hostname: provenance::hostname(),
        };
        hints = hints::derive(cmps, &ctx);
    }
    if report.seed_check.as_ref().is_some_and(|s| !s.ok) {
        hints.push(Hint {
            id: "seed_ignored".to_string(),
            cause: "the seed does not reach the code that produces the outputs".to_string(),
            fix: "pass the seed to random.seed, numpy.random.seed and torch.manual_seed, and derive every component seed from it".to_string(),
            evidence: "seed and seed+1 produced identical outputs".to_string(),
        });
    }
    if let Some(c) = &report.cache {
        if c.issues.iter().any(|i| i.check == "poisoning") {
            hints.push(Hint {
                id: "cache_poisoning".to_string(),
                cause: "a cached computation returns different values for the same key".to_string(),
                fix: "seed the computation from the key parts (including seed) or add the missing input to the key".to_string(),
                evidence: "same key, different value_blake3 in the cache log".to_string(),
            });
        }
    }
    hints
}

fn print_run(term: &Term, rd: &RunData) {
    let st = term.style;
    let status = if rd.outcome.ok() {
        st.green("ok  ")
    } else {
        st.red("FAIL")
    };
    let peak = rd
        .info
        .peak_rss_mb
        .map(|m| format!("   {m} MB peak"))
        .unwrap_or_default();
    term.line(&format!(
        "{:<7} {status}   {}   {} files{peak}",
        rd.info.label,
        util::fmt_secs(rd.info.wall_ms),
        rd.info.files
    ));
}

fn print_summary(term: &Term, r: &CheckReport) {
    let st = term.style;
    let o = &r.outputs;
    let matched = o.identical + o.identical_after_normalization;
    let mut line = format!("{matched}/{} identical", o.total);
    if o.identical_after_normalization > 0 {
        line.push_str(&format!(
            " ({} after normalization)",
            o.identical_after_normalization
        ));
    }
    if matched < o.total {
        line.push_str(&format!(
            " ({} differ, {} in one run only)",
            o.different, o.one_sided
        ));
        line = st.red(&line);
    }
    term.line(&format!("outputs        {line}"));

    let ch = &r.config_hash_check;
    let ch_line = match ch.status.as_str() {
        "ok" => st.green("recorded and matches"),
        "wrong" => st.red("not recorded or wrong"),
        "not_recorded" => st.yellow("not recorded (no run_meta.json with config_hash)"),
        _ => st.dim("skipped (no [project].config)"),
    };
    term.line(&format!("config hash    {ch_line}"));

    if let Some(c) = &r.cache {
        let per_run: Vec<String> = c
            .runs
            .iter()
            .map(|s| {
                format!(
                    "{} {} ev ({} hit / {} miss)",
                    s.label.replace(' ', ""),
                    s.events,
                    s.hits,
                    s.misses
                )
            })
            .collect();
        let tail = if !c.ok() {
            st.red(&format!("{} issue(s)", c.issues.len()))
        } else if c.same_sequence == Some(true) {
            "same sequence".to_string()
        } else {
            st.green("ok")
        };
        term.line(&format!("cache log      {}   {tail}", per_run.join("   ")));
        for i in &c.issues {
            term.line(&format!("  {} {}", st.red("✗"), i.message));
            for d in i.details.iter().take(3) {
                term.line(&st.dim(&format!("      {d}")));
            }
        }
    }
    if let Some(s) = &r.seed_check {
        let txt = if s.ok {
            st.green(&format!(
                "seed+1 changes outputs (ok, {} file(s))",
                s.changed_files
            ))
        } else {
            st.red("seed+1 gives identical outputs: the seed is ignored somewhere")
        };
        term.line(&format!("seed check     {txt}"));
    }
    for f in &r.float_close {
        term.line(&st.yellow(&format!(
            "float-tol      {} {}: {} cell(s) within {:e} — numerically close but NOT byte-identical",
            f.path, f.field, f.cells, f.max_abs_diff
        )));
    }

    // First failing file with its localized diff.
    if let Some((c, m)) = r
        .comparisons
        .iter()
        .find_map(|c| c.mismatches.first().map(|m| (c, m)))
    {
        term.line(&st.bold(&format!(
            "first difference  {} vs {}: {} ({})",
            c.a,
            c.b,
            m.path,
            m.status.label()
        )));
        if let Some(loc) = &m.localization {
            for l in report::describe_localization(loc) {
                term.line(&format!("  {l}"));
            }
            if let Some(d) = &loc.unified_diff {
                for l in d.lines() {
                    let colored = if l.starts_with('+') && !l.starts_with("+++") {
                        st.green(l)
                    } else if l.starts_with('-') && !l.starts_with("---") {
                        st.red(l)
                    } else {
                        st.dim(l)
                    };
                    term.line(&format!("    {colored}"));
                }
            }
        }
    }
    if !r.hints.is_empty() {
        term.line(&st.bold("likely causes"));
        for h in &r.hints {
            term.line(&format!("  - {}  [{}]", h.cause, h.evidence));
            term.line(&st.dim(&format!("    fix: {}", h.fix)));
        }
    }
}

/// Top-level directories the input walk must not enter.
fn skip_dirs(root: &Path, own: &[&Path]) -> Vec<String> {
    let mut v = vec![".git".to_string(), ".repro-cache".to_string()];
    for p in own {
        if let Ok(rel) = p.strip_prefix(root) {
            v.push(util::to_slash(rel));
        }
    }
    v
}

fn prepare_out(out: &Path, force: bool) -> Result<()> {
    if out.exists() {
        if !out.is_dir() {
            bail!("--out {} exists and is not a directory", out.display());
        }
        let foreign: BTreeSet<String> = std::fs::read_dir(out)
            .with_context(|| format!("cannot list {}", out.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !OWN_OUT_ENTRIES.contains(&n.as_str()))
            .collect();
        if !foreign.is_empty() && !force {
            bail!(
                "--out {} already contains files repro did not write ({}); choose another --out or pass --force",
                out.display(),
                foreign.into_iter().take(5).collect::<Vec<_>>().join(", ")
            );
        }
        // Old per-run manifests are ours (marker present) and would be stale.
        let manifests = out.join("manifests");
        if out.join(OUT_MARKER).is_file() && manifests.is_dir() {
            std::fs::remove_dir_all(&manifests)
                .with_context(|| format!("cannot clear {}", manifests.display()))?;
        }
    }
    std::fs::create_dir_all(out.join("manifests"))
        .with_context(|| format!("cannot create {}", out.display()))?;
    std::fs::write(
        out.join(OUT_MARKER),
        "written by repro; files listed in repro's report may be overwritten\n",
    )?;
    Ok(())
}

fn prepare_work(work: &Path) -> Result<()> {
    if work.exists() {
        if !work.join(WORK_MARKER).is_file() {
            bail!(
                "work directory {} exists but was not created by repro; remove it or pass --work-dir",
                work.display()
            );
        }
        std::fs::remove_dir_all(work)
            .with_context(|| format!("cannot clear old work directory {}", work.display()))?;
    }
    std::fs::create_dir_all(work).with_context(|| format!("cannot create {}", work.display()))?;
    std::fs::write(
        work.join(WORK_MARKER),
        "created by repro; this directory is deleted and recreated on every `repro check`\n",
    )?;
    Ok(())
}
