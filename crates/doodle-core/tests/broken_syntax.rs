//! M1.13 broken-syntax message corpus: render the front-end's diagnostics for
//! each hand-written broken program (one mistake each, kid-plausible) and snapshot
//! the plain-text output — so the messages a beginner actually sees are pinned and
//! reviewable against the error-message rubric
//! (`discussions/plan/error-message-rubric.md`). The rubric-pass notes and the
//! user's sign-off live in `tests/broken-syntax/README.md`; this test only locks
//! the rendered text so a message change surfaces for re-review (`cargo insta
//! review`). Per plan-m1 M1.13 the agent does not self-certify — the user approves.

use doodle_core::diag::render::{SourceView, render_diagnostics};
use doodle_core::full_to_diagnostics;
use doodle_core::source::normalize;
use std::path::{Path, PathBuf};

/// The broken programs, sorted for determinism.
fn programs() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/broken-syntax");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "doodle"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no .doodle programs in {}", dir.display());
    out
}

fn snap_name(path: &Path) -> String {
    path.file_stem()
        .expect("stem")
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[test]
fn broken_programs_render_reviewable_messages() {
    for path in programs() {
        let raw = std::fs::read_to_string(&path).expect("read program");
        let src = normalize(&raw);
        let diagnostics = full_to_diagnostics(src.as_ref());
        assert!(
            !diagnostics.is_empty(),
            "{} is meant to be broken but produced no diagnostics",
            path.display()
        );
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let view = SourceView {
            name: &name,
            source: src.as_ref(),
        };
        insta::assert_snapshot!(snap_name(&path), render_diagnostics(&diagnostics, &view));
    }
}
