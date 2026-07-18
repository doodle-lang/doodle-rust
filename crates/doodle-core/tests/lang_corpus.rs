//! Golden AST snapshots for the language-spec corpus (M1.12): every `doodle`-
//! tagged code example extracted from `discussions/spec/language.md` into a
//! `spec-b*.doodle` fixture (see `scripts/lang-corpus-sync.py`) is parsed here and
//! its AST snapshotted, so the M1 exit criterion "every code example in L parses
//! to a golden AST" holds mechanically. The fixtures are committed in this repo,
//! so this test is self-contained — it does not read the sibling `discussions`
//! checkout (the sync script is what ties the fixtures back to the spec).
//!
//! A new/changed example changes a snapshot: review with `cargo insta review`.

use doodle_core::parse::parse_program;
use doodle_core::source::normalize;
use doodle_core::span::ModuleId;
use std::path::{Path, PathBuf};

/// The committed spec-example fixtures, sorted for determinism. Rooted at the
/// crate-relative conformance tree (independent of the test's working directory).
fn spec_fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/v0.1/lang");
    let mut out = Vec::new();
    collect(&root, &mut out);
    out.sort();
    assert!(
        !out.is_empty(),
        "no spec-b*.doodle fixtures found under {}",
        root.display()
    );
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out);
        } else if is_spec_fixture(&path) {
            out.push(path);
        }
    }
}

fn is_spec_fixture(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("spec-b") && n.ends_with(".doodle"))
}

/// The fixture path relative to the `lang/` root, as a snapshot-safe slug
/// (`L3.6.4/spec-b07.doodle` -> `L3_6_4__spec_b07`) — the stable snapshot name.
fn snapshot_name(path: &Path) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/v0.1/lang");
    let rel = path.strip_prefix(&root).unwrap_or(path).with_extension("");
    rel.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The example source: the fixture with its leading `#!` directive header removed,
/// so the snapshotted AST (and its spans) is exactly the example as written in the
/// spec. Only the top contiguous run of `#! ` directive lines (the generated
/// `clause`/`mode`/`stage` header) is dropped — a `#!` shebang or comment inside an
/// example body is preserved.
fn example_source(text: &str) -> String {
    let mut lines = text.lines().peekable();
    while lines.peek().is_some_and(|l| l.starts_with("#! ")) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n")
}

#[test]
fn spec_examples_parse_to_golden_asts() {
    for path in spec_fixtures() {
        let text = std::fs::read_to_string(&path).expect("read fixture");
        let example = example_source(&text);
        let src = normalize(&example);
        let parsed = parse_program(src.as_ref(), ModuleId(0));
        assert!(
            parsed.diagnostics.is_empty(),
            "spec example {} should parse cleanly: {:?}",
            path.display(),
            parsed.diagnostics
        );
        insta::assert_debug_snapshot!(snapshot_name(&path), parsed.ast);
    }
}
