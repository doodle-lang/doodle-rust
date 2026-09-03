//! Integration tests for the `doodle` binary (M7.4c): they invoke the built CLI as a subprocess
//! (`CARGO_BIN_EXE_doodle`), so they exercise argument parsing, the drive loop, capability
//! resolution, streaming output, error rendering, and the process exit code end to end — the real
//! host, not a library call.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The outcome of one CLI invocation: its exit code and captured stdout/stderr.
struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the `doodle` binary with `args`, feeding `stdin` to it, and captures the result.
fn run_doodle(args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_doodle"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the doodle binary");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for doodle");
    Output {
        code: out.status.code().expect("a normal exit code"),
        stdout: String::from_utf8(out.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(out.stderr).expect("utf-8 stderr"),
    }
}

/// Writes `source` to `<CARGO_TARGET_TMPDIR>/<name>` and returns its path (a per-suite temp dir
/// cargo provides, so tests do not collide or litter the tree).
fn fixture(name: &str, source: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, source).expect("write fixture");
    path
}

#[test]
fn run_prints_to_stdout_and_exits_zero() {
    let program = fixture("hello.doodle", "print(1 + 2)\nprint(\"hi\")\n");
    let out = run_doodle(&["run", program.to_str().unwrap()], b"");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "3\nhi\n");
}

#[test]
fn run_reads_a_line_from_stdin() {
    // `read_line` suspends and the CLI resolves it from stdin; the prompt streams before the read,
    // then the echoed line prints after it.
    let program = fixture(
        "greet.doodle",
        "print(\"name?\")\nlet who = read_line()\nprint(who)\n",
    );
    let out = run_doodle(&["run", program.to_str().unwrap()], b"Ada\n");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "name?\nAda\n");
}

#[test]
fn run_renders_a_load_error_to_stderr_and_exits_one() {
    // A resolve-time error fails the load: rendered to stderr with a snippet, nothing on stdout.
    let program = fixture("bad.doodle", "let y = undefined_thing\n");
    let out = run_doodle(&["run", program.to_str().unwrap()], b"");
    assert_eq!(out.code, 1);
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert!(
        out.stderr.contains("error[") && out.stderr.contains("undefined_thing"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn run_renders_a_runtime_raise_with_a_snippet() {
    // A runtime raise (division by zero) renders via the raise path (E§9) — an `error[...]`
    // header plus a `-->` locator into the source — and exits nonzero.
    let program = fixture("divzero.doodle", "print(1 / 0)\n");
    let out = run_doodle(&["run", program.to_str().unwrap()], b"");
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("error[division-by-zero]") && out.stderr.contains("-->"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn seeded_random_is_reproducible() {
    // The `--seed N` contract (D-M7-16): the same seed yields the same stream across runs.
    let program = fixture("rand.doodle", "print(random())\nprint(random())\n");
    let path = program.to_str().unwrap();
    let first = run_doodle(&["run", path, "--seed", "42"], b"");
    let second = run_doodle(&["run", path, "--seed", "42"], b"");
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);
    assert_eq!(first.stdout, second.stdout, "same seed must replay");
    // A different seed diverges (the stream is seed-dependent, not constant).
    let other = run_doodle(&["run", path, "--seed", "7"], b"");
    assert_ne!(first.stdout, other.stdout);
}

#[test]
fn a_bad_seed_is_a_usage_error() {
    let program = fixture("rand2.doodle", "print(random())\n");
    let out = run_doodle(&["run", program.to_str().unwrap(), "--seed", "abc"], b"");
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("--seed"), "stderr: {}", out.stderr);
}

#[test]
fn test_subcommand_runs_the_suite_in_process() {
    // `doodle test` links the conformance runner (D-M7-19) and runs the real suite; the workspace
    // conformance root is two levels up from this crate.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance");
    let out = run_doodle(&["test", root.to_str().unwrap()], b"");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("passed") && out.stdout.contains("0 failed"),
        "stdout tail: {}",
        out.stdout.lines().last().unwrap_or_default()
    );
}

#[test]
fn run_resolves_a_multi_file_import() {
    // The gallery `hello_modules` imports its sibling `sayings` module; the CLI's resolver maps
    // `import sayings` to `sayings.doodle` beside the entry file (E§6), independent of cwd.
    let entry =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/gallery/hello_modules.doodle");
    let out = run_doodle(&["run", entry.to_str().unwrap()], b"");
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "Hello, world!\nHooray for Doodle!\n");
}

#[test]
fn a_missing_import_raises_module_not_found() {
    // The resolver returns `NotFound` for an absent module file; the engine raises
    // `module-not-found` at the `import` site (E§6, S-7).
    let program = fixture("missing_import.doodle", "import no_such_module\n");
    let out = run_doodle(&["run", program.to_str().unwrap()], b"");
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("module-not-found"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn no_command_is_a_usage_error() {
    let out = run_doodle(&[], b"");
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("usage:"), "stderr: {}", out.stderr);
}
