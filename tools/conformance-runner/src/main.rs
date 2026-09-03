//! The `conformance-runner` binary: a thin wrapper over the [`conformance_runner`] library
//! (D-M7-19). Takes the suite root as the first argument (default `conformance`), runs the suite
//! in-process, and maps its failed-test count to the process exit code — the same [`run`] the
//! `doodle test` subcommand links (E§11 cross-surface parity).
//!
//! [`run`]: conformance_runner::run

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "conformance".to_string());
    match conformance_runner::run(Path::new(&root)) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("conformance-runner: {message}");
            ExitCode::FAILURE
        }
    }
}
