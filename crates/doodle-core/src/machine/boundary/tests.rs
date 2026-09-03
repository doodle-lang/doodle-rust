//! Native tests for the handle boundary (engine spec E§4): value constructors and
//! typed readers over a `Ready` [`Instance`], including the arbitrary-precision integer
//! path that carries a bignum across the boundary without an `i64` ceiling.

use super::*;
use crate::drive::Limits;
use crate::machine::Registry;
use crate::span::ModuleId;

/// A `Ready` instance over a trivial clean-loading program — the boundary API
/// needs only the heap + handle table, not a running program.
fn instance() -> Instance {
    use crate::diag::Severity;
    let nfc = crate::source::normalize("1\n");
    let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
    assert!(
        !parsed
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error),
        "unexpected parse error(s): {:?}",
        parsed.diagnostics
    );
    let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
    assert!(
        resolved.diagnostics.is_empty(),
        "unexpected resolve diagnostic(s): {:?}",
        resolved.diagnostics
    );
    Instance::load(resolved.module, Limits::default(), Registry::new(), "main")
}

#[test]
fn int_round_trips_and_reports_its_kind() {
    let mut inst = instance();
    let h = inst.make_int(42);
    assert_eq!(inst.kind_of(h), Ok(Kind::Int));
    assert_eq!(inst.as_int(h), Ok(42));
    assert_eq!(inst.is_nil(h), Ok(false));
}

#[test]
fn a_typed_reader_on_the_wrong_kind_reports_both_kinds() {
    let mut inst = instance();
    let h = inst.make_bool(true);
    assert_eq!(inst.as_bool(h), Ok(true));
    assert_eq!(
        inst.as_int(h),
        Err(ValueError::WrongKind {
            expected: Kind::Int,
            got: Kind::Bool,
        })
    );
}

#[test]
fn a_bignum_is_int_kind_but_out_of_i64_range() {
    let mut inst = instance();
    // A bignum value (exceeds i64) constructed directly on the heap — no `make_*`
    // produces one (they take host scalars), but arithmetic does (M2a.3).
    let big = inst.heap.alloc_bigint(num_bigint::BigInt::from(i128::MAX));
    let h = inst.machine.handles.intern(Value::BigInt(big));
    assert_eq!(inst.kind_of(h), Ok(Kind::Int));
    assert_eq!(inst.as_int(h), Err(ValueError::IntOutOfRange));
    // The arbitrary-precision reader renders the same bignum in full (no i64 ceiling).
    assert_eq!(inst.as_int_decimal(h), Ok(i128::MAX.to_string()));
}

#[test]
fn make_int_decimal_round_trips_a_bignum_beyond_i64() {
    let mut inst = instance();
    // 2^128, comfortably past i64 — the host embedding builds this from a JS bigint.
    let text = "340282366920938463463374607431768211456";
    let h = inst.make_int_decimal(text).unwrap();
    assert_eq!(inst.kind_of(h), Ok(Kind::Int));
    assert_eq!(inst.as_int_decimal(h), Ok(text.to_string()));
    // It is a genuine bignum: the fixed-width reader still refuses it.
    assert_eq!(inst.as_int(h), Err(ValueError::IntOutOfRange));
}

#[test]
fn make_int_decimal_canonicalizes_an_i64_magnitude_to_a_machine_word() {
    let mut inst = instance();
    // A magnitude fitting i64 must intern as a machine-word `Int` (MD §3), so the
    // fixed-width reader accepts it — the decimal path is not a separate integer type.
    let h = inst.make_int_decimal("-42").unwrap();
    assert_eq!(inst.as_int(h), Ok(-42));
    assert_eq!(inst.as_int_decimal(h), Ok("-42".to_string()));
}

#[test]
fn make_int_decimal_rejects_non_integer_text() {
    let mut inst = instance();
    assert_eq!(inst.make_int_decimal("1.5"), Err(ValueError::MalformedInt));
    assert_eq!(inst.make_int_decimal("0x1f"), Err(ValueError::MalformedInt));
    assert_eq!(inst.make_int_decimal(""), Err(ValueError::MalformedInt));
}

#[test]
fn make_float_canonicalizes_nan_and_passes_infinities_through() {
    let mut inst = instance();
    let nan = inst.make_float(f64::NAN);
    assert_eq!(
        inst.as_float(nan).unwrap().to_bits(),
        crate::machine::values::CANONICAL_NAN_BITS
    );
    // A NaN with a different payload/sign is canonicalized to the same pattern.
    let other_nan = inst.make_float(f64::from_bits(0xFFF8_0000_0000_0001));
    assert_eq!(
        inst.as_float(other_nan).unwrap().to_bits(),
        crate::machine::values::CANONICAL_NAN_BITS
    );
    // ±∞ is inert data (S-56): stored, not canonicalized away.
    let inf = inst.make_float(f64::INFINITY);
    assert_eq!(inst.as_float(inf), Ok(f64::INFINITY));
    let neg_inf = inst.make_float(f64::NEG_INFINITY);
    assert_eq!(inst.as_float(neg_inf), Ok(f64::NEG_INFINITY));
    // -0.0 is a distinct value preserved bit-for-bit: S-28 makes -0.0 == 0.0 for
    // equality, but only NaN is bit-canonicalized, so the sign bit survives.
    let neg_zero = inst.make_float(-0.0);
    assert_eq!(
        inst.as_float(neg_zero).unwrap().to_bits(),
        (-0.0f64).to_bits()
    );
}

#[test]
fn make_string_validates_utf8_and_normalizes_to_nfc() {
    let mut inst = instance();
    let composed = "caf\u{e9}"; // "café" with a precomposed é (NFC)
    let decomposed = "cafe\u{301}"; // e + combining acute (NFD)
    // Non-NFC input is normalized; string_bytes round-trips to the NFC form.
    let h = inst.make_string(decomposed.as_bytes()).unwrap();
    assert_eq!(inst.kind_of(h), Ok(Kind::String));
    assert_eq!(inst.string_bytes(h), Ok(composed.as_bytes()));
    // Already-NFC input round-trips unchanged.
    let h2 = inst.make_string(composed.as_bytes()).unwrap();
    assert_eq!(inst.string_bytes(h2), Ok(composed.as_bytes()));
}

#[test]
fn make_string_rejects_invalid_utf8() {
    let mut inst = instance();
    // The error carries the byte offset of the first invalid sequence (here 0), the same
    // position Doodle `decode` names — one story across the boundary (S-30/S-58).
    assert_eq!(
        inst.make_string(&[0x61, 0xff, 0xfe]),
        Err(ValueError::InvalidUtf8 { position: 1 })
    );
}

#[test]
fn bytes_are_raw_and_uninterpreted() {
    let mut inst = instance();
    let h = inst.make_bytes(&[0xff, 0x00, 0x80]);
    assert_eq!(inst.kind_of(h), Ok(Kind::Bytes));
    assert_eq!(inst.as_bytes(h), Ok(&[0xff, 0x00, 0x80][..]));
}

#[test]
fn nil_reads_as_nil() {
    let mut inst = instance();
    let h = inst.make_nil();
    assert_eq!(inst.kind_of(h), Ok(Kind::Nil));
    assert_eq!(inst.is_nil(h), Ok(true));
}

#[test]
fn a_list_is_built_and_read_back() {
    let mut inst = instance();
    let list = inst.make_list();
    assert_eq!(inst.list_length(list), Ok(0));
    let a = inst.make_int(10);
    let b = inst.make_int(20);
    inst.list_append(list, a).unwrap();
    inst.list_append(list, b).unwrap();
    assert_eq!(inst.list_length(list), Ok(2));
    let got0 = inst.list_get(list, 0).unwrap();
    let got1 = inst.list_get(list, 1).unwrap();
    assert_eq!(inst.as_int(got0), Ok(10));
    assert_eq!(inst.as_int(got1), Ok(20));
    assert_eq!(inst.list_get(list, 2), Err(ValueError::IndexOutOfBounds));
}

#[test]
fn list_append_onto_a_non_list_reports_wrong_kind() {
    let mut inst = instance();
    let notlist = inst.make_int(1);
    let v = inst.make_int(2);
    assert_eq!(
        inst.list_append(notlist, v),
        Err(ValueError::WrongKind {
            expected: Kind::List,
            got: Kind::Int,
        })
    );
}

#[test]
fn a_released_handle_reads_as_stale() {
    let mut inst = instance();
    let h = inst.make_int(7); // refs = 1
    inst.release(h).unwrap(); // refs = 0 → freed
    assert_eq!(inst.kind_of(h), Err(ValueError::Stale));
    assert_eq!(inst.as_int(h), Err(ValueError::Stale));
}

#[test]
fn a_constructed_value_survives_collection_while_held() {
    let mut inst = instance();
    let h = inst.make_string("hello".as_bytes()).unwrap();
    inst.force_collect(); // the handle roots the string
    assert_eq!(inst.string_bytes(h), Ok("hello".as_bytes()));
}
