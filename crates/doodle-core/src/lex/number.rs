//! Numeric-literal scanning (L§3.6.1/§3.6.2). Consumes maximally, then
//! validates the lexical shape; the literal *value* is lowered later (M1.6).

use crate::lex::token::TokenKind;

/// A scanned numeric literal.
pub(super) struct Number {
    /// Byte offset just past the literal.
    pub end: usize,
    /// `Int` or `Float`.
    pub kind: TokenKind,
    /// A malformed-number message, if the literal is ill-formed.
    pub error: Option<&'static str>,
}

const BAD_UNDERSCORE: &str = "underscores in a number must sit between digits";

/// Scans a numeric literal at `start`, where `source.as_bytes()[start]` is an
/// ASCII digit.
pub(super) fn scan_number(source: &str, start: usize) -> Number {
    let b = source.as_bytes();

    // Base-prefixed integer. The prefix is lowercase only (`0X`/`0B`/`0O` are
    // not prefixes — `0XFF` lexes as `0` then the identifier `XFF`).
    if b[start] == b'0' && start + 1 < b.len() {
        let is_digit: Option<fn(u8) -> bool> = match b[start + 1] {
            b'x' => Some(is_hex_digit),
            b'b' => Some(is_bin_digit),
            b'o' => Some(is_oct_digit),
            _ => None,
        };
        if let Some(is_digit) = is_digit {
            let run_start = start + 2;
            let end = consume_run(b, run_start, is_digit);
            let run = &source[run_start..end];
            let error = if !run.bytes().any(is_digit) {
                Some("this base prefix needs at least one digit")
            } else if !valid_underscores(run) {
                Some(BAD_UNDERSCORE)
            } else {
                None
            };
            return Number {
                end,
                kind: TokenKind::Int,
                error,
            };
        }
    }

    // Decimal integer, optionally a float via a fractional and/or exponent part.
    let int_end = consume_run(b, start, is_dec_digit);
    let mut end = int_end;
    let mut is_float = false;
    let mut error = if valid_underscores(&source[start..int_end]) {
        None
    } else {
        Some(BAD_UNDERSCORE)
    };

    // Fractional: `.` counts only when followed by a digit (`2.5` is a float;
    // `2.field` is `2`, `.`, `field`).
    if end < b.len() && b[end] == b'.' && end + 1 < b.len() && is_dec_digit(b[end + 1]) {
        is_float = true;
        let frac_start = end + 1;
        end = consume_run(b, frac_start, is_dec_digit);
        if error.is_none() && !valid_underscores(&source[frac_start..end]) {
            error = Some(BAD_UNDERSCORE);
        }
    }

    // Exponent: seeing `e`/`E` commits to an exponent, so a missing digit is a
    // malformed number (`1e`, `1e+`) rather than an identifier `e`. Consume
    // through the (possibly empty) digit run so the token spans `e`/`e+` even
    // when malformed.
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') {
        is_float = true;
        let mut j = end + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        end = consume_run(b, j, is_dec_digit);
        let run = &source[j..end];
        if error.is_none() {
            if !run.bytes().any(is_dec_digit) {
                error = Some("this exponent needs at least one digit");
            } else if !valid_underscores(run) {
                error = Some(BAD_UNDERSCORE);
            }
        }
    }

    let kind = if is_float {
        TokenKind::Float
    } else {
        TokenKind::Int
    };
    Number { end, kind, error }
}

/// Consumes the maximal run of digits (per `is_digit`) and underscores.
fn consume_run(b: &[u8], mut i: usize, is_digit: fn(u8) -> bool) -> usize {
    while i < b.len() && (is_digit(b[i]) || b[i] == b'_') {
        i += 1;
    }
    i
}

/// Whether `run` (digits and underscores) places every underscore between two
/// digits — no leading, trailing, or doubled underscore, and non-empty.
fn valid_underscores(run: &str) -> bool {
    let b = run.as_bytes();
    !b.is_empty() && b[0] != b'_' && b[b.len() - 1] != b'_' && !run.contains("__")
}

fn is_dec_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn is_hex_digit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn is_bin_digit(c: u8) -> bool {
    c == b'0' || c == b'1'
}

fn is_oct_digit(c: u8) -> bool {
    (b'0'..=b'7').contains(&c)
}
