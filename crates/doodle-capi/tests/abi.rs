//! Integration tests for the C ABI (M7.1), calling the `extern "C"` functions through the
//! crate's `rlib` target (a staticlib crate hosts no test target; the `rlib` is added for
//! exactly this, and for `cargo miri test` in M7.6 — D-M7-11). These exercise the frozen
//! surface end-to-end: load → drive → outcome, handle round-trips, copy-out, and the
//! canonicalizing constructors. The `unsafe` mirrors a C host's calls.

// Each `unsafe` block below is a single FFI call made exactly as a C host would, with its
// preconditions (a live instance, valid out-params) established by the surrounding test
// setup — a per-call SAFETY comment would only restate that. The library's own `unsafe` (the
// actual pointer marshalling) is fully documented; this allow is scoped to the test file.
#![allow(clippy::undocumented_unsafe_blocks)]

use doodle_capi::abi::{
    DoodleDirective, DoodleFault, DoodleKind, DoodleOutcome, DoodleOutcomeKind, DoodleStatus,
};
use doodle_capi::config::{doodle_config_free, doodle_config_new, doodle_config_set_limits};
use doodle_capi::instance::{
    DoodleInstance, doodle_drive, doodle_free, doodle_load, doodle_output, doodle_raised_kind,
};
use doodle_capi::value::{
    doodle_as_bool, doodle_as_int, doodle_as_int_decimal, doodle_kind_of, doodle_make_int,
    doodle_make_string, doodle_release, doodle_string_bytes,
};
use std::ptr;

/// Loads `source` with the default config, asserting success and returning the instance.
fn load(source: &str) -> *mut DoodleInstance {
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    let status = unsafe {
        doodle_load(
            source.as_ptr(),
            source.len(),
            ptr::null(),
            &mut inst,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, DoodleStatus::Ok, "load failed for {source:?}");
    assert!(!inst.is_null());
    inst
}

/// Drives an instance to a terminal/pause under `RunToCompletion`, returning the outcome.
fn drive(inst: *mut DoodleInstance) -> DoodleOutcome {
    let mut out = DoodleOutcome::blank();
    let status = unsafe { doodle_drive(inst, DoodleDirective::RunToCompletion, &mut out) };
    assert_eq!(status, DoodleStatus::Ok);
    out
}

#[test]
fn version_and_abi_version_are_reported() {
    let v = unsafe { std::ffi::CStr::from_ptr(doodle_capi::doodle_version()) };
    assert!(!v.to_bytes().is_empty());
    // The header pins DOODLE_ABI_VERSION_MAJOR=0, MINOR=1 → (0 << 16) | 1.
    assert_eq!(doodle_capi::doodle_abi_version(), 1);
}

#[test]
fn a_clean_program_loads_and_completes() {
    let inst = load("let a = 1 + 2\n");
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Completed);
    assert_eq!(out.value, 0, "a module drive completes Void");
    unsafe { doodle_free(inst) };
}

#[test]
fn an_uncaught_raise_surfaces_with_its_described_kind() {
    let inst = load("1 / 0\n");
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Raised);
    assert!(out.has_span, "the raising site is known");
    // Copy out the kind slug (copy-out: ask the length, then fill).
    let mut len = 0usize;
    let status = unsafe { doodle_raised_kind(inst, ptr::null_mut(), 0, &mut len) };
    assert_eq!(status, DoodleStatus::ErrBufferTooSmall);
    let mut buf = vec![0u8; len];
    let status = unsafe { doodle_raised_kind(inst, buf.as_mut_ptr(), buf.len(), &mut len) };
    assert_eq!(status, DoodleStatus::Ok);
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "division-by-zero");
    unsafe { doodle_free(inst) };
}

#[test]
fn a_tiny_step_budget_faults_step_budget() {
    let config = doodle_config_new();
    // A tiny step budget trips at a safe point inside the loop.
    unsafe { doodle_config_set_limits(config, 50, 1 << 20, 1000, u64::MAX) };
    let src = "loop do\n1\nend\n";
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    let status = unsafe {
        doodle_load(
            src.as_ptr(),
            src.len(),
            config,
            &mut inst,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, DoodleStatus::Ok);
    unsafe { doodle_config_free(config) };
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Faulted);
    assert_eq!(out.fault, DoodleFault::LimitStepBudget);
    unsafe { doodle_free(inst) };
}

#[test]
fn a_load_error_reports_err_load_with_a_message() {
    let src = "let x =\n"; // a `let` with no initializer — a parse error
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    let mut buf = [0u8; 256];
    let mut len = 0usize;
    let status = unsafe {
        doodle_load(
            src.as_ptr(),
            src.len(),
            ptr::null(),
            &mut inst,
            buf.as_mut_ptr(),
            buf.len(),
            &mut len,
        )
    };
    assert_eq!(status, DoodleStatus::ErrLoad);
    assert!(inst.is_null(), "no instance on a load error");
    assert!(len > 0, "an error message was written");
    assert!(std::str::from_utf8(&buf[..len]).is_ok());
}

#[test]
fn handle_round_trips_and_release_makes_stale() {
    let inst = load("1\n");
    // int round-trip
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_make_int(inst, -7, &mut h) },
        DoodleStatus::Ok
    );
    let mut n = 0i64;
    assert_eq!(unsafe { doodle_as_int(inst, h, &mut n) }, DoodleStatus::Ok);
    assert_eq!(n, -7);
    // kind_of
    let mut kind = DoodleKind::Nil;
    assert_eq!(
        unsafe { doodle_kind_of(inst, h, &mut kind) },
        DoodleStatus::Ok
    );
    assert_eq!(kind, DoodleKind::Int);
    // wrong-kind: read an int as a bool
    let mut b = false;
    assert_eq!(
        unsafe { doodle_as_bool(inst, h, &mut b) },
        DoodleStatus::ErrWrongKind
    );
    // release, then a use is stale
    assert_eq!(unsafe { doodle_release(inst, h) }, DoodleStatus::Ok);
    assert_eq!(
        unsafe { doodle_as_int(inst, h, &mut n) },
        DoodleStatus::ErrStaleHandle
    );
    unsafe { doodle_free(inst) };
}

#[test]
fn string_round_trips_by_copy_out_and_reports_needed_length() {
    let inst = load("1\n");
    let text = "héllo"; // multi-byte, NFC
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_make_string(inst, text.as_ptr(), text.len(), &mut h) },
        DoodleStatus::Ok
    );
    // Ask the length with a zero-capacity buffer.
    let mut len = 0usize;
    assert_eq!(
        unsafe { doodle_string_bytes(inst, h, ptr::null_mut(), 0, &mut len) },
        DoodleStatus::ErrBufferTooSmall
    );
    assert_eq!(len, text.len());
    // Fill it.
    let mut buf = vec![0u8; len];
    assert_eq!(
        unsafe { doodle_string_bytes(inst, h, buf.as_mut_ptr(), buf.len(), &mut len) },
        DoodleStatus::Ok
    );
    assert_eq!(&buf, text.as_bytes());
    unsafe { doodle_release(inst, h) };
    unsafe { doodle_free(inst) };
}

#[test]
fn make_string_rejects_invalid_utf8() {
    let inst = load("1\n");
    let bytes = [0xFF, 0xFE, 0x00];
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_make_string(inst, bytes.as_ptr(), bytes.len(), &mut h) },
        DoodleStatus::ErrInvalidUtf8
    );
    unsafe { doodle_free(inst) };
}

#[test]
fn a_bignum_reads_as_decimal_not_as_int() {
    let inst = load("1\n");
    let big = "170141183460469231731687303715884105727"; // > i64::MAX
    let mut h = 0u64;
    assert_eq!(
        unsafe {
            doodle_capi::value::doodle_make_int_decimal(inst, big.as_ptr(), big.len(), &mut h)
        },
        DoodleStatus::Ok
    );
    let mut n = 0i64;
    assert_eq!(
        unsafe { doodle_as_int(inst, h, &mut n) },
        DoodleStatus::ErrIntOutOfRange
    );
    let mut len = 0usize;
    let _ = unsafe { doodle_as_int_decimal(inst, h, ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len];
    assert_eq!(
        unsafe { doodle_as_int_decimal(inst, h, buf.as_mut_ptr(), buf.len(), &mut len) },
        DoodleStatus::Ok
    );
    assert_eq!(std::str::from_utf8(&buf).unwrap(), big);
    unsafe { doodle_release(inst, h) };
    unsafe { doodle_free(inst) };
}

#[test]
fn a_null_instance_is_a_defined_error_not_a_crash() {
    let mut n = 0i64;
    assert_eq!(
        unsafe { doodle_as_int(ptr::null(), 0, &mut n) },
        DoodleStatus::ErrNullPointer
    );
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(ptr::null_mut(), DoodleDirective::RunToCompletion, &mut out) },
        DoodleStatus::ErrNullPointer
    );
    // A NULL free is a no-op (no crash).
    unsafe { doodle_free(ptr::null_mut()) };
}

#[test]
fn output_is_empty_without_a_print_capability() {
    // M7.1 registers no host capabilities, so a program cannot print — output is empty. (The
    // accessor is exercised for real once `print` can be registered, M7.2.)
    let inst = load("let a = 1\n");
    drive(inst);
    let mut len = 42usize;
    assert_eq!(
        unsafe { doodle_output(inst, ptr::null_mut(), 0, &mut len) },
        DoodleStatus::Ok
    );
    assert_eq!(len, 0);
    unsafe { doodle_free(inst) };
}
