//! `report.json`: the full machine-readable result. The schema is versioned by `repro_schema`.

use crate::check::CheckReport;
use anyhow::Result;

pub fn render(r: &CheckReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(r)?)
}
