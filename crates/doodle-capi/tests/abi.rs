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
    DoodleInstance, doodle_capability_arg, doodle_drive, doodle_free, doodle_import_path_segment,
    doodle_load, doodle_load_with_registry, doodle_output, doodle_raised_kind, doodle_resolve,
    doodle_resolve_import, doodle_resolve_import_not_found,
};
use doodle_capi::registry::{DoodleBuiltin, doodle_registry_add_builtin, doodle_registry_new};
use doodle_capi::value::{
    doodle_as_bool, doodle_as_int, doodle_as_int_decimal, doodle_foreign_ptr, doodle_foreign_tag,
    doodle_kind_of, doodle_make_foreign, doodle_make_int, doodle_make_nil, doodle_make_string,
    doodle_release, doodle_string_bytes,
};
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

// Records the most recent foreign-value finalizer call (only the foreign-value test uses
// these, so no cross-test race). `extern "C"` — the exact type a C host supplies.
static FINALIZED_COUNT: AtomicU32 = AtomicU32::new(0);
static FINALIZED_PTR: AtomicU64 = AtomicU64::new(0);
extern "C" fn record_finalizer(ptr: u64) {
    FINALIZED_PTR.store(ptr, Relaxed);
    FINALIZED_COUNT.fetch_add(1, Relaxed);
}

/// Loads `source` with a registry of `builtins` (in order), asserting success.
fn load_with(source: &str, builtins: &[DoodleBuiltin]) -> *mut DoodleInstance {
    let registry = doodle_registry_new();
    for &b in builtins {
        assert_eq!(
            unsafe { doodle_registry_add_builtin(registry, b) },
            DoodleStatus::Ok
        );
    }
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    let status = unsafe {
        doodle_load_with_registry(
            source.as_ptr(),
            source.len(),
            ptr::null(),
            registry,
            &mut inst,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, DoodleStatus::Ok);
    inst
}

/// Copies out the instance's full output.
fn read_output(inst: *const DoodleInstance) -> Vec<u8> {
    let mut len = 0usize;
    let _ = unsafe { doodle_output(inst, ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len];
    assert_eq!(
        unsafe { doodle_output(inst, buf.as_mut_ptr(), buf.len(), &mut len) },
        DoodleStatus::Ok
    );
    buf
}

/// Makes a `nil` handle for resolving a `to` capability.
fn make_nil(inst: *mut DoodleInstance) -> u64 {
    let mut h = 0u64;
    assert_eq!(unsafe { doodle_make_nil(inst, &mut h) }, DoodleStatus::Ok);
    h
}

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
fn a_registered_print_writes_output() {
    let inst = load_with("print(1 + 2)\n", &[DoodleBuiltin::Print]);
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Completed);
    assert_eq!(read_output(inst), b"3\n");
    unsafe { doodle_free(inst) };
}

#[test]
fn a_capability_suspends_exposes_its_args_and_resolves() {
    // `draw_line` is a `to` capability taking 8 coordinate/colour arguments.
    let inst = load_with(
        "draw_line(1, 2, 3, 4, 5, 6, 7, 8)\n",
        &[DoodleBuiltin::DrawLine],
    );
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Suspended);
    assert_eq!(out.request_count, 8);
    // Read the first and last bound argument (host-owned handles; release after reading).
    for (index, expected) in [(0u32, 1i64), (7, 8)] {
        let mut h = 0u64;
        assert_eq!(
            unsafe { doodle_capability_arg(inst, index, &mut h) },
            DoodleStatus::Ok
        );
        let mut n = 0i64;
        assert_eq!(unsafe { doodle_as_int(inst, h, &mut n) }, DoodleStatus::Ok);
        assert_eq!(n, expected);
        unsafe { doodle_release(inst, h) };
    }
    // Out of range.
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_capability_arg(inst, 8, &mut h) },
        DoodleStatus::ErrIndexOutOfBounds
    );
    // A `to` capability yields Void: resolve with nil → the program completes.
    let nil = make_nil(inst);
    let mut done = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, nil, &mut done) },
        DoodleStatus::Ok
    );
    assert_eq!(done.kind, DoodleOutcomeKind::Completed);
    unsafe { doodle_release(inst, nil) };
    unsafe { doodle_free(inst) };
}

#[test]
fn a_resolved_fn_capability_value_flows_into_the_program() {
    // `print` (registered first) + `read_line` (a `fn` capability): the resolved value is
    // printed. Registration order is replay identity (§11): print=0, read_line=1.
    let inst = load_with(
        "print(read_line())\n",
        &[DoodleBuiltin::Print, DoodleBuiltin::ReadLine],
    );
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Suspended);
    assert_eq!(out.request_count, 0, "read_line takes no arguments");
    // Resolve with a string; the program prints it.
    let text = "world";
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_make_string(inst, text.as_ptr(), text.len(), &mut h) },
        DoodleStatus::Ok
    );
    let mut done = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, h, &mut done) },
        DoodleStatus::Ok
    );
    assert_eq!(done.kind, DoodleOutcomeKind::Completed);
    unsafe { doodle_release(inst, h) };
    assert_eq!(read_output(inst), b"world\n");
    unsafe { doodle_free(inst) };
}

#[test]
fn resolving_a_non_suspended_instance_is_a_contract_error() {
    let inst = load_with("print(1)\n", &[DoodleBuiltin::Print]);
    // Nothing has suspended yet.
    let nil = make_nil(inst);
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, nil, &mut out) },
        DoodleStatus::ErrContract
    );
    unsafe { doodle_release(inst, nil) };
    unsafe { doodle_free(inst) };
}

#[test]
fn a_foreign_value_round_trips_and_finalizes_once_on_free() {
    FINALIZED_COUNT.store(0, Relaxed);
    FINALIZED_PTR.store(0, Relaxed);
    let inst = load("1\n");
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_make_foreign(inst, 7, 42, Some(record_finalizer), &mut h) },
        DoodleStatus::Ok
    );
    // Opaque to Doodle, recognized by tag + ptr.
    let mut kind = DoodleKind::Nil;
    assert_eq!(
        unsafe { doodle_kind_of(inst, h, &mut kind) },
        DoodleStatus::Ok
    );
    assert_eq!(kind, DoodleKind::Foreign);
    let mut tag = 0u64;
    assert_eq!(
        unsafe { doodle_foreign_tag(inst, h, &mut tag) },
        DoodleStatus::Ok
    );
    assert_eq!(tag, 7);
    let mut p = 0u64;
    assert_eq!(
        unsafe { doodle_foreign_ptr(inst, h, &mut p) },
        DoodleStatus::Ok
    );
    assert_eq!(p, 42);
    // A non-foreign reader on it is a wrong-kind error.
    let mut n = 0i64;
    assert_eq!(
        unsafe { doodle_as_int(inst, h, &mut n) },
        DoodleStatus::ErrWrongKind
    );
    // The handle keeps it live; freeing the instance finalizes it exactly once with its ptr.
    assert_eq!(FINALIZED_COUNT.load(Relaxed), 0, "not finalized while live");
    unsafe { doodle_free(inst) };
    assert_eq!(FINALIZED_COUNT.load(Relaxed), 1, "finalized once at free");
    assert_eq!(
        FINALIZED_PTR.load(Relaxed),
        42,
        "finalizer received the ptr"
    );
}

#[test]
fn an_import_suspends_exposes_its_path_and_resolves_with_source() {
    let inst = load("import shapes\n");
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::SuspendedImport);
    assert_eq!(out.request_count, 1);
    // Read the (single) path segment.
    let mut len = 0usize;
    let _ = unsafe { doodle_import_path_segment(inst, 0, ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len];
    assert_eq!(
        unsafe { doodle_import_path_segment(inst, 0, buf.as_mut_ptr(), buf.len(), &mut len) },
        DoodleStatus::Ok
    );
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "shapes");
    // Resolve with a trivial module source: the module loads and the program completes.
    let module_src = "let x = 1\n";
    let canonical = "shapes";
    let mut done = DoodleOutcome::blank();
    assert_eq!(
        unsafe {
            doodle_resolve_import(
                inst,
                module_src.as_ptr(),
                module_src.len(),
                canonical.as_ptr(),
                canonical.len(),
                &mut done,
            )
        },
        DoodleStatus::Ok
    );
    assert_eq!(done.kind, DoodleOutcomeKind::Completed);
    unsafe { doodle_free(inst) };
}

#[test]
fn an_unresolvable_import_raises_module_not_found() {
    let inst = load("import missing\n");
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::SuspendedImport);
    let mut done = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve_import_not_found(inst, &mut done) },
        DoodleStatus::Ok
    );
    assert_eq!(done.kind, DoodleOutcomeKind::Raised);
    // The described kind is `module-not-found`.
    let mut len = 0usize;
    let _ = unsafe { doodle_raised_kind(inst, ptr::null_mut(), 0, &mut len) };
    let mut buf = vec![0u8; len];
    let _ = unsafe { doodle_raised_kind(inst, buf.as_mut_ptr(), buf.len(), &mut len) };
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "module-not-found");
    unsafe { doodle_free(inst) };
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
