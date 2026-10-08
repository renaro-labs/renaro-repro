//! repro: a reproducibility checker. Runs the user's experiment command several times and
//! judges whether the outputs are byte-identical. Never touches the network.
//!
//! Exit codes: 0 identical, 1 different, 2 tool/usage/config error, 3 the command failed.

mod cachelog;
mod check;
mod cli;
mod commands;
mod compare;
mod config;
mod hashing;
mod hints;
mod manifest;
mod normalize;
mod provenance;
mod report;
mod runner;
mod util;

use clap::Parser;
use cli::{Cli, Cmd};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // --help and --version print to stdout and are not errors.
            let code = if e.use_stderr() { check::EXIT_ERROR } else { 0 };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    if let Some(dir) = &cli.dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("error: cannot change to directory {}: {e}", dir.display());
            return ExitCode::from(check::EXIT_ERROR);
        }
    }
    let result = match cli.command {
        Cmd::Init => commands::init(),
        Cmd::Check(a) => check::run(a),
        Cmd::Hash(a) => commands::hash(a),
        Cmd::Verify(a) => commands::verify(a),
        Cmd::Diff(a) => commands::diff(a),
        Cmd::CacheLog(a) => commands::cache_log(a),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(check::EXIT_ERROR)
        }
    }
}
