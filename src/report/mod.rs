//! Report writers: `report.json` (machine-readable), `report.md` (paste into a PR / Jira),
//! `report.html` (single self-contained file, works offline).

pub mod html;
pub mod json;
pub mod md;

use crate::check::CheckReport;
use crate::compare::Localization;
use anyhow::{Context, Result};
use std::path::Path;

pub fn write_all(r: &CheckReport, out: &Path) -> Result<()> {
    let write = |name: &str, text: String| -> Result<()> {
        let p = out.join(name);
        std::fs::write(&p, text).with_context(|| format!("cannot write {}", p.display()))
    };
    write("report.json", json::render(r)? + "\n")?;
    write("report.md", md::render(r))?;
    write("report.html", html::render(r))?;
    Ok(())
}

/// Human-readable lines describing where a file differs (shared by terminal, Markdown, HTML).
pub fn describe_localization(loc: &Localization) -> Vec<String> {
    let mut v = Vec::new();
    let norm = if loc.normalized {
        " (after declared normalization)"
    } else {
        ""
    };
    if let Some(off) = loc.byte_offset {
        v.push(format!(
            "first differing byte: offset {off}{norm}; 32 bytes from offset {}:",
            loc.context_start.unwrap_or(0)
        ));
        if let Some(a) = &loc.context_a {
            v.push(format!("  A: {a}"));
        }
        if let Some(b) = &loc.context_b {
            v.push(format!("  B: {b}"));
        }
    }
    if let Some(n) = loc.line {
        v.push(format!("first differing line: {n}"));
    }
    if loc.line_ending_only {
        v.push("only the line ending differs (CRLF vs LF)".to_string());
    }
    if let Some(p) = &loc.json_path {
        v.push(format!("first differing key: {p}"));
    }
    if !loc.record_diff.is_empty() {
        v.push("first differing record, per key:".to_string());
        for k in loc.record_diff.iter().take(8) {
            v.push(format!(
                "  {}: {} -> {}",
                k.key,
                k.a.as_deref().unwrap_or("(absent)"),
                k.b.as_deref().unwrap_or("(absent)")
            ));
        }
    }
    if loc.parsed_equal == Some(true) {
        v.push(
            "the JSON values are equal once parsed: only key order or formatting differ"
                .to_string(),
        );
    }
    if loc.same_records_different_order == Some(true) {
        v.push("same records, different order".to_string());
    }
    for n in loc.numeric.iter().take(5) {
        if n.differing > 0 {
            v.push(format!(
                "field {}: {} numeric cell(s) differ (max abs {:.3e}, max rel {:.3e}){}",
                n.field,
                n.differing,
                n.max_abs_diff,
                n.max_rel_diff,
                if n.non_numeric_diffs > 0 {
                    format!(", {} non-numeric", n.non_numeric_diffs)
                } else {
                    String::new()
                }
            ));
        } else {
            v.push(format!(
                "field {}: {} non-numeric difference(s)",
                n.field, n.non_numeric_diffs
            ));
        }
    }
    if let Some(p) = &loc.parquet {
        v.push(format!(
            "parquet: rows {} vs {}{}; differing columns: {}; row groups: {}",
            p.rows_a,
            p.rows_b,
            if p.schema_differs {
                "; schema differs"
            } else {
                ""
            },
            if p.columns.is_empty() {
                "-".to_string()
            } else {
                p.columns.join(", ")
            },
            p.row_groups
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(row) = p.first_row {
            v.push(format!("first differing row: {row}"));
            if let Some(a) = &p.first_row_a {
                v.push(format!("  A: {a}"));
            }
            if let Some(b) = &p.first_row_b {
                v.push(format!("  B: {b}"));
            }
        }
    }
    for n in &loc.notes {
        v.push(format!("note: {n}"));
    }
    v
}
