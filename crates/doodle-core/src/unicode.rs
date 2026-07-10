//! The one wrapper over the pinned Unicode crates (plan AD4). Everything that
//! needs Unicode data — NFC normalization and UAX#31 identifier classification
//! now, grapheme segmentation later (M4) — goes through here, so the crate and
//! UCD-version pins live in a single place.
//!
//! Identifier classification uses UAX#31 **`XID_Start`/`XID_Continue`** (via
//! `unicode-ident`) — the NFC-closed variants, the right choice for a language
//! that NFC-normalizes source and compares identifiers by NFC equality
//! (L§3.1/§3.4): an `XID` identifier stays valid after normalization, whereas
//! the plain `ID_*` sets are not closed under NFC. L§3.4 currently writes the
//! non-closed `ID_Start`/`ID_Continue`; that divergence is filed as a spec-delta
//! (recommend L§3.4 → `XID`). The two differ only at a few code points (e.g.
//! U+037A).

use std::borrow::Cow;
use unicode_normalization::UnicodeNormalization;

/// Normalizes `s` to Unicode Normalization Form C (L§3.1). Idempotent; borrows
/// without allocating when `s` is already NFC.
pub fn nfc(s: &str) -> Cow<'_, str> {
    if unicode_normalization::is_nfc(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.nfc().collect())
    }
}

/// Whether `c` may start an identifier (L§3.4): `_`, or a UAX#31 `XID_Start`
/// character. `unicode-ident` excludes `_`, so it is added explicitly.
pub fn is_ident_start(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_start(c)
}

/// Whether `c` may continue an identifier (L§3.4): `_`, or a UAX#31
/// `XID_Continue` character.
pub fn is_ident_continue(c: char) -> bool {
    c == '_' || unicode_ident::is_xid_continue(c)
}

/// Whether `s` has the lexical shape of an identifier (L§3.4:
/// `ID_START ID_CONTINUE*`). Emoji and other non-UAX#31 characters are excluded
/// by the underlying properties.
///
/// This is the shape only; it does **not** exclude keywords (L§3.5) — that is
/// the lexer's keyword-table check (M1.3). `s` is expected to be NFC, since
/// identifiers are compared by NFC code-point equality (L§3.4).
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => is_ident_start(first) && chars.all(is_ident_continue),
        None => false,
    }
}

/// Whether `s` is a valid module name (L§3.4): `[a-z][a-z0-9_]*` — lowercase
/// ASCII letters, digits, and underscores, beginning with a letter.
pub fn is_module_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => {
            first.is_ascii_lowercase()
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_unifies_composed_and_decomposed() {
        let composed = "caf\u{e9}"; // café with U+00E9
        let decomposed = "cafe\u{301}"; // cafe + combining acute
        assert_eq!(nfc(composed), nfc(decomposed));
        assert!(matches!(nfc(composed), Cow::Borrowed(_))); // already NFC: borrow
        assert!(matches!(nfc(decomposed), Cow::Owned(_)));
        assert_eq!(
            nfc(nfc(decomposed).as_ref()).as_ref(),
            nfc(composed).as_ref()
        );
    }

    #[test]
    fn identifier_classification() {
        assert!(is_identifier("_"));
        assert!(is_identifier("café"));
        assert!(is_identifier("θ"));
        assert!(is_identifier("длина"));
        assert!(is_identifier("角度"));
        assert!(is_identifier("x2"));
        assert!(is_identifier("end")); // keywords are lexically identifiers here
        assert!(!is_identifier("2x")); // digit start
        assert!(!is_identifier("foo-bar"));
        assert!(!is_identifier("🐢")); // emoji excluded by UAX#31
        assert!(!is_identifier(""));
        assert!(!is_identifier("\u{301}x")); // a combining mark cannot START
        assert!(is_identifier("e\u{301}x")); // ...but is a valid CONTINUE char
    }

    #[test]
    fn module_name_rule() {
        assert!(is_module_name("turtle"));
        assert!(is_module_name("my_module"));
        assert!(is_module_name("m2"));
        assert!(!is_module_name("Turtle"));
        assert!(!is_module_name("2mod"));
        assert!(!is_module_name("_x"));
        assert!(!is_module_name("mod-name"));
        assert!(!is_module_name("café")); // non-ASCII not allowed in module names
        assert!(!is_module_name(""));
    }
}
