//! `report.md`: short enough to paste into a Jira comment or a pull request.

use super::describe_localization;
use crate::check::CheckReport;
use crate::util;
use std::fmt::Write;

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

pub fn render(r: &CheckReport) -> String {
    let mut m = String::new();
    let icon = match r.verdict.status.as_str() {
        "IDENTICAL" => "✅",
        "DIFFERENT" => "❌",
        _ => "⚠️",
    };
    let _ = writeln!(m, "## {icon} repro: {}\n", r.verdict.label);
    let _ = writeln!(
        m,
        "`{}` · project **{}** · config `{}` · seed {} · {} runs · cache mode {} · repro {}\n",
        cell(&r.command.join(" ")),
        r.project.as_deref().unwrap_or("-"),
        r.config_hash.as_deref().map_or("-", util::short),
        r.seed,
        r.runs_requested,
        r.cache_mode,
        r.tool_version
    );
    if !r.verdict.failures.is_empty() {
        let _ = writeln!(m, "**Failures**\n");
        for f in &r.verdict.failures {
            let _ = writeln!(m, "- {}", cell(f));
        }
        m.push('\n');
    }

    let _ = writeln!(m, "| run | status | wall | files | peak RSS | run id |");
    let _ = writeln!(m, "|---|---|---|---|---|---|");
    for run in r.runs.iter().chain(r.seed_run.iter()) {
        let _ = writeln!(
            m,
            "| {} | {} | {} | {} | {} | `{}` |",
            run.label,
            run.error.as_deref().map_or("ok".to_string(), cell),
            util::fmt_secs(run.wall_ms),
            run.files,
            run.peak_rss_mb
                .map_or("-".to_string(), |v| format!("{v} MB")),
            run.run_id
        );
    }
    m.push('\n');

    let o = &r.outputs;
    if o.total > 0 {
        let _ = writeln!(
            m,
            "**Outputs:** {}/{} identical ({} after normalization, {} different, {} in one run only)\n",
            o.identical + o.identical_after_normalization,
            o.total,
            o.identical_after_normalization,
            o.different,
            o.one_sided
        );
    }
    let _ = writeln!(
        m,
        "**Config hash:** {} — {}\n",
        r.config_hash_check.status,
        cell(&r.config_hash_check.message)
    );
    if let Some(c) = &r.cache {
        let runs: Vec<String> = c
            .runs
            .iter()
            .map(|s| {
                format!(
                    "{}: {} events ({} hit / {} miss, hit rate {:.0}%)",
                    s.label,
                    s.events,
                    s.hits,
                    s.misses,
                    s.hit_rate * 100.0
                )
            })
            .collect();
        let _ = writeln!(m, "**Cache log ({}):** {}\n", c.mode, runs.join("; "));
        for i in &c.issues {
            let _ = writeln!(m, "- ❌ `{}` {}", i.check, cell(&i.message));
            for d in i.details.iter().take(3) {
                let _ = writeln!(m, "  - {}", cell(d));
            }
        }
        if !c.issues.is_empty() {
            m.push('\n');
        }
    }
    if let Some(s) = &r.seed_check {
        let _ = writeln!(
            m,
            "**Seed check:** {}\n",
            if s.ok {
                format!("seed+1 changes {} file(s) (ok)", s.changed_files)
            } else {
                "seed+1 gives identical outputs: the seed is ignored somewhere".to_string()
            }
        );
    }

    let mut shown = 0;
    for c in &r.comparisons {
        for f in &c.mismatches {
            if shown >= 3 {
                break;
            }
            shown += 1;
            let _ = writeln!(
                m,
                "<details><summary>{} vs {}: <code>{}</code> ({})</summary>\n",
                c.a,
                c.b,
                f.path,
                f.status.label()
            );
            let _ = writeln!(m, "```");
            if let Some(loc) = &f.localization {
                for l in describe_localization(loc) {
                    let _ = writeln!(m, "{l}");
                }
                if let Some(d) = &loc.unified_diff {
                    let _ = writeln!(m, "{d}");
                }
            }
            let _ = writeln!(m, "```\n</details>\n");
        }
    }

    if !r.hints.is_empty() {
        let _ = writeln!(m, "**Likely causes**\n");
        for h in &r.hints {
            let _ = writeln!(
                m,
                "- {} — fix: {} _({})_",
                cell(&h.cause),
                cell(&h.fix),
                cell(&h.evidence)
            );
        }
        m.push('\n');
    }
    if !r.float_close.is_empty() {
        let _ = writeln!(m, "**Numerically close but NOT byte-identical**\n");
        for f in &r.float_close {
            let _ = writeln!(
                m,
                "- `{}` {}: {} cell(s), max abs diff {:e}",
                f.path, f.field, f.cells, f.max_abs_diff
            );
        }
        m.push('\n');
    }
    let used: Vec<_> = r
        .normalizations
        .iter()
        .filter(|n| !n.needed_for.is_empty())
        .collect();
    if !used.is_empty() {
        let _ = writeln!(m, "**Declared normalizations used**\n");
        for n in used {
            let _ = writeln!(
                m,
                "- `{}` — {} (reason: {}; needed for {})",
                n.files,
                cell(&n.description),
                cell(&n.reason),
                n.needed_for.join(", ")
            );
        }
        m.push('\n');
    }
    let p = &r.provenance;
    let _ = writeln!(
        m,
        "<sub>git {}{} · {} · {} threads{} · {} input file(s) hashed</sub>",
        p.git_commit
            .as_deref()
            .map_or("-", |c| &c[..c.len().min(12)]),
        if p.git_dirty == Some(true) {
            " (dirty)"
        } else {
            ""
        },
        p.os,
        p.cpu_threads,
        p.gpus
            .as_ref()
            .map(|g| format!(" · {}", g.join(", ")))
            .unwrap_or_default(),
        p.inputs.len()
    );
    m
}
