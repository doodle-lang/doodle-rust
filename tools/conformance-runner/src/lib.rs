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

mod c_host;
mod capability;
mod directive;
mod drive;
mod drivescript;
mod matcher;
mod model;
mod transcript;

use doodle_core::stage::implemented_through;
use model::{Mode, Test};
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
                let ready = stage_ready(&test);
                let header = format!(
                    "{}  [{}]  mode={:?} stage={:?}",
                    test.id,
                    test.clauses.join(","),
                    test.mode,
                    test.required
                );
                if !ready {
                    println!(
                        "SKIP  {header}  ({} expectation(s), matched once its stage lands)",
                        test.expectations.len()
                    );
                    skipped += 1;
                } else {
                    let mut reasons =
                        matcher::execute(&test, &source, fixture.modules_dir.as_deref())
                            .err()
                            .unwrap_or_default();
                    // A `run`/`drive` fixture also carries a committed transcript oracle (D-M7-20):
                    // the produced transcript must match its `<entry>.transcript` sidecar. Drift (or a
                    // missing sidecar) is a FAIL — regenerate with `--write`.
                    if let Err(mut drift) = check_transcript(
                        &test,
                        &source,
                        fixture.modules_dir.as_deref(),
                        &fixture.entry,
                    ) {
                        reasons.append(&mut drift);
                    }
                    if reasons.is_empty() {
                        println!("PASS  {header}");
                        passed += 1;
                    } else {
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

    println!();
    println!("Clause coverage ({} clause(s)):", clause_tests.len());
    for (clause, count) in &clause_tests {
        println!("  {clause}: {count} test(s)");
    }
    println!();
    println!("=== {passed} passed, {failed} failed, {skipped} skipped ===");
    Ok(failed)
}

/// Runs every `mode: run` fixture under `root` through the example **C host** (M7.5e) and compares
/// its finalized transcript to the canonical sidecar — the third surface, transitively identical to
/// native/wasm through the shared oracle. Drive fixtures are the M7.5e-2 extension (skipped here).
/// Returns the failed-fixture count (0 = green), or a runner-level `Err`.
pub fn run_c_host(root: &Path, c_host: &Path) -> Result<usize, String> {
    let fixtures = discover(root)?;
    let (mut passed, mut failed, mut skipped) = (0usize, 0usize, 0usize);
    for fixture in &fixtures {
        let rel = rel_path(root, &fixture.logical);
        let source = std::fs::read_to_string(&fixture.entry)
            .map_err(|e| format!("reading {}: {e}", fixture.entry.display()))?;
        let Ok(test) = directive::parse_test(&rel, &source) else {
            continue; // malformed fixtures are `run`'s report, not the C surface's
        };
        // The C host handles `mode: run` only (drive is M7.5e-2); everything else SKIPs visibly.
        if test.mode != Mode::Run || !stage_ready(&test) {
            skipped += 1;
            continue;
        }
        let sidecar = sidecar_path(&fixture.entry);
        let canonical = match std::fs::read_to_string(&sidecar) {
            Ok(text) => text,
            Err(e) => {
                println!("FAIL  {rel}: missing transcript sidecar ({e})");
                failed += 1;
                continue;
            }
        };
        match c_host::transcript_through_c(
            c_host,
            &test,
            &fixture.entry,
            fixture.modules_dir.as_deref(),
        ) {
            Ok(produced) if produced == canonical => {
                println!("PASS  {rel} (through C)");
                passed += 1;
            }
            Ok(_) => {
                println!("FAIL  {rel}: C-host transcript differs from the canonical oracle");
                failed += 1;
            }
            Err(reason) => {
                println!("FAIL  {rel}: {reason}");
                failed += 1;
            }
        }
    }
    println!("\n=== {passed} passed, {failed} failed, {skipped} skipped (through the C host) ===");
    Ok(failed)
}

/// Regenerates the committed transcript sidecar (`<entry>.transcript`) for every `run`/`drive`
/// fixture under `root` (D-M7-20): the native surface **generates** the canonical oracle, and the
/// default [`run`] drift-checks against it (the M1.12 lang-corpus-sync house pattern). Static
/// fixtures have no transcript. Returns the number of sidecars written, or a runner-level `Err`.
pub fn write(root: &Path) -> Result<usize, String> {
    let fixtures = discover(root)?;
    let mut written = 0usize;
    for fixture in &fixtures {
        let source = std::fs::read_to_string(&fixture.entry)
            .map_err(|e| format!("reading {}: {e}", fixture.entry.display()))?;
        let rel = rel_path(root, &fixture.logical);
        let Ok(test) = directive::parse_test(&rel, &source) else {
            continue; // a malformed fixture is reported by `run`, not regenerated here
        };
        if !is_transcript_mode(test.mode) || !stage_ready(&test) {
            continue;
        }
        let transcript = produce_transcript(&test, &source, fixture.modules_dir.as_deref())
            .map_err(|reasons| format!("{}: {}", rel, reasons.join("; ")))?;
        let sidecar = sidecar_path(&fixture.entry);
        std::fs::write(&sidecar, transcript.serialize())
            .map_err(|e| format!("writing {}: {e}", sidecar.display()))?;
        println!("wrote {}", sidecar.display());
        written += 1;
    }
    println!("\n=== {written} transcript(s) written ===");
    Ok(written)
}

/// Checks a stage-ready fixture's committed transcript sidecar against the freshly produced one
/// (D-M7-20). A static fixture has no transcript (`Ok`). A drift or a missing sidecar is the FAIL.
fn check_transcript(
    test: &Test,
    source: &str,
    modules_dir: Option<&Path>,
    entry: &Path,
) -> Result<(), Vec<String>> {
    if !is_transcript_mode(test.mode) {
        return Ok(());
    }
    let produced = produce_transcript(test, source, modules_dir)?.serialize();
    let sidecar = sidecar_path(entry);
    match std::fs::read_to_string(&sidecar) {
        Ok(committed) if committed == produced => Ok(()),
        Ok(_) => Err(vec![format!(
            "transcript drift: {} differs from the produced transcript (regenerate with `--write`)",
            sidecar.display()
        )]),
        Err(_) => Err(vec![format!(
            "missing transcript sidecar {} (regenerate with `--write`)",
            sidecar.display()
        )]),
    }
}

/// Produces a fixture's transcript by mode (`run`/`drive`); the emitter runs the program.
fn produce_transcript(
    test: &Test,
    source: &str,
    modules_dir: Option<&Path>,
) -> Result<transcript::Transcript, Vec<String>> {
    match test.mode {
        Mode::Run => transcript::record_run(test, source, modules_dir),
        Mode::Drive => transcript::record_drive(test, source, modules_dir),
        Mode::Static => unreachable!("a static fixture has no transcript"),
    }
}

/// The committed transcript sidecar path for a fixture's entry file (`<entry>.transcript`).
fn sidecar_path(entry: &Path) -> PathBuf {
    let mut name = entry.as_os_str().to_os_string();
    name.push(".transcript");
    PathBuf::from(name)
}

/// Whether a fixture's mode carries a transcript oracle (D-M7-20 scope: `run`/`drive`, not static).
fn is_transcript_mode(mode: Mode) -> bool {
    matches!(mode, Mode::Run | Mode::Drive)
}

/// Whether doodle-core implements the stage this test requires (else it SKIPs, no transcript).
fn stage_ready(test: &Test) -> bool {
    implemented_through().is_some_and(|impl_stage| impl_stage >= test.required)
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
