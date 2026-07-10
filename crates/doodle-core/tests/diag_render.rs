//! M1.1 acceptance: snapshot the plain-text diagnostic renderer over a
//! hand-built diagnostic set (no lexer/parser exists yet). These lock the
//! renderer's *mechanics*; the M1.13 broken-syntax message review
//! (reviewer = the user, against the error-message rubric) is a separate,
//! later gate over ~40 real programs.

use doodle_core::diag::code::DiagnosticCode;
use doodle_core::diag::render::{SourceView, render_diagnostic, render_load_error};
use doodle_core::diag::{Diagnostic, LoadError, Note, Replacement, Severity, Suggestion};
use doodle_core::span::{ModuleId, Span};

const M: ModuleId = ModuleId(0);

fn view<'a>(name: &'a str, source: &'a str) -> SourceView<'a> {
    SourceView { name, source }
}

/// The byte span of the first occurrence of `needle` in `source`.
fn span_of(source: &str, needle: &str) -> Span {
    let start = source.find(needle).expect("needle present in source");
    Span::new(start as u32, (start + needle.len()) as u32)
}

#[test]
fn single_line_error_with_caret_and_suggestion() {
    let source = "to main()\n  if (a < b < c) then\n  end\nend\n";
    let d = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span_of(source, "a < b < c"),
        "comparison operators don't chain",
    )
    .with_suggestion(Suggestion {
        message: "write `a < b and b < c` instead".to_string(),
        replacement: None,
    });
    insta::assert_snapshot!(render_diagnostic(&d, &view("playground.doodle", source)));
}

#[test]
fn zero_width_span_at_end_of_input() {
    let source = "to greet()\n  say(\"hello";
    let end = source.len() as u32;
    let d = Diagnostic::error(
        DiagnosticCode::UnterminatedString,
        M,
        Span::new(end, end),
        "this string is never closed",
    );
    insta::assert_snapshot!(render_diagnostic(&d, &view("greet.doodle", source)));
}

#[test]
fn multi_line_span_underlines_first_line_only() {
    let source = "to f()\n  x = \"\"\"\n  still going\n  and going";
    let d = Diagnostic::error(
        DiagnosticCode::UnterminatedString,
        M,
        span_of(source, "\"\"\"\n  still going\n  and going"),
        "this block string is never closed",
    );
    insta::assert_snapshot!(render_diagnostic(&d, &view("f.doodle", source)));
}

#[test]
fn error_with_a_spanned_note() {
    let source = "const pi = 3\n  pi = 4\n";
    let d = Diagnostic::error(
        DiagnosticCode::ConstReassignment,
        M,
        span_of(source, "pi = 4"),
        "can't reassign `pi` — it's a constant",
    )
    .with_note(Note {
        message: "`pi` is declared const here".to_string(),
        span: Some(span_of(source, "const pi = 3")),
    });
    insta::assert_snapshot!(render_diagnostic(&d, &view("consts.doodle", source)));
}

#[test]
fn standalone_warning_with_note() {
    let source = "count = 1\nto f()\n  count = 2\nend\n";
    let d = Diagnostic::warning(
        DiagnosticCode::Shadowing,
        M,
        span_of(source, "count = 2"),
        "this `count` hides an outer `count`",
    )
    .with_note(Note {
        message: "the outer `count` is declared here".to_string(),
        span: Some(span_of(source, "count = 1")),
    });
    insta::assert_snapshot!(render_diagnostic(&d, &view("shadow.doodle", source)));
}

#[test]
fn multi_diagnostic_load_error() {
    let source = "to f()\n  if (a < b < c) then\n  const x = 1\n  x = 2\nend\n";
    let err = LoadError {
        diagnostics: vec![
            Diagnostic::error(
                DiagnosticCode::ChainedComparison,
                M,
                span_of(source, "a < b < c"),
                "comparison operators don't chain",
            ),
            Diagnostic::error(
                DiagnosticCode::ConstReassignment,
                M,
                span_of(source, "x = 2"),
                "can't reassign `x` — it's a constant",
            ),
        ],
    };
    insta::assert_snapshot!(render_load_error(&err, &view("two.doodle", source)));
}

#[test]
fn spanless_diagnostic_is_header_only() {
    let d = Diagnostic {
        severity: Severity::Error,
        code: DiagnosticCode::UnterminatedString,
        message: "a diagnostic with no source location".to_string(),
        module: None,
        span: None,
        notes: Vec::new(),
        suggestion: None,
    };
    insta::assert_snapshot!(render_diagnostic(&d, &view("nowhere.doodle", "")));
}

#[test]
fn multibyte_prefix_uses_code_point_columns() {
    // `café = (` before the span: 8 code points but 9 bytes (é is 2 bytes), so
    // the caret pad must count code points, not bytes.
    let source = "café = (a < b < c)\n";
    let d = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span_of(source, "a < b < c"),
        "comparison operators don't chain",
    );
    insta::assert_snapshot!(render_diagnostic(&d, &view("unicode.doodle", source)));
}

#[test]
fn structured_diagnostic_debug_shape() {
    // Locks the normative structured surface (not just the rendering).
    let source = "const pi = 3\n  pi = 4\n";
    let d = Diagnostic::error(
        DiagnosticCode::ConstReassignment,
        M,
        span_of(source, "pi = 4"),
        "can't reassign `pi` — it's a constant",
    )
    .with_note(Note {
        message: "`pi` is declared const here".to_string(),
        span: Some(span_of(source, "const pi = 3")),
    });
    insta::assert_debug_snapshot!(d);
}

#[test]
fn spanless_note_renders_as_a_note_line() {
    let source = "count = 1\nto f()\n  count = 2\nend\n";
    let d = Diagnostic::warning(
        DiagnosticCode::Shadowing,
        M,
        span_of(source, "count = 2"),
        "this `count` hides an outer `count`",
    )
    .with_note(Note {
        message: "shadowing is allowed, but it's often a mistake".to_string(),
        span: None,
    });
    insta::assert_snapshot!(render_diagnostic(&d, &view("shadow.doodle", source)));
}

#[test]
fn structured_suggestion_with_replacement_debug_shape() {
    let source = "to f()\n  if (a < b < c) then\n";
    let d = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span_of(source, "a < b < c"),
        "comparison operators don't chain",
    )
    .with_suggestion(Suggestion {
        message: "write `a < b and b < c` instead".to_string(),
        replacement: Some(Replacement {
            span: span_of(source, "a < b < c"),
            text: "a < b and b < c".to_string(),
        }),
    });
    insta::assert_debug_snapshot!(d);
}

#[test]
fn a_machine_applicable_replacement_does_not_change_plain_text() {
    // The structured Replacement is for the IDE (rubric element (c)); the
    // plain-text rendering shows only the suggestion prose, byte-for-byte the
    // same whether or not a Replacement is attached.
    let source = "to f()\n  if (a < b < c) then\n";
    let span = span_of(source, "a < b < c");
    let message = "write `a < b and b < c` instead";
    let prose_only = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span,
        "comparison operators don't chain",
    )
    .with_suggestion(Suggestion {
        message: message.to_string(),
        replacement: None,
    });
    let with_edit = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span,
        "comparison operators don't chain",
    )
    .with_suggestion(Suggestion {
        message: message.to_string(),
        replacement: Some(Replacement {
            span,
            text: "a < b and b < c".to_string(),
        }),
    });
    let v = view("f.doodle", source);
    assert_eq!(
        render_diagnostic(&prose_only, &v),
        render_diagnostic(&with_edit, &v)
    );
}

#[test]
fn malformed_spans_never_panic() {
    // `café` puts a multibyte char (é: bytes 3..5) early in the source so a
    // span landing mid-char exercises the char-boundary clamp.
    let source = "café = (a < b)\n";
    let v = view("x.doodle", source);
    let d = |start: u32, end: u32| {
        Diagnostic::error(
            DiagnosticCode::ChainedComparison,
            M,
            Span::new(start, end),
            "m",
        )
    };
    // start and end past the end of source; end < start; start splitting é;
    // a zero-width span mid-line. None may panic.
    let _ = render_diagnostic(&d(9999, 10000), &v);
    let _ = render_diagnostic(&d(8, 2), &v);
    let _ = render_diagnostic(&d(4, 4), &v);
    let _ = render_diagnostic(&d(2, 2), &v);
}

#[test]
fn crlf_line_ending_is_stripped_from_the_snippet() {
    let source = "to f()\r\n  if (a < b < c) then\r\n";
    let d = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span_of(source, "a < b < c"),
        "comparison operators don't chain",
    );
    insta::assert_snapshot!(render_diagnostic(&d, &view("crlf.doodle", source)));
}

#[test]
fn nfc_normalized_source_positions_a_caret_correctly() {
    // A decomposed é (`e` + U+0301) NFC-normalizes to `café`; rendering over the
    // normalized source produces a code-point-derived caret byte-identical to
    // rendering the already-composed source — NFC-on-load (L§3.1) and S-1
    // code-point columns line up.
    let decomposed = "cafe\u{301} = draw(a < b < c)\n";
    let nfc = doodle_core::source::normalize(decomposed);
    let d = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span_of(nfc.as_ref(), "a < b < c"),
        "comparison operators don't chain",
    );
    let rendered = render_diagnostic(&d, &view("café.doodle", nfc.as_ref()));

    let composed = "café = draw(a < b < c)\n";
    let d2 = Diagnostic::error(
        DiagnosticCode::ChainedComparison,
        M,
        span_of(composed, "a < b < c"),
        "comparison operators don't chain",
    );
    assert_eq!(
        rendered,
        render_diagnostic(&d2, &view("café.doodle", composed))
    );

    insta::assert_snapshot!(rendered);
}
