//! The Doodle conformance suite runner, as a library (D-M7-19).
//!
//! Discovers `<root>/**/*.doodle` (default root `conformance`), parses each file's `#!` directive
//! block, and applies the staged pass policy: a test whose required pipeline stage doodle-core
//! implements (`doodle_core::stage::implemented_through`) is executed and its `expect-…` directives
//! matched against real output; a test above the implemented stage is SKIPped, not failed. [`run`]
//! prints per-test results, a clause-coverage summary, and the overall
//! `=== N passed, N failed, N skipped ===` line, returning the failed-test count (0 = green).
//!
//! Two hosts share this logic in-process: the `conformance-runner` binary (`main.rs`, a thin
//! wrapper), and the `doodle test` subcommand (M7.4c), which links this crate and calls [`run`]
//! rather than shelling out — so the two run identical discovery/matching over the same
//! `demo_registry` (E§11 cross-surface parity).
//!
//! As of M2b.2 the machine runs the demo subset and the `print` intrinsic is registered before
//! load (S-43), so `mode: run` tests execute fully — driving the program and matching both
//! `expect-raise` (the uncaught exception) and `expect-out` (the captured output). A run test above
//! the implemented stage still SKIPs.

mod capability;
mod directive;
mod drive;
mod drivescript;
mod matcher;
mod model;

use doodle_core::stage::implemented_through;
use model::Test;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A discovered fixture: a **single-module** `*.doodle` file, or a **multi-module** directory
/// holding `main.doodle` (the entry, carrying the `#!` directives) plus sibling `<name>.doodle`
/// modules an `import name` resolves to. `logical` is the fixture's identity path — the file
/// itself, or the directory — used for the id and the clause-directory check; `entry` is the file
/// to parse; `modules_dir` is `Some(dir)` for a multi-module fixture.
struct Fixture {
    logical: PathBuf,
    entry: PathBuf,
    modules_dir: Option<PathBuf>,
}

/// Runs the suite rooted at `root`, printing the report. Returns the number of failed tests
/// (0 = green), or an `Err` for a runner-level failure (a missing/unreadable suite root).
pub fn run(root: &Path) -> Result<usize, String> {
    let fixtures = discover(root)?;

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut clause_tests: BTreeMap<String, usize> = BTreeMap::new();

    for fixture in &fixtures {
        let rel = rel_path(root, &fixture.logical);
        let source = match std::fs::read_to_string(&fixture.entry) {
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
                if let Some(message) = clause_path_mismatch(&fixture.logical, &test) {
                    println!("FAIL  {rel}: {message}");
                    failed += 1;
                    continue;
                }
                for clause in &test.clauses {
                    *clause_tests.entry(clause.clone()).or_default() += 1;
                }
                let stage_ready =
                    implemented_through().is_some_and(|impl_stage| impl_stage >= test.required);
                let header = format!(
                    "{}  [{}]  mode={:?} stage={:?}",
                    test.id,
                    test.clauses.join(","),
                    test.mode,
                    test.required
                );
                if !stage_ready {
                    println!(
                        "SKIP  {header}  ({} expectation(s), matched once its stage lands)",
                        test.expectations.len()
                    );
                    skipped += 1;
                } else {
                    match matcher::execute(&test, &source, fixture.modules_dir.as_deref()) {
                        Ok(()) => {
                            println!("PASS  {header}");
                            passed += 1;
                        }
                        Err(reasons) => {
                            println!("FAIL  {header}");
                            for reason in &reasons {
                                println!("        {reason}");
                            }
                            failed += 1;
                        }
                    }
                }
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

/// Discovers fixtures under `root`, in a deterministic sorted order (by logical path).
fn discover(root: &Path) -> Result<Vec<Fixture>, String> {
    if !root.exists() {
        return Err(format!("suite root `{}` does not exist", root.display()));
    }
    let mut out = Vec::new();
    collect(root, &mut out)?;
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<Fixture>) -> Result<(), String> {
    // A directory holding `main.doodle` is a **single multi-module fixture**: its `main.doodle`
    // is the entry and its other `.doodle` files are its modules (not separate fixtures), so
    // this branch does not recurse.
    let main = dir.join("main.doodle");
    if main.is_file() {
        out.push(Fixture {
            logical: dir.to_path_buf(),
            entry: main,
            modules_dir: Some(dir.to_path_buf()),
        });
        return Ok(());
    }
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
            out.push(Fixture {
                logical: path.clone(),
                entry: path,
                modules_dir: None,
            });
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
