//! Tokens: the lexical vocabulary (L§3.5–§3.7), keyword recognition, and the
//! S-2 continuation-trigger predicate.

use crate::span::Span;

/// A lexed token: a [`TokenKind`] and its source [`Span`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// The token's byte span in the NFC source.
    pub span: Span,
}

/// A lexical token kind (L§3.5–§3.7). Literal variants carry only their
/// lexical shape; the literal *value* is lowered by the parser (M1.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    /// An integer literal (L§3.6.1); the span carries the digits.
    Int,
    /// A float literal (L§3.6.2).
    Float,
    /// The opening `"` of a string literal (L§3.6.3). A string is lexed as a
    /// stream — `StrStart`, then `StrText`/interpolation parts, then `StrEnd`.
    StrStart,
    /// A run of literal string text between interpolations; the span is the raw
    /// source slice (escapes decoded by the parser, M1.6).
    StrText,
    /// The `{` opening an interpolation (L§6.7).
    InterpStart,
    /// The `}` closing an interpolation.
    InterpEnd,
    /// The closing `"` of a string literal.
    StrEnd,
    /// A bytes literal `b"…"` (L§3.6.5); the span carries the whole literal, no
    /// interpolation. Value decoded by the parser (M1.6).
    Bytes,
    /// An identifier (L§3.4).
    Ident,
    /// A keyword (L§3.5).
    Keyword(Keyword),
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `//`
    SlashSlash,
    /// `%`
    Percent,
    /// `**`
    StarStar,
    /// `==`
    EqEq,
    /// `!=`
    BangEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `=`
    Eq,
    /// `.`
    Dot,
    /// `.*` — the import wildcard (L§11.2). Lexed as one token (and, unlike a
    /// bare `*`, not a continuation trigger) so `import m.*` ends its line.
    DotStar,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// A statement separator from a line break (L§3.2).
    Newline,
    /// `;`
    Semicolon,
    /// End of input.
    Eof,
}

impl TokenKind {
    /// Whether this token, appearing at the end of a line, continues the
    /// statement onto the next physical line (S-2, L§3.2). `=`, `not`, `.`, and
    /// `:` deliberately do not.
    pub fn is_continuation_trigger(self) -> bool {
        matches!(
            self,
            TokenKind::Plus
                | TokenKind::Minus
                | TokenKind::Star
                | TokenKind::Slash
                | TokenKind::SlashSlash
                | TokenKind::Percent
                | TokenKind::StarStar
                | TokenKind::EqEq
                | TokenKind::BangEq
                | TokenKind::Lt
                | TokenKind::Gt
                | TokenKind::Le
                | TokenKind::Ge
                | TokenKind::Comma
                | TokenKind::Keyword(Keyword::And)
                | TokenKind::Keyword(Keyword::Or)
                | TokenKind::Keyword(Keyword::Is)
        )
    }
}

/// A reserved keyword (L§3.5). `next` and `use` are reserved but unused by this
/// language version.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyword {
    /// `and`
    And,
    /// `as`
    As,
    /// `break`
    Break,
    /// `const`
    Const,
    /// `continue`
    Continue,
    /// `do`
    Do,
    /// `else`
    Else,
    /// `end`
    End,
    /// `exports`
    Exports,
    /// `extends`
    Extends,
    /// `false`
    False,
    /// `fn`
    Fn,
    /// `for`
    For,
    /// `if`
    If,
    /// `implement`
    Implement,
    /// `import`
    Import,
    /// `is`
    Is,
    /// `let`
    Let,
    /// `loop`
    Loop,
    /// `module`
    Module,
    /// `next` (reserved, unused)
    Next,
    /// `nil`
    Nil,
    /// `not`
    Not,
    /// `or`
    Or,
    /// `parameter`
    Parameter,
    /// `protocol`
    Protocol,
    /// `raise`
    Raise,
    /// `record`
    Record,
    /// `ref`
    Ref,
    /// `rescue`
    Rescue,
    /// `return`
    Return,
    /// `then`
    Then,
    /// `to`
    To,
    /// `true`
    True,
    /// `try`
    Try,
    /// `use` (reserved, unused)
    Use,
    /// `while`
    While,
    /// `with`
    With,
}

/// Returns the keyword for `word`, or `None` if it is an ordinary identifier.
///
/// A single `match` — deterministic, exhaustive, and greppable (no hashing).
pub fn keyword(word: &str) -> Option<Keyword> {
    let kw = match word {
        "and" => Keyword::And,
        "as" => Keyword::As,
        "break" => Keyword::Break,
        "const" => Keyword::Const,
        "continue" => Keyword::Continue,
        "do" => Keyword::Do,
        "else" => Keyword::Else,
        "end" => Keyword::End,
        "exports" => Keyword::Exports,
        "extends" => Keyword::Extends,
        "false" => Keyword::False,
        "fn" => Keyword::Fn,
        "for" => Keyword::For,
        "if" => Keyword::If,
        "implement" => Keyword::Implement,
        "import" => Keyword::Import,
        "is" => Keyword::Is,
        "let" => Keyword::Let,
        "loop" => Keyword::Loop,
        "module" => Keyword::Module,
        "next" => Keyword::Next,
        "nil" => Keyword::Nil,
        "not" => Keyword::Not,
        "or" => Keyword::Or,
        "parameter" => Keyword::Parameter,
        "protocol" => Keyword::Protocol,
        "raise" => Keyword::Raise,
        "record" => Keyword::Record,
        "ref" => Keyword::Ref,
        "rescue" => Keyword::Rescue,
        "return" => Keyword::Return,
        "then" => Keyword::Then,
        "to" => Keyword::To,
        "true" => Keyword::True,
        "try" => Keyword::Try,
        "use" => Keyword::Use,
        "while" => Keyword::While,
        "with" => Keyword::With,
        _ => return None,
    };
    Some(kw)
}
