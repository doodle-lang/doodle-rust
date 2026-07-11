//! Plain double-quoted string boundary scanning (L§3.6.3). M1.3 lexes only the
//! boundary; escape *processing*, interpolation, and bytes literals are M1.4,
//! and triple-quoted strings are M1.5 — this is the seam they grow into.

/// A scanned plain string literal.
pub(super) struct StringLit {
    /// Byte offset just past the closing quote, or past the last byte consumed
    /// if the string is unterminated.
    pub end: usize,
    /// Whether the string reached a newline or end of input while still open.
    pub unterminated: bool,
}

/// Scans a plain string at `start`, where `source.as_bytes()[start]` is `"`. A
/// backslash escapes the next byte, so `\"` does not close the string; a
/// newline or end of input before the closing quote is unterminated.
pub(super) fn scan_string(source: &str, start: usize) -> StringLit {
    let b = source.as_bytes();
    let mut i = start + 1; // past the opening quote
    while i < b.len() {
        match b[i] {
            b'"' => {
                return StringLit {
                    end: i + 1,
                    unterminated: false,
                };
            }
            b'\n' => break,
            // A backslash escapes the next byte, unless that byte is a newline
            // (a plain string does not span lines at M1.3).
            b'\\' if i + 1 < b.len() && b[i + 1] != b'\n' => i += 2,
            _ => i += 1,
        }
    }
    StringLit {
        end: i,
        unterminated: true,
    }
}
