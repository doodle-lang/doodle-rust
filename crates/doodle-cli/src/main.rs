//! The `doodle` command-line host (AD7). Two subcommands: `run` a program (`run.rs`), or `test`
//! the conformance suite (linking the [`conformance_runner`] library in-process, D-M7-19).
//! Arguments are hand-parsed (`std::env::args`) — the small `run`/`test` grammar needs no
//! dependency, matching the audited minimal-dep house style. Exit codes: `0` success, `1` a program
//! error (raise/fault/load error) or a failing suite, `2` a usage error.

mod draw;
mod rng;
mod run;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(dispatch(&args))
}

/// Dispatches on the subcommand, returning the process exit code.
fn dispatch(args: &[String]) -> u8 {
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("test") => cmd_test(&args[1..]),
        Some("--help" | "-h") => {
            print!("{USAGE}");
            0
        }
        Some("--version" | "-V") => {
            println!("doodle {}", env!("CARGO_PKG_VERSION"));
            0
        }
        None => {
            eprint!("{USAGE}");
            2
        }
        Some(other) => {
            eprintln!("doodle: unknown command `{other}`");
            eprint!("{USAGE}");
            2
        }
    }
}

/// `doodle run <file> [--seed N]`: parse the arguments, then run the program (`run::run`).
fn cmd_run(args: &[String]) -> u8 {
    let mut file: Option<String> = None;
    let mut seed: Option<u64> = None;
    let mut draw_log = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--seed" {
            i += 1;
            match args.get(i) {
                Some(value) => match parse_seed(value) {
                    Ok(n) => seed = Some(n),
                    Err(code) => return code,
                },
                None => {
                    eprintln!("doodle: `--seed` needs a value");
                    return 2;
                }
            }
        } else if let Some(value) = arg.strip_prefix("--seed=") {
            match parse_seed(value) {
                Ok(n) => seed = Some(n),
                Err(code) => return code,
            }
        } else if arg == "--draw-log" {
            draw_log = true;
        } else if arg.starts_with('-') {
            eprintln!("doodle: unknown option `{arg}`");
            return 2;
        } else if file.is_some() {
            eprintln!("doodle: unexpected extra argument `{arg}`");
            return 2;
        } else {
            file = Some(arg.to_string());
        }
        i += 1;
    }
    match file {
        Some(file) => run::run(run::RunOptions {
            file,
            seed,
            draw_log,
        }),
        None => {
            eprintln!("doodle: `run` needs a program file");
            eprintln!("usage: doodle run <file> [--seed N] [--draw-log]");
            2
        }
    }
}

/// Parses a `--seed` value as a non-negative integer, or reports a usage error (exit code 2).
fn parse_seed(value: &str) -> Result<u64, u8> {
    value.parse::<u64>().map_err(|_| {
        eprintln!("doodle: `--seed` wants a non-negative integer, got `{value}`");
        2
    })
}

/// `doodle test [root]`: run the conformance suite (default root `conformance`) in-process, mapping
/// its failed-test count to the exit code.
fn cmd_test(args: &[String]) -> u8 {
    let root = match args {
        [] => "conformance",
        [only] if !only.starts_with('-') => only.as_str(),
        _ => {
            eprintln!("doodle: usage: doodle test [root]");
            return 2;
        }
    };
    match conformance_runner::run(Path::new(root)) {
        Ok(0) => 0,
        Ok(_) => 1,
        Err(message) => {
            eprintln!("doodle: {message}");
            1
        }
    }
}

/// The usage text, shown for `--help` (to stdout) and on a usage error (to stderr).
const USAGE: &str = "\
doodle — the Doodle language CLI

usage:
  doodle run <file> [--seed N] [--draw-log]   run a Doodle program
  doodle test [root]                          run the conformance suite (default: conformance)
  doodle --help                               show this help
  doodle --version                            show the version

  --seed N     make `random` reproducible (default: seeded from the clock)
  --draw-log   print one line per drawing command (turtle programs)
";
