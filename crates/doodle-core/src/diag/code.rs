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
    /// Reassigning a `const` binding (L§5.2).
    ConstReassignment,
    /// A string literal that reaches end of input unclosed (L§3).
    UnterminatedString,
    /// A malformed numeric literal — bad underscore, base prefix, or exponent
    /// (L§3.6.1/§3.6.2).
    MalformedNumber,
    /// A character that cannot begin a token here (L§3).
    UnexpectedCharacter,
    /// A binding that hides an outer one of the same name (L§5.1; a warning).
    Shadowing,
}

impl DiagnosticCode {
    /// The canonical kebab-case slug (e.g. `"chained-comparison"`).
    pub fn slug(self) -> &'static str {
        match self {
            DiagnosticCode::ChainedComparison => "chained-comparison",
            DiagnosticCode::ConstReassignment => "const-reassignment",
            DiagnosticCode::UnterminatedString => "unterminated-string",
            DiagnosticCode::MalformedNumber => "malformed-number",
            DiagnosticCode::UnexpectedCharacter => "unexpected-character",
            DiagnosticCode::Shadowing => "shadowing",
        }
    }
}

impl core::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.slug())
    }
}
