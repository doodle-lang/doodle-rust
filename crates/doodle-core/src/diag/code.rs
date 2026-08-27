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
    /// Assigning to a non-mutable binding — a `const` or a declaration (`to`/`fn`/
    /// `record`/`protocol`/`parameter`/`module`, S-6 rule 2a) (L§5.3).
    ConstReassignment,
    /// A string literal that reaches end of input unclosed (L§3).
    UnterminatedString,
    /// A malformed numeric literal — bad underscore, base prefix, or exponent
    /// (L§3.6.1/§3.6.2).
    MalformedNumber,
    /// A float literal whose value rounds to ±∞ (e.g. `1e999`): a static error, so the
    /// finite-float invariant (S-56, L§4.2) holds at the source boundary and no non-finite
    /// value ever enters the AST (L§3.6.2). An underflow to `0.0`/a subnormal is legal.
    FloatLiteralOutOfRange,
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
    /// A valued `break`/`continue` whose destination is a `while`/`loop`, which
    /// yields no value (L§7.10, S-10 loop half): the value has nowhere to go. Only
    /// a block-consuming call can receive a `break`/`continue` value.
    ValuedExitInLoop,
    /// A valued `return` inside a procedure (`to`), which yields no value (L§8.4):
    /// the returned value has no destination. Use a plain `return`, or an `fn`.
    ValuedReturnInProcedure,
    /// A `with` whose target name is not a module-level dynamic `parameter` (L§5.5):
    /// a different global kind (`let`/`const`, `to`/`fn`, a record/protocol/module),
    /// or no such declaration. `with` never rebinds a lexical binding, and there is
    /// no auto-creation of a parameter.
    WithTargetNotParameter,
    /// A `do name` block parameter used as a **value** rather than invoked (L§8.5):
    /// a block is second-class — it may only be invoked (`name(…)`), never stored,
    /// returned, assigned, or passed on.
    BlockUsedAsValue,
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
    /// value, where that is statically determinable (a module-level `to` callee,
    /// directly or propagated through an expression-position `if`/`try`). The
    /// unified L§6.11 diagnostic (S-6 consuming-site check). An unknown callee's
    /// Void-ness is deferred to the runtime check (M2a).
    ProcedureInExpression,
    /// An `if` used as a value (in a consuming position) with no `else` — it might
    /// produce no value (L§6.8).
    IfExpressionMissingElse,
    /// A present branch/body of a value-position `if`/`try` whose tail produces no
    /// value (L§6.8/§6.9) — e.g. it ends in a `let`/`while`/assignment.
    NonProducingBranch,
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
            DiagnosticCode::FloatLiteralOutOfRange => "float-literal-out-of-range",
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
            DiagnosticCode::ValuedExitInLoop => "valued-exit-in-loop",
            DiagnosticCode::ValuedReturnInProcedure => "valued-return-in-procedure",
            DiagnosticCode::WithTargetNotParameter => "with-target-not-parameter",
            DiagnosticCode::BlockUsedAsValue => "block-used-as-value",
            DiagnosticCode::DuplicateDeclaration => "duplicate-declaration",
            DiagnosticCode::UndeclaredAssignment => "undeclared-assignment",
            DiagnosticCode::FunctionFallsOffEnd => "function-falls-off-end",
            DiagnosticCode::ProcedureInExpression => "procedure-in-expression",
            DiagnosticCode::IfExpressionMissingElse => "if-expression-missing-else",
            DiagnosticCode::NonProducingBranch => "non-producing-branch",
        }
    }
}

impl core::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.slug())
    }
}
