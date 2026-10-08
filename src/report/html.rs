//! `report.html`: one self-contained file (inline CSS, a little vanilla JS, inline SVG).
//! No external requests of any kind, so it opens offline by double-click.

use super::describe_localization;
use crate::cachelog::CacheReport;
use crate::check::CheckReport;
use crate::compare::FileStatus;
use crate::util;
use std::fmt::Write;

const TEMPLATE: &str = include_str!("template.html");

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

pub fn render(r: &CheckReport) -> String {
    let title = format!(
        "repro: {} — {}",
        r.project.as_deref().unwrap_or("check"),
        r.verdict.label
    );
    TEMPLATE
        .replace("{{TITLE}}", &esc(&title))
        .replace("<!--BODY-->", &body(r))
}

fn body(r: &CheckReport) -> String {
    let mut h = String::new();
    let _ = writeln!(
        h,
        "<h1>repro report{}</h1>",
        r.project
            .as_deref()
            .map(|p| format!(": {}", esc(p)))
            .unwrap_or_default()
    );
    let _ = writeln!(
        h,
        "<div class=\"meta\"><code>{}</code><br>config {} · hash <code>{}</code> · seed {} · {} runs · cache mode {} · repro {}{}</div>",
        esc(&r.command.join(" ")),
        esc(r.config_path.as_deref().unwrap_or("-")),
        esc(r.config_hash.as_deref().unwrap_or("-")),
        r.seed,
        r.runs_requested,
        esc(&r.cache_mode),
        esc(&r.tool_version),
        if r.strict_env { " · strict env" } else { "" }
    );

    // Verdict banner.
    let _ = write!(
        h,
        "<div class=\"banner {}\">VERDICT: {}",
        esc(&r.verdict.status),
        esc(&r.verdict.label)
    );
    if !r.verdict.failures.is_empty() {
        h.push_str("<ul>");
        for f in &r.verdict.failures {
            let _ = write!(h, "<li>{}</li>", esc(f));
        }
        h.push_str("</ul>");
    }
    h.push_str("</div>\n");
    for w in &r.warnings {
        let _ = writeln!(h, "<p class=\"warn\">warning: {}</p>", esc(w));
    }

    // Hints first: they are what the reader acts on.
    if !r.hints.is_empty() {
        h.push_str("<h2>Likely causes</h2>\n");
        for x in &r.hints {
            let _ = writeln!(
                h,
                "<div class=\"hint\"><strong>{}</strong> <span class=\"meta\">({})</span><div class=\"fix\">fix: {}</div></div>",
                esc(&x.cause),
                esc(&x.evidence),
                esc(&x.fix)
            );
        }
    }

    // Runs.
    h.push_str("<h2>Runs</h2>\n<div class=\"scroll\"><table><thead><tr><th>run</th><th>status</th><th>wall</th><th>files</th><th>peak RSS</th><th>run id</th><th>stdout / stderr BLAKE3</th></tr></thead><tbody>\n");
    for run in r.runs.iter().chain(r.seed_run.iter()) {
        let status = match &run.error {
            None => "<span class=\"ok\">ok</span>".to_string(),
            Some(e) => format!("<span class=\"bad\">{}</span>", esc(e)),
        };
        let _ = writeln!(
            h,
            "<tr><td>{}</td><td>{status}</td><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td><td><code>{}</code> / <code>{}</code></td></tr>",
            esc(&run.label),
            util::fmt_secs(run.wall_ms),
            run.files,
            run.peak_rss_mb.map_or("-".to_string(), |v| format!("{v} MB")),
            esc(&run.run_id),
            util::short(&run.stdout_blake3),
            util::short(&run.stderr_blake3)
        );
    }
    h.push_str("</tbody></table></div>\n");
    for run in r.runs.iter().chain(r.seed_run.iter()) {
        if run.stderr_tail.is_empty() && run.stdout_tail.is_empty() {
            continue;
        }
        let _ = writeln!(
            h,
            "<details{}><summary>{}: last output lines</summary>",
            if run.ok { "" } else { " open" },
            esc(&run.label)
        );
        if !run.stdout_tail.is_empty() {
            let _ = writeln!(
                h,
                "<div class=\"meta\">stdout</div><pre>{}</pre>",
                esc(&run.stdout_tail.join("\n"))
            );
        }
        if !run.stderr_tail.is_empty() {
            let _ = writeln!(
                h,
                "<div class=\"meta\">stderr</div><pre>{}</pre>",
                esc(&run.stderr_tail.join("\n"))
            );
        }
        h.push_str("</details>\n");
    }

    // Checks.
    h.push_str("<h2>Checks</h2>\n<div class=\"scroll\"><table><tbody>\n");
    let o = &r.outputs;
    if o.total > 0 {
        let cls = if o.different + o.one_sided == 0 {
            "ok"
        } else {
            "bad"
        };
        let _ = writeln!(
            h,
            "<tr><th>outputs</th><td class=\"{cls}\">{}/{} identical ({} after normalization, {} different, {} in one run only)</td></tr>",
            o.identical + o.identical_after_normalization,
            o.total,
            o.identical_after_normalization,
            o.different,
            o.one_sided
        );
    }
    let ch_cls = match r.config_hash_check.status.as_str() {
        "ok" => "ok",
        "wrong" => "bad",
        "not_recorded" => "warn",
        _ => "meta",
    };
    let _ = writeln!(
        h,
        "<tr><th>config hash</th><td class=\"{ch_cls}\">{}</td></tr>",
        esc(&r.config_hash_check.message)
    );
    if let Some(s) = &r.seed_check {
        let _ = writeln!(
            h,
            "<tr><th>seed check</th><td class=\"{}\">{}</td></tr>",
            if s.ok { "ok" } else { "bad" },
            if s.ok {
                format!("seed {} changes {} file(s) (ok)", s.seed, s.changed_files)
            } else {
                format!(
                    "seed {} gives identical outputs: the seed is ignored somewhere",
                    s.seed
                )
            }
        );
    }
    for f in &r.float_close {
        let _ = writeln!(
            h,
            "<tr><th>float-tol</th><td class=\"warn\">{} <code>{}</code> {}: {} cell(s), max abs diff {:e}: numerically close but NOT byte-identical</td></tr>",
            esc(&f.comparison),
            esc(&f.path),
            esc(&f.field),
            f.cells,
            f.max_abs_diff
        );
    }
    h.push_str("</tbody></table></div>\n");

    // Files: mismatches first, filterable.
    if !r.files.is_empty() {
        h.push_str("<h2>Files</h2>\n<div class=\"controls\"><input type=\"search\" id=\"file-filter\" placeholder=\"Filter by path\"><label><input type=\"checkbox\" id=\"file-only-bad\"> only mismatches</label></div>\n");
        h.push_str("<div class=\"scroll\"><table id=\"file-table\"><thead><tr><th>path</th><th>status</th><th>size (run 1)</th><th>BLAKE3 (run 1)</th><th>normalization</th></tr></thead><tbody>\n");
        let mut files: Vec<_> = r.files.iter().collect();
        files.sort_by_key(|f| (f.status.is_match(), f.path.clone()));
        for f in files {
            let status = serde_json::to_value(f.status)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            let rules: Vec<String> = f
                .rules
                .iter()
                .filter_map(|i| r.normalizations.get(*i).map(|n| n.description.clone()))
                .collect();
            let _ = writeln!(
                h,
                "<tr data-path=\"{}\" data-bad=\"{}\"><td><code>{}</code></td><td class=\"s-{status}\">{}</td><td>{}</td><td><code title=\"{}\">{}</code></td><td>{}</td></tr>",
                esc(&f.path),
                if f.status.is_match() { "0" } else { "1" },
                esc(&f.path),
                f.status.label(),
                f.size.map_or("-".to_string(), util::fmt_bytes),
                esc(f.blake3.as_deref().unwrap_or("")),
                util::short(f.blake3.as_deref().unwrap_or("-")),
                esc(&rules.join("; "))
            );
        }
        h.push_str("</tbody></table></div>\n");
    }

    // Diff viewer for the first mismatches.
    let mismatches: Vec<_> = r
        .comparisons
        .iter()
        .flat_map(|c| c.mismatches.iter().map(move |m| (c, m)))
        .take(10)
        .collect();
    if !mismatches.is_empty() {
        h.push_str("<h2>Differences</h2>\n");
        for (i, (c, m)) in mismatches.iter().enumerate() {
            let _ = writeln!(
                h,
                "<details{}><summary>{} vs {}: <code>{}</code> — {}</summary>",
                if i < 3 { " open" } else { "" },
                esc(&c.a),
                esc(&c.b),
                esc(&m.path),
                m.status.label()
            );
            if let Some(loc) = &m.localization {
                let _ = writeln!(
                    h,
                    "<pre>{}</pre>",
                    esc(&describe_localization(loc).join("\n"))
                );
                if let Some(d) = &loc.unified_diff {
                    h.push_str("<pre>");
                    for l in d.lines() {
                        let cls = if l.starts_with("@@")
                            || l.starts_with("---")
                            || l.starts_with("+++")
                        {
                            "hunk"
                        } else if l.starts_with('+') {
                            "add"
                        } else if l.starts_with('-') {
                            "del"
                        } else {
                            ""
                        };
                        let _ = writeln!(h, "<span class=\"{cls}\">{}</span>", esc(l));
                    }
                    h.push_str("</pre>");
                }
            } else if m.status == FileStatus::Different {
                h.push_str(
                    "<p class=\"meta\">not localized (only the first files per comparison are)</p>",
                );
            }
            h.push_str("</details>\n");
        }
    }

    if let Some(c) = &r.cache {
        cache_section(&mut h, c);
    }

    // Declared normalizations.
    h.push_str("<h2>Declared normalizations</h2>\n");
    if r.normalizations.is_empty() {
        h.push_str("<p class=\"meta\">none declared: every output is compared byte for byte</p>\n");
    } else {
        h.push_str("<div class=\"scroll\"><table><thead><tr><th>files</th><th>rule</th><th>reason</th><th>applies to</th><th>needed for</th></tr></thead><tbody>\n");
        for n in &r.normalizations {
            let _ = writeln!(
                h,
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{} file(s)</td><td>{}</td></tr>",
                esc(&n.files),
                esc(&n.description),
                esc(&n.reason),
                n.applied_to.len(),
                if n.needed_for.is_empty() {
                    "-".to_string()
                } else {
                    esc(&n.needed_for.join(", "))
                }
            );
        }
        h.push_str("</tbody></table></div>\n");
    }

    // Provenance.
    let p = &r.provenance;
    h.push_str("<h2>Provenance</h2>\n<div class=\"scroll\"><table><tbody>\n");
    let row = |h: &mut String, k: &str, v: String| {
        let _ = writeln!(h, "<tr><th>{k}</th><td>{v}</td></tr>");
    };
    row(
        &mut h,
        "git",
        match &p.git_commit {
            Some(c) => format!(
                "<code>{}</code>{}",
                esc(c),
                if p.git_dirty == Some(true) {
                    " <span class=\"warn\">(dirty)</span>"
                } else {
                    ""
                }
            ),
            None => "-".to_string(),
        },
    );
    row(&mut h, "os", esc(&p.os));
    row(&mut h, "cpu threads", p.cpu_threads.to_string());
    row(
        &mut h,
        "gpus",
        p.gpus
            .as_ref()
            .map_or("-".to_string(), |g| esc(&g.join(", "))),
    );
    row(
        &mut h,
        "env (allowlisted)",
        if p.env.is_empty() {
            "-".to_string()
        } else {
            p.env
                .iter()
                .map(|(k, v)| format!("<code>{}={}</code>", esc(k), esc(v)))
                .collect::<Vec<_>>()
                .join(" ")
        },
    );
    row(&mut h, "work dir", esc(&r.work_dir));
    h.push_str("</tbody></table></div>\n");
    if !p.inputs.is_empty() {
        let _ = writeln!(
            h,
            "<details><summary>{} input file(s) hashed</summary><div class=\"scroll\"><table><thead><tr><th>path</th><th>size</th><th>BLAKE3</th></tr></thead><tbody>",
            p.inputs.len()
        );
        for i in &p.inputs {
            let _ = writeln!(
                h,
                "<tr><td><code>{}</code></td><td>{}</td><td><code>{}</code></td></tr>",
                esc(&i.path),
                util::fmt_bytes(i.size),
                esc(&i.blake3)
            );
        }
        h.push_str("</tbody></table></div></details>\n");
    }
    h
}

fn cache_section(h: &mut String, c: &CacheReport) {
    h.push_str("<h2>Cache</h2>\n");
    let _ = writeln!(
        h,
        "<p class=\"meta\">mode {} · required key parts: {}{}</p>",
        esc(&c.mode),
        esc(&c.required_key_parts.join(", ")),
        match c.same_sequence {
            Some(true) => " · same hit/miss sequence in every run",
            Some(false) => " · hit/miss sequences differ",
            None => "",
        }
    );
    // Inline SVG: hits vs misses per run.
    let max = c
        .runs
        .iter()
        .map(|s| s.hits.max(s.misses))
        .max()
        .unwrap_or(0)
        .max(1);
    let (label_w, bar_w, row_h) = (70.0, 360.0, 34.0);
    let height = row_h * c.runs.len() as f64 + 8.0;
    let _ = writeln!(
        h,
        "<svg role=\"img\" aria-label=\"cache hits and misses per run\" width=\"100%\" viewBox=\"0 0 {} {height}\" style=\"max-width:560px\">",
        label_w + bar_w + 90.0
    );
    for (i, s) in c.runs.iter().enumerate() {
        let y = i as f64 * row_h + 4.0;
        let hw = bar_w * s.hits as f64 / max as f64;
        let mw = bar_w * s.misses as f64 / max as f64;
        let _ = writeln!(
            h,
            "<text x=\"0\" y=\"{:.1}\">{}</text><rect class=\"bar-hit\" x=\"{label_w}\" y=\"{y:.1}\" width=\"{hw:.1}\" height=\"12\"></rect><text x=\"{:.1}\" y=\"{:.1}\">{} hit</text><rect class=\"bar-miss\" x=\"{label_w}\" y=\"{:.1}\" width=\"{mw:.1}\" height=\"12\"></rect><text x=\"{:.1}\" y=\"{:.1}\">{} miss</text>",
            y + 18.0,
            esc(&s.label),
            label_w + hw + 6.0,
            y + 10.0,
            s.hits,
            y + 14.0,
            label_w + mw + 6.0,
            y + 24.0,
            s.misses
        );
    }
    h.push_str("</svg>\n<div class=\"meta legend\"><span class=\"bar-hit\" style=\"background:var(--hit)\"></span>hits<span style=\"background:var(--miss)\"></span>misses</div>\n");

    h.push_str("<div class=\"scroll\"><table><thead><tr><th>run</th><th>events</th><th>hits</th><th>misses</th><th>stores</th><th>hit rate</th><th>top missed experts</th></tr></thead><tbody>\n");
    for s in &c.runs {
        let _ = writeln!(
            h,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1}%</td><td>{}</td></tr>",
            esc(&s.label),
            s.events,
            s.hits,
            s.misses,
            s.stores,
            s.hit_rate * 100.0,
            esc(&s
                .top_missed_experts
                .iter()
                .map(|e| format!("{} ({})", e.expert, e.misses))
                .collect::<Vec<_>>()
                .join(", "))
        );
    }
    h.push_str("</tbody></table></div>\n");
    if c.issues.is_empty() {
        h.push_str("<p class=\"ok\">all cache checks passed</p>\n");
    }
    for i in &c.issues {
        let _ = write!(
            h,
            "<p class=\"bad\"><code>{}</code> {}</p>",
            esc(&i.check),
            esc(&i.message)
        );
        if !i.details.is_empty() {
            let _ = writeln!(h, "<pre>{}</pre>", esc(&i.details.join("\n")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_has_no_external_references() {
        let t = TEMPLATE.to_ascii_lowercase();
        assert!(!t.contains("http"), "template must not reference any URL");
        assert!(!t.contains("src="));
        assert!(!t.contains("@import"));
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
