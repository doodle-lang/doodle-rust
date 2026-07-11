//! Escape-shape validation for string and bytes literals (L§3.6.3/§3.6.5).
//!
//! The escape set is **closed**: this validates the *shape* of an escape and
//! reports an unknown or malformed one; the escape's decoded value is produced
//! later by the parser (M1.6), like numeric literals. `\xHH` is a code point
//! `U+00HH` in a string and a byte `0xHH` in `b"…"` — the same spelling, the
//! type's natural unit — so only the surrounding literal kind differs here.

use crate::diag::code::DiagnosticCode;
use crate::span::Span;

/// A shape error found in an escape: which class, a kid-readable message, and
/// the span to point at.
pub(super) struct EscapeError {
    pub code: DiagnosticCode,
    pub message: &'static str,
    pub span: Span,
}

/// The result of scanning one escape: the byte offset just past it (for the
/// text run to resume from) and any shape error.
pub(super) struct Escape {
    pub end: usize,
    pub error: Option<EscapeError>,
}

/// Validates the escape beginning at `bs`, where `source.as_bytes()[bs] == b'\\'`.
/// `bytes_literal` selects the `b"…"` rules: `\u{…}` is rejected there.
pub(super) fn scan_escape(source: &str, bs: usize, bytes_literal: bool) -> Escape {
    let b = source.as_bytes();
    match b.get(bs + 1) {
        // A backslash at EOF or right before a newline is not an escape; leave
        // the terminator for the caller's unterminated-literal handling.
        None | Some(b'\n') => Escape {
            end: bs + 1,
            error: None,
        },
        Some(b'"' | b'\\' | b'n' | b't' | b'r' | b'0') => Escape {
            end: bs + 2,
            error: None,
        },
        Some(b'x') => scan_hex(b, bs),
        Some(b'u') if bytes_literal => Escape {
            end: bs + 2,
            error: Some(err(
                DiagnosticCode::MalformedEscape,
                "`\\u` isn't allowed in a bytes literal — use `\\xHH`",
                bs,
                bs + 2,
            )),
        },
        Some(b'u') => scan_unicode(b, bs),
        // Any other character — take the whole char, so a non-ASCII `\é` is
        // reported as one unit.
        Some(_) => {
            let ch = source[bs + 1..].chars().next().unwrap_or('\u{FFFD}');
            let end = bs + 1 + ch.len_utf8();
            Escape {
                end,
                error: Some(err(
                    DiagnosticCode::UnknownEscape,
                    "this isn't a valid escape — write `\\\\` for a literal backslash",
                    bs,
                    end,
                )),
            }
        }
    }
}

/// `\xHH` — exactly two hex digits.
fn scan_hex(b: &[u8], bs: usize) -> Escape {
    let is_hex = |i: usize| b.get(i).is_some_and(u8::is_ascii_hexdigit);
    if is_hex(bs + 2) && is_hex(bs + 3) {
        return Escape {
            end: bs + 4,
            error: None,
        };
    }
    // Recover past `\x` and the one hex digit that may be present.
    let end = if is_hex(bs + 2) { bs + 3 } else { bs + 2 };
    Escape {
        end,
        error: Some(err(
            DiagnosticCode::MalformedEscape,
            "`\\x` needs exactly two hex digits, like `\\x1B`",
            bs,
            end,
        )),
    }
}

/// `\u{H…}` — 1–6 hex digits naming a non-surrogate Unicode scalar value.
fn scan_unicode(b: &[u8], bs: usize) -> Escape {
    if b.get(bs + 2) != Some(&b'{') {
        return Escape {
            end: bs + 2,
            error: Some(err(
                DiagnosticCode::MalformedEscape,
                "`\\u` needs braces, like `\\u{E9}`",
                bs,
                bs + 2,
            )),
        };
    }
    let mut i = bs + 3;
    let mut digits = 0u32;
    let mut value = 0u32;
    while let Some(&c) = b.get(i) {
        let Some(d) = (c as char).to_digit(16) else {
            break;
        };
        value = value.saturating_mul(16).saturating_add(d);
        digits += 1;
        i += 1;
    }
    if b.get(i) != Some(&b'}') {
        return Escape {
            end: i,
            error: Some(err(
                DiagnosticCode::MalformedEscape,
                "`\\u{…}` is missing its closing `}`",
                bs,
                i,
            )),
        };
    }
    let end = i + 1;
    let message = if digits == 0 {
        Some("`\\u{}` needs at least one hex digit")
    } else if digits > 6 {
        Some("`\\u{…}` takes at most six hex digits")
    } else if (0xD800..=0xDFFF).contains(&value) {
        Some("`\\u{…}` can't name a surrogate (`D800`–`DFFF`)")
    } else if value > 0x10FFFF {
        Some("`\\u{…}` is above the largest code point `10FFFF`")
    } else {
        None
    };
    Escape {
        end,
        error: message.map(|m| err(DiagnosticCode::MalformedEscape, m, bs, end)),
    }
}

fn err(code: DiagnosticCode, message: &'static str, start: usize, end: usize) -> EscapeError {
    EscapeError {
        code,
        message,
        span: Span::new(start as u32, end as u32),
    }
}
