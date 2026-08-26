//! UCD conformance vectors (plan M4.10): the engine's Unicode primitives checked against
//! the official Unicode Character Database test suites for the pinned version (AD4, D-M4-3:
//! UCD 17.0). Two files are vendored under `tests/data/ucd/` (they carry their own Unicode
//! license header):
//!
//! - `NormalizationTest.txt` drives [`nfc`]: `toNFC(source) == NFC` and `toNFC(NFD) == NFC`
//!   for every row, plus the **AD4 seam** law — for every split of a row's NFD form,
//!   `seam_concat(nfc(left), nfc(right))` equals the whole's NFC — exercising the engine's
//!   own seam pass ([`seam_concat`]) on real composition data, not just a synthetic alphabet.
//! - `GraphemeBreakTest.txt` drives [`grapheme_offsets`]: the extended-grapheme-cluster
//!   starts it reports match UAX #29's expected boundaries for every row.
//!
//! These guard determinism (E§11): a `cargo update` that silently moved a `unicode-*` crate
//! off UCD 17.0 would diverge here even if it still *reported* 17.0 (the compile-time
//! version cross-check in `unicode.rs` cannot see a data regression).
//!
//! [`nfc`]: doodle_core::unicode::nfc
//! [`seam_concat`]: doodle_core::unicode::seam_concat
//! [`grapheme_offsets`]: doodle_core::unicode::grapheme_offsets

use doodle_core::unicode::{UNICODE_VERSION, grapheme_offsets, nfc, seam_concat};

const NORMALIZATION_TEST: &str = include_str!("data/ucd/NormalizationTest.txt");
const GRAPHEME_BREAK_TEST: &str = include_str!("data/ucd/GraphemeBreakTest.txt");

/// The version the vendored vectors are for; the engine must be pinned to it, or the
/// vectors are testing a different Unicode than the engine implements.
const VECTORS_VERSION: (u16, u16, u16) = (17, 0, 0);

/// Parses a space-separated run of hex code points (`"0044 0307"`) into a `String`.
/// Returns `None` if any code point is not a Unicode scalar (a lone surrogate) — such rows
/// cannot be represented as a Rust `str` and are skipped (they do not occur in these files).
fn code_points(field: &str) -> Option<String> {
    field
        .split_whitespace()
        .map(|hex| char::from_u32(u32::from_str_radix(hex, 16).ok()?))
        .collect()
}

#[test]
fn the_engine_is_pinned_to_the_vectors_unicode_version() {
    assert_eq!(
        (
            UNICODE_VERSION.major,
            UNICODE_VERSION.minor,
            UNICODE_VERSION.micro
        ),
        VECTORS_VERSION,
        "the engine's UCD version must match the vendored conformance vectors"
    );
}

#[test]
fn nfc_matches_the_ucd_normalization_test_suite() {
    let mut rows = 0;
    for line in NORMALIZATION_TEST.lines() {
        // Rows: `source; NFC; NFD; NFKC; NFKD; # comment`. Skip comments and `@PartN` headers.
        let data = line.split('#').next().unwrap_or("").trim();
        if data.is_empty() || data.starts_with('@') {
            continue;
        }
        let cols: Vec<&str> = data.split(';').collect();
        let (Some(source), Some(nfc_form), Some(nfd_form)) = (
            code_points(cols[0]),
            code_points(cols[1]),
            code_points(cols[2]),
        ) else {
            continue;
        };
        // toNFC(source) == NFC, and toNFC(NFD) == NFC (recomposition).
        assert_eq!(nfc(&source).as_ref(), nfc_form, "toNFC({source:?})");
        assert_eq!(nfc(&nfd_form).as_ref(), nfc_form, "toNFC(NFD {nfd_form:?})");
        rows += 1;
    }
    assert!(
        rows > 10_000,
        "expected the full suite, parsed only {rows} rows"
    );
}

#[test]
fn seam_concat_matches_whole_string_nfc_over_the_ucd_suite() {
    // The AD4 seam law on real UCD data: splitting a decomposed (NFD) sequence anywhere and
    // rejoining the NFC'd halves at the seam must equal the whole's NFC.
    let mut splits = 0;
    for line in NORMALIZATION_TEST.lines() {
        let data = line.split('#').next().unwrap_or("").trim();
        if data.is_empty() || data.starts_with('@') {
            continue;
        }
        let cols: Vec<&str> = data.split(';').collect();
        let (Some(nfc_form), Some(nfd_form)) = (code_points(cols[1]), code_points(cols[2])) else {
            continue;
        };
        for (boundary, _) in nfd_form.char_indices().skip(1) {
            let (left, right) = nfd_form.split_at(boundary);
            let (l, r) = (nfc(left), nfc(right));
            assert_eq!(
                seam_concat(&l, &r),
                nfc_form,
                "seam of {nfd_form:?} split at byte {boundary}"
            );
            splits += 1;
        }
    }
    assert!(
        splits > 10_000,
        "expected many seam splits, ran only {splits}"
    );
}

#[test]
fn grapheme_offsets_match_the_ucd_segmentation_test_suite() {
    let mut rows = 0;
    for line in GRAPHEME_BREAK_TEST.lines() {
        // Rows: `÷ 0061 × 0308 ÷ ... # comment`. `÷` is a cluster break, `×` a non-break;
        // a code point preceded by `÷` starts a new grapheme cluster (at its byte offset).
        let data = line.split('#').next().unwrap_or("");
        if data.trim().is_empty() {
            continue;
        }
        let mut string = String::new();
        let mut expected_starts = Vec::new();
        let mut at_break = false;
        let mut malformed = false;
        for tok in data.split_whitespace() {
            match tok {
                "\u{00F7}" => at_break = true,  // ÷
                "\u{00D7}" => at_break = false, // ×
                hex => {
                    let Some(cp) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    else {
                        malformed = true;
                        break;
                    };
                    if at_break {
                        expected_starts.push(string.len() as u32);
                    }
                    string.push(cp);
                    at_break = false;
                }
            }
        }
        if malformed || string.is_empty() {
            continue;
        }
        assert_eq!(
            grapheme_offsets(&string).as_ref(),
            expected_starts.as_slice(),
            "grapheme boundaries for {string:?}"
        );
        rows += 1;
    }
    assert!(
        rows > 500,
        "expected the full suite, parsed only {rows} rows"
    );
}
