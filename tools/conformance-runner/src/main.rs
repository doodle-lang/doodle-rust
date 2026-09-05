//! The `conformance-runner` binary: a thin wrapper over the [`conformance_runner`] library
//! (D-M7-19). Takes the suite root as the first positional argument (default `conformance`), runs
//! the suite in-process, and maps its failed-test count to the process exit code — the same [`run`]
//! the `doodle test` subcommand links (E§11 cross-surface parity). With `--write` it regenerates the
//! committed transcript sidecars ([`write`], the M7.5d oracle) instead of checking them.
//!
//! [`run`]: conformance_runner::run
//! [`write`]: conformance_runner::write

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let write = args.iter().any(|a| a == "--write");
    let root = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "conformance".to_string());
    let result = if write {
        conformance_runner::write(Path::new(&root)).map(|_| 0)
    } else {
        conformance_runner::run(Path::new(&root))
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("conformance-runner: {message}");
            ExitCode::FAILURE
        }
    }
}
