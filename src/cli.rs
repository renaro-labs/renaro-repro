//! Command-line interface (clap derive).

use crate::cachelog::CacheMode;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "repro",
    version,
    about = "Reproducibility checker: runs your experiment command N times and proves the outputs are byte-identical",
    after_help = "Exit codes: 0 identical, 1 different, 2 tool/usage/config error, 3 the command failed or timed out."
)]
pub struct Cli {
    /// Run as if repro was started in DIR (the project root holding repro.toml)
    #[arg(short = 'C', long = "dir", global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Write a commented sample repro.toml
    Init,
    /// Run the command N times with the same seed and compare all outputs
    Check(CheckArgs),
    /// Make a manifest (path, size, BLAKE3) of files and directories
    Hash(HashArgs),
    /// Compare files and directories to a golden manifest
    Verify(VerifyArgs),
    /// Compare two manifests or two directories
    Diff(DiffArgs),
    /// Analyze cache logs (JSONL) on their own
    CacheLog(CacheLogArgs),
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Number of identical-seed runs
    #[arg(long, default_value_t = 2)]
    pub runs: u32,
    /// Global seed passed to the command (default: [project].seed in repro.toml)
    #[arg(long)]
    pub seed: Option<u64>,
    /// cold-cold: every run gets an empty cache; cold-warm: run 2+ reuse run 1's cache
    #[arg(long, value_enum, default_value = "cold-cold")]
    pub cache_mode: CacheMode,
    /// Also run once with seed+1 and require that at least one output changes
    #[arg(long)]
    pub seed_sensitivity: bool,
    /// Report numeric fields differing by less than X (information only, never changes the verdict)
    #[arg(long, value_name = "X")]
    pub float_tol: Option<f64>,
    /// Run the command with only allowlisted env vars ([run].env_allow) plus PATH/HOME/TEMP basics
    #[arg(long)]
    pub strict_env: bool,
    /// Keep run directories (default: keep only when the verdict is not identical)
    #[arg(long)]
    pub keep: bool,
    /// Report directory
    #[arg(long, default_value = "repro-report")]
    pub out: PathBuf,
    /// Write into an --out directory that already contains other files
    #[arg(long)]
    pub force: bool,
    /// Work directory for runs (created and owned by repro)
    #[arg(long, default_value = ".repro-work")]
    pub work_dir: PathBuf,
    /// Per-run timeout in seconds (default: [project].timeout_s)
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,
    /// Print the machine-readable summary (report.json) to stdout
    #[arg(long)]
    pub json: bool,
    /// The command to run (default: [run].command). Placeholders: {run_dir} {seed} {run_id} {cache_dir} {config}
    #[arg(last = true, value_name = "CMD")]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct HashArgs {
    /// Files or directories to hash
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Manifest to write
    #[arg(long)]
    pub out: PathBuf,
    /// Manifest paths are relative to this directory
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// Only include files matching these globs
    #[arg(long)]
    pub include: Vec<String>,
    /// Skip files matching these globs
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Also record provenance (git, OS, CPU, GPUs) in the manifest
    #[arg(long)]
    pub provenance: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Files or directories to verify
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Golden manifest
    #[arg(long)]
    pub against: PathBuf,
    /// Manifest paths are relative to this directory
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Manifest or directory
    pub a: PathBuf,
    /// Manifest or directory
    pub b: PathBuf,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CacheLogArgs {
    /// Cache logs, one per run, in run order
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
    /// Also check mode expectations across the logs
    #[arg(long, value_enum)]
    pub mode: Option<CacheMode>,
    /// Required key parts (default: [cache].required_key_parts, else state_hash,expert,schema_version,seed)
    #[arg(long, value_delimiter = ',')]
    pub required_key_parts: Option<Vec<String>>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}
