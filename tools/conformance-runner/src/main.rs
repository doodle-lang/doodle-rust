//! Runs the Doodle conformance suite against doodle-core.
//!
//! Discovers `<root>/**/*.doodle` (default root `conformance`), parses each
//! file's `#!` directive block, and — per the M0 pass policy — SKIPs any test
//! whose required pipeline stage doodle-core does not implement yet
//! (`doodle_core::stage::implemented_through`). Reports per-test results, a
//! clause-coverage summary, and the overall `=== N passed, N failed, N
//! skipped ===` line, exiting non-zero only on an unexpected result (a FAIL).
//!
//! At M0 doodle-core implements no stages, so every test SKIPs and the suite
//! is green. Execution and expectation matching land with the first stage
//! (M1); until then, a test that doodle-core reports as executable is a
//! runner/coordination bug and fails the run loudly.

mod directive;
mod model;

use doodle_core::stage::implemented_through;
use model::Test;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "conformance".to_string());
    match run(Path::new(&root)) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("conformance-runner: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the suite rooted at `root`, printing the report. Returns the number of
/// failed tests (0 = green), or an `Err` for a runner-level failure.
fn run(root: &Path) -> Result<usize, String> {
    let files = discover(root)?;

    // No PASS path until execution lands (M1); `passed` stays 0 for now.
    let passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut clause_tests: BTreeMap<String, usize> = BTreeMap::new();

    for path in &files {
        let rel = rel_path(root, path);
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) => {
                // A non-UTF-8 or unreadable file is one malformed test, not a
                // reason to abort the whole run.
                println!("FAIL  {rel}: unreadable ({e})");
                failed += 1;
                continue;
            }
        };

        match directive::parse_test(&rel, &source) {
            Err(message) => {
                println!("FAIL  {rel}: {message}");
                failed += 1;
            }
            Ok(test) => {
                if let Some(message) = clause_path_mismatch(path, &test) {
                    println!("FAIL  {rel}: {message}");
                    failed += 1;
                    continue;
                }
                for clause in &test.clauses {
                    *clause_tests.entry(clause.clone()).or_default() += 1;
                }
                if implemented_through().is_some_and(|impl_stage| impl_stage >= test.required) {
                    return Err(format!(
                        "doodle-core reports stage {:?} but the runner cannot execute tests \
                         yet ({}); add execution + expectation matching at M1",
                        test.required, test.id
                    ));
                }
                println!(
                    "SKIP  {}  [{}]  mode={:?} stage={:?}  ({} expectation(s), matched from M1)",
                    test.id,
                    test.clauses.join(","),
                    test.mode,
                    test.required,
                    test.expectation_count
                );
                skipped += 1;
            }
        }
    }

    println!();
    println!("Clause coverage ({} clause(s)):", clause_tests.len());
    for (clause, count) in &clause_tests {
        println!("  {clause}: {count} test(s)");
    }
    println!();
    println!("=== {passed} passed, {failed} failed, {skipped} skipped ===");
    Ok(failed)
}

/// Discovers `*.doodle` files under `root`, in a deterministic sorted order.
fn discover(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Err(format!("suite root `{}` does not exist", root.display()));
    }
    let mut out = Vec::new();
    collect(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("reading dir {}: {e}", dir.display()))?;
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| format!("reading dir {}: {e}", dir.display()))?;
        entries.push(entry.path());
    }
    entries.sort();
    for path in entries {
        // symlink_metadata does not follow symlinks, so a symlinked directory
        // is neither a dir nor a file here and is skipped — this avoids
        // unbounded recursion on a symlink cycle.
        let file_type = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .file_type();
        if file_type.is_dir() {
            collect(&path, out)?;
        } else if file_type.is_file() && path.extension().and_then(|x| x.to_str()) == Some("doodle")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The path of `path` relative to `root`, as a `/`-joined display string.
fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Reports a message if the file's clause directory does not match the test's
/// primary `#! clause:` — the format pins the primary clause in the path
/// (conformance/README.md), so a mismatch is a test-authoring error.
fn clause_path_mismatch(path: &Path, test: &Test) -> Option<String> {
    let dir = path.parent()?.file_name()?.to_str()?;
    let primary = test.clauses.first()?;
    (dir != primary.as_str())
        .then(|| format!("clause directory `{dir}` does not match primary `#! clause: {primary}`"))
}
