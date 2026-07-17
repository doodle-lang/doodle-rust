//! The diagnostic code registry: one stable kebab-case slug per diagnostic
//! class (plan-m1 M1.1). Provisional scheme; a numbered scheme, if ever
//! wanted, is a future spec delta. The IDE consumes these slugs as a de-facto
//! API surface, so a slug names the *rule*, not the offending token.

/// A stable, machine-readable identifier for a class of diagnostic.
///
/// The enum is closed and grows by one variant per diagnostic class as the
/// producing milestone lands (M1.3–M1.11); this keeps the catalog greppable and
/// exhaustively documented. [`Display`](core::fmt::Display) and
/// [`DiagnosticCode::slug`] both yield the canonical kebab-case slug.
///
/// Only classes with a producer (or, at M1.1, a renderer test) are present;
/// the full reserved-slug catalog lives in the error-message rubric
/// (`discussions/plan/error-message-rubric.md`).
///
/// It is deliberately **not** `#[non_exhaustive]`: doodle-core is unpublished,
/// so its in-workspace consumers (and the eventual bindings) evolve in lockstep
/// and benefit from exhaustive matching. Revisit if an out-of-tree consumer
/// ever depends on it across a stability boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DiagnosticCode {
    /// `a < b < c`: comparison operators don't chain (L§6.5).
    ChainedComparison,
    /// A general syntax error — an unexpected or missing token (L§6, M1.6).
    SyntaxError,
    /// Reassigning a `const` binding (L§5.2).
    ConstReassignment,
    /// A string literal that reaches end of input unclosed (L§3).
    UnterminatedString,
    /// A malformed numeric literal — bad underscore, base prefix, or exponent
    /// (L§3.6.1/§3.6.2).
    MalformedNumber,
    /// A character that cannot begin a token here (L§3).
    UnexpectedCharacter,
    /// A backslash escape outside the closed set, e.g. `\q` (L§3.6.3).
    UnknownEscape,
    /// A known escape in malformed form — `\x` short a digit, braceless/empty/
    /// over-long `\u`, a surrogate scalar, or `\u` in a bytes literal (L§3.6.3).
    MalformedEscape,
    /// An interpolation with no expression, `{}` or `{ }` (L§6.7).
    EmptyInterpolation,
    /// A `#` comment inside an interpolation `{…}` (L§6.7): not allowed, since a
    /// comment would run to end of line and swallow the closing `}`.
    CommentInInterpolation,
    /// An interpolation not closed before end of line or input — a line
    /// terminator inside `{…}`, or EOF (L§6.7).
    UnterminatedInterpolation,
    /// A non-ASCII code point inside a bytes literal `b"…"` (L§3.6.5).
    NonAsciiBytes,
    /// A content line of a triple-quoted string does not match the closing
    /// `"""` margin (L§3.6.4).
    MarginMismatch,
    /// A triple-quoted string's opening `"""` is not alone on its line —
    /// something other than whitespace follows it (L§3.6.4).
    MalformedTripleQuote,
    /// A binding that hides an outer one of the same name (L§5.1; a warning).
    Shadowing,
    /// A `return`/`break`/`continue` outside its valid context — `return` outside
    /// a procedure/function, or `break`/`continue` outside a loop or block (L§7.10).
    MisplacedExit,
    /// Two bindings of the same name in one scope (L§5.2).
    DuplicateDeclaration,
    /// Assigning to a name that is not a mutable (`let`) binding visible here — an
    /// undeclared name, or one that could only come from an import (imports are
    /// read-only, S-39) (L§5.3). `const` and declaration targets are the distinct
    /// [`ConstReassignment`](Self::ConstReassignment) family instead.
    UndeclaredAssignment,
    /// A function (`fn`) whose body can complete without producing a value, where
    /// that is statically determinable (L§8.4, S-5 tail classifier).
    FunctionFallsOffEnd,
    /// A procedure (`to`) call used where a value is required — Void consumed as a
    /// value, where that is statically determinable (a same-module `to` callee,
    /// directly or propagated through an expression-position `if`/`try`). The
    /// unified L§6.11 diagnostic (S-6 consuming-site check). An unknown callee's
    /// Void-ness is deferred to the runtime check (M2a).
    ProcedureInExpression,
}

impl DiagnosticCode {
    /// The canonical kebab-case slug (e.g. `"chained-comparison"`).
    pub fn slug(self) -> &'static str {
        match self {
            DiagnosticCode::ChainedComparison => "chained-comparison",
            DiagnosticCode::SyntaxError => "syntax-error",
            DiagnosticCode::ConstReassignment => "const-reassignment",
            DiagnosticCode::UnterminatedString => "unterminated-string",
            DiagnosticCode::MalformedNumber => "malformed-number",
            DiagnosticCode::UnexpectedCharacter => "unexpected-character",
            DiagnosticCode::UnknownEscape => "unknown-escape",
            DiagnosticCode::MalformedEscape => "malformed-escape",
            DiagnosticCode::EmptyInterpolation => "empty-interpolation",
            DiagnosticCode::CommentInInterpolation => "comment-in-interpolation",
            DiagnosticCode::UnterminatedInterpolation => "unterminated-interpolation",
            DiagnosticCode::NonAsciiBytes => "non-ascii-bytes",
            DiagnosticCode::MarginMismatch => "margin-mismatch",
            DiagnosticCode::MalformedTripleQuote => "malformed-triple-quote",
            DiagnosticCode::Shadowing => "shadowing",
            DiagnosticCode::MisplacedExit => "misplaced-exit",
            DiagnosticCode::DuplicateDeclaration => "duplicate-declaration",
            DiagnosticCode::UndeclaredAssignment => "undeclared-assignment",
            DiagnosticCode::FunctionFallsOffEnd => "function-falls-off-end",
            DiagnosticCode::ProcedureInExpression => "procedure-in-expression",
        }
    }
}

impl core::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.slug())
    }
}
