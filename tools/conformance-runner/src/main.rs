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
    // Flags: `--write` (regenerate transcript sidecars), `--c-host <binary>` (drive fixtures through
    // the example C host, M7.5e). The one positional is the suite root (default `conformance`).
    let mut write = false;
    let mut c_host: Option<&str> = None;
    let mut root = "conformance";
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--write" => write = true,
            "--c-host" => {
                c_host = args.get(i + 1).map(String::as_str);
                i += 1;
            }
            other => root = other,
        }
        i += 1;
    }
    let result = if let Some(c_host) = c_host {
        conformance_runner::run_c_host(Path::new(root), Path::new(c_host))
    } else if write {
        conformance_runner::write(Path::new(root)).map(|_| 0)
    } else {
        conformance_runner::run(Path::new(root))
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
