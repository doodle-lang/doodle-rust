//! The one wrapper over the pinned Unicode crates (plan AD4). Everything that
//! needs Unicode data — NFC normalization and UAX#31 identifier classification
//! now, grapheme segmentation later (M4) — goes through here, so the crate and
//! UCD-version pins live in a single place.
//!
//! Identifier classification uses UAX#31 **`XID_Start`/`XID_Continue`** (via
//! `unicode-ident`), matching L§3.4: the NFC-closed variants, so an identifier
//! stays valid after the NFC normalization L applies (L§3.1). The plain `ID_*`
//! sets are not NFC-closed and differ from `XID` only at a few code points
//! (e.g. U+037A).

use std::borrow::Cow;
use std::fmt;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

/// A Unicode/UCD version, `major.minor.micro` (L§4.4). Names the release whose
/// normalization and (at M4) grapheme behavior an instance uses; carried in the
/// instance config's optional target-version field (E§3.1, S-41).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnicodeVersion {
    /// Major version (e.g. `16`).
    pub major: u16,
    /// Minor version.
    pub minor: u16,
    /// Micro (patch) version.
    pub micro: u16,
}

impl fmt::Display for UnicodeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

/// The UCD version this engine is pinned to (L§4.4, plan AD4). Read from the
/// `unicode-normalization` crate's **own** pin, so the constant can never drift from
/// the normalization tables actually linked in. The engine reports it in its
/// identity and a recording records it; a replay refuses a mismatch (AD4; E§11;
/// S-36). Identifier classification (L§3.4) runs through the separately-versioned
/// `unicode-ident` crate; the compile-time cross-check below guarantees it pins the
/// **same** UCD version, so this single reported version covers identifier lexing
/// too — not just normalization.
pub const UNICODE_VERSION: UnicodeVersion = UnicodeVersion {
    major: unicode_normalization::UNICODE_VERSION.0 as u16,
    minor: unicode_normalization::UNICODE_VERSION.1 as u16,
    micro: unicode_normalization::UNICODE_VERSION.2 as u16,
};

// The reported version must cover EVERY UCD-dependent, language-observable path, not
// just normalization. `unicode-ident` is versioned independently and — AD4 warns —
// has historically skewed from `unicode-normalization`; both are caret-pinned, so a
// routine `cargo update` could move one without the other. Both crates export their
// pinned UCD version, so cross-check them here: a skew is a **build failure**, not a
// silent lex/replay divergence (E§11) hidden behind a reported version that only
// reflects normalization. This is the compile-time half of AD4's per-crate pin
// verification; the behavioral XID conformance vector is deeper and lands at M4.
// (`unicode-segmentation` joins this check when grapheme segmentation lands, M4.)
const _: () = assert!(
    unicode_ident::UNICODE_VERSION.0 == unicode_normalization::UNICODE_VERSION.0
        && unicode_ident::UNICODE_VERSION.1 == unicode_normalization::UNICODE_VERSION.1
        && unicode_ident::UNICODE_VERSION.2 == unicode_normalization::UNICODE_VERSION.2,
    "unicode-ident and unicode-normalization pin different UCD versions: the engine's \
     reported Unicode version would not cover identifier classification (AD4, S-41)"
);

/// Normalizes `s` to Unicode Normalization Form C (L§3.1). Idempotent; borrows
/// without allocating when `s` is already NFC.
pub fn nfc(s: &str) -> Cow<'_, str> {
    if unicode_normalization::is_nfc(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.nfc().collect())
    }
}

/// Whether `s` is already in NFC. For preconditions/asserts on load-normalized
/// text (L§3.1).
pub fn is_nfc(s: &str) -> bool {
    unicode_normalization::is_nfc(s)
}

/// Concatenates two **NFC** strings, producing the NFC of their concatenation while
/// renormalizing only the **seam** (plan AD4, MD §5). The result is exactly
/// `nfc(&format!("{a}{b}"))`, but the prefix of `a` before its last normalization
/// boundary and the suffix of `b` after its first are already NFC and are copied
/// verbatim — only the boundary region is re-normalized. Both inputs must be NFC.
///
/// "Around the seam" is defined against true UAX #15 boundaries, not "back to the last
/// starter" naively: the region extends across `a`'s trailing non-starter run (canonical
/// reordering can move a mark from `b` into it) up to and including the starter that
/// anchors it, and across `b`'s leading non-starters **and** leading Hangul V/T jamo,
/// which compose backward onto a trailing L/LV even though they are starters.
pub fn seam_concat(a: &str, b: &str) -> String {
    debug_assert!(
        is_nfc(a) && is_nfc(b),
        "seam_concat requires NFC inputs (AD4)"
    );
    if a.is_empty() {
        return b.to_owned();
    }
    if b.is_empty() {
        return a.to_owned();
    }
    let a_split = seam_start_in_a(a);
    let b_split = seam_end_in_b(b);
    let mut out = String::with_capacity(a.len() + b.len());
    out.push_str(&a[..a_split]);
    let seam: String = a[a_split..]
        .chars()
        .chain(b[..b_split].chars())
        .nfc()
        .collect();
    out.push_str(&seam);
    out.push_str(&b[b_split..]);
    out
}

/// The byte index in `a` where the seam region begins: the start of `a`'s last
/// normalization unit — its trailing non-starter run together with the starter that
/// anchors it (or `0` if `a` has no starter). Everything before it is unaffected by an
/// append, since composition and canonical reordering never reach back past a starter.
fn seam_start_in_a(a: &str) -> usize {
    let mut anchor = 0;
    for (i, c) in a.char_indices().rev() {
        anchor = i;
        if canonical_combining_class(c) == 0 {
            break; // the starter anchoring the trailing non-starter run
        }
    }
    anchor
}

/// The byte index in `b` where the seam region ends: past `b`'s leading non-starters and
/// leading Hangul V/T jamo — the characters that can reorder into `a`'s tail or compose
/// backward onto a trailing L/LV. Stops at the first clean starter (a normal starter, or
/// an L jamo, which begins a fresh syllable and never composes backward).
fn seam_end_in_b(b: &str) -> usize {
    let mut end = 0;
    for (i, c) in b.char_indices() {
        if canonical_combining_class(c) == 0 && !is_backward_composing_jamo(c) {
            break;
        }
        end = i + c.len_utf8();
    }
    end
}

/// Whether `c` is a Hangul **V** (medial vowel) or **T** (trailing consonant) conjoining
/// jamo — a starter (CCC 0) that nonetheless composes *backward* onto a preceding L or LV
/// at a seam (L+V→LV, LV+T→LVT, UAX #15 §Hangul). L jamo and precomposed syllables do
/// not compose backward, so they are excluded.
fn is_backward_composing_jamo(c: char) -> bool {
    let u = c as u32;
    (0x1161..=0x1175).contains(&u) || (0x11A8..=0x11C2).contains(&u)
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

    /// The seam concat must equal whole-string NFC of the concatenation — the AD4
    /// correctness contract. Covers ASCII, seam composition, canonical reordering across
    /// the seam, Hangul jamo (L|V and LV|T), non-starter runs, empties, and the
    /// regional-indicator case (RIs do not compose under NFC, so the seam is a no-op).
    /// The seam concat must equal whole-string NFC of the concatenation — the AD4
    /// correctness contract. Inputs are normalized first (seam_concat requires NFC),
    /// chosen so the seam still does real work: composition, canonical reordering, and
    /// Hangul jamo (L|V, LV|T) all straddle the join.
    #[test]
    fn seam_concat_equals_whole_string_nfc() {
        let raw: &[(&str, &str)] = &[
            ("ab", "cd"),                     // pure ASCII — no seam work
            ("e", "\u{301}"),                 // base + lone acute → é composes at the seam
            ("cafe", "\u{301}"),              // composition reaches back to `e`
            ("e\u{301}", "\u{323}"),          // é + dot-below → reorder + recompose at seam
            ("\u{1100}", "\u{1161}"),         // Hangul L + V → 가
            ("\u{ac00}", "\u{11a8}"),         // Hangul LV (가) + T → 각
            ("\u{1100}", "\u{1161}\u{11a8}"), // L + (V T) → 각
            ("\u{301}", "\u{300}"),           // two lone non-starters (no starter in `a`)
            ("\u{1f1fa}", "\u{1f1f8}"),       // RI + RI — no NFC composition (seam no-op)
            ("", "abc"),                      // empty left
            ("abc", ""),                      // empty right
            ("A\u{30a}", "b"),                // Å (from A + ring) then a clean starter
        ];
        for (a_raw, b_raw) in raw {
            let a: String = a_raw.nfc().collect();
            let b: String = b_raw.nfc().collect();
            let whole: String = format!("{a}{b}").nfc().collect();
            assert_eq!(
                seam_concat(&a, &b),
                whole,
                "seam != whole-string NFC for {a:?}+{b:?}"
            );
            assert!(
                is_nfc(&seam_concat(&a, &b)),
                "seam result not NFC: {a:?}+{b:?}"
            );
        }
    }

    /// Exhaustive small-alphabet check: over every pair of NFC strings of length ≤ 2 built
    /// from bases, combining marks of two combining classes, and Hangul L/V/T/LV jamo, the
    /// seam concat must equal whole-string NFC. Catches boundary bugs the hand-picked cases
    /// might miss (wrong reorder window, jamo mishandled, an off-by-one seam split).
    #[test]
    fn seam_concat_matches_whole_string_exhaustively() {
        let alphabet = [
            'a', 'e', '\u{301}', '\u{323}', '\u{300}', '\u{1100}', '\u{1161}', '\u{11a8}',
            '\u{ac00}',
        ];
        let mut strings: Vec<String> = alphabet.iter().map(|c| c.to_string()).collect();
        for &c1 in &alphabet {
            for &c2 in &alphabet {
                strings.push(format!("{c1}{c2}"));
            }
        }
        let nfc_strings: Vec<String> = strings.iter().map(|s| s.nfc().collect()).collect();
        for a in &nfc_strings {
            for b in &nfc_strings {
                let whole: String = format!("{a}{b}").nfc().collect();
                assert_eq!(seam_concat(a, b), whole, "seam mismatch for {a:?}+{b:?}");
            }
        }
    }

    #[test]
    fn uses_xid_not_id() {
        // U+037A (GREEK YPOGEGRAMMENI) is ID_Continue but NOT XID_Continue;
        // using XID (L§3.4) means it is not an identifier-continue character.
        assert!(!is_ident_continue('\u{037A}'));
    }
}
