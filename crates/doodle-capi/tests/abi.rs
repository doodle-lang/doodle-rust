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
    DOODLE_NULL_HANDLE, DoodleBlockOutcome, DoodleBodyKind, DoodleDirective, DoodleFault,
    DoodleKind, DoodleOutcome, DoodleOutcomeKind, DoodleStatus,
};
use doodle_capi::call::{
    DoodleCallCtx, DoodleForeignFn, doodle_call_arg, doodle_call_block, doodle_call_set_raise,
    doodle_call_set_result,
};
use doodle_capi::call_read::{doodle_call_as_int, doodle_call_release};
use doodle_capi::call_value::{doodle_call_make_int, doodle_call_make_string};
use doodle_capi::config::{doodle_config_free, doodle_config_new, doodle_config_set_limits};
use doodle_capi::desc::{
    DoodleForeignDesc, doodle_foreign_desc_block_param, doodle_foreign_desc_default_string,
    doodle_foreign_desc_new, doodle_foreign_desc_param, doodle_foreign_desc_set_callback,
};
use doodle_capi::instance::{
    DoodleInstance, doodle_capability_arg, doodle_drive, doodle_free, doodle_import_path_segment,
    doodle_load, doodle_load_with_registry, doodle_output, doodle_raised_kind, doodle_resolve,
    doodle_resolve_import, doodle_resolve_import_not_found,
};
use doodle_capi::registry::{
    DoodleBuiltin, doodle_registry_add_builtin, doodle_registry_add_foreign, doodle_registry_new,
};
use doodle_capi::value::{
    doodle_as_bool, doodle_as_int, doodle_as_int_decimal, doodle_foreign_ptr, doodle_foreign_tag,
    doodle_kind_of, doodle_make_foreign, doodle_make_int, doodle_make_nil, doodle_make_string,
    doodle_release, doodle_string_bytes,
};
use std::ffi::c_void;
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

// ---- M7.2b: host foreign functions through the C ABI ---------------------------------------

/// A host `to` `greet(who="world", body)`: reads its one bound argument as a handle and invokes
/// the block with it, then frees the arg handle. The accept criterion — a default + a block, both
/// binding per L§8.3, registered and driven entirely across the C ABI.
extern "C" fn greet_cb(ctx: *mut DoodleCallCtx, _user: *mut c_void) -> DoodleStatus {
    let mut who = DOODLE_NULL_HANDLE;
    let status = unsafe { doodle_call_arg(ctx, 0, &mut who) };
    if status != DoodleStatus::Ok {
        return status;
    }
    let mut outcome = DoodleBlockOutcome::Completed;
    let status = unsafe { doodle_call_block(ctx, &who, 1, &mut outcome) };
    let _ = unsafe { doodle_call_release(ctx, who) };
    status
}

/// A host `fn` `add(a, b)`: reads both integer args, constructs their sum through the ctx, and
/// sets it as the result (which consumes that handle); frees the arg handles.
extern "C" fn add_cb(ctx: *mut DoodleCallCtx, _user: *mut c_void) -> DoodleStatus {
    let (mut ha, mut hb) = (DOODLE_NULL_HANDLE, DOODLE_NULL_HANDLE);
    if unsafe { doodle_call_arg(ctx, 0, &mut ha) } != DoodleStatus::Ok
        || unsafe { doodle_call_arg(ctx, 1, &mut hb) } != DoodleStatus::Ok
    {
        return DoodleStatus::ErrContract;
    }
    let (mut a, mut b) = (0i64, 0i64);
    if unsafe { doodle_call_as_int(ctx, ha, &mut a) } != DoodleStatus::Ok
        || unsafe { doodle_call_as_int(ctx, hb, &mut b) } != DoodleStatus::Ok
    {
        return DoodleStatus::ErrWrongKind;
    }
    let mut sum = DOODLE_NULL_HANDLE;
    if unsafe { doodle_call_make_int(ctx, a + b, &mut sum) } != DoodleStatus::Ok {
        return DoodleStatus::ErrContract;
    }
    let status = unsafe { doodle_call_set_result(ctx, sum) }; // consumes `sum`
    let _ = unsafe { doodle_call_release(ctx, ha) };
    let _ = unsafe { doodle_call_release(ctx, hb) };
    status
}

/// A host `to` `boom()`: constructs a string through the ctx and raises it at the call site.
extern "C" fn boom_cb(ctx: *mut DoodleCallCtx, _user: *mut c_void) -> DoodleStatus {
    let msg = b"boom";
    let mut handle = DOODLE_NULL_HANDLE;
    let status = unsafe { doodle_call_make_string(ctx, msg.as_ptr(), msg.len(), &mut handle) };
    if status != DoodleStatus::Ok {
        return status;
    }
    unsafe { doodle_call_set_raise(ctx, handle) } // consumes `handle`
}

/// Registers `print` + a foreign function (named `name` of `kind`, its parameters built by
/// `build`, its body `callback`), loads and drives `source`, asserts it completed, and returns
/// the captured output.
fn run_with_foreign(
    source: &str,
    name: &[u8],
    kind: DoodleBodyKind,
    build: impl FnOnce(*mut DoodleForeignDesc),
    callback: DoodleForeignFn,
) -> Vec<u8> {
    let registry = doodle_registry_new();
    assert_eq!(
        unsafe { doodle_registry_add_builtin(registry, DoodleBuiltin::Print) },
        DoodleStatus::Ok
    );
    let desc = unsafe { doodle_foreign_desc_new(name.as_ptr(), name.len(), kind) };
    assert!(!desc.is_null());
    build(desc);
    assert_eq!(
        unsafe { doodle_foreign_desc_set_callback(desc, callback, ptr::null_mut()) },
        DoodleStatus::Ok
    );
    assert_eq!(
        unsafe { doodle_registry_add_foreign(registry, desc) },
        DoodleStatus::Ok
    );
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    assert_eq!(
        unsafe {
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
        },
        DoodleStatus::Ok
    );
    let outcome = drive(inst);
    assert_eq!(outcome.kind, DoodleOutcomeKind::Completed, "{outcome:?}");
    let out = read_output(inst);
    unsafe { doodle_free(inst) };
    out
}

#[test]
fn a_host_foreign_to_with_default_and_block_binds_and_invokes_through_c() {
    // The M7 accept criterion, end-to-end across the C ABI: an omitted argument binds the
    // descriptor's string default ("world"); a passed argument overrides it ("moon"); the block
    // is invoked reentrantly with the bound value each time.
    let out = run_with_foreign(
        "greet() do (who)\nprint(who)\nend\ngreet(\"moon\") do (who)\nprint(who)\nend\n",
        b"greet",
        DoodleBodyKind::Proc,
        |desc| {
            let (who, world, body) = (b"who".as_slice(), b"world".as_slice(), b"body".as_slice());
            assert_eq!(
                unsafe {
                    doodle_foreign_desc_default_string(
                        desc,
                        who.as_ptr(),
                        who.len(),
                        world.as_ptr(),
                        world.len(),
                    )
                },
                DoodleStatus::Ok
            );
            assert_eq!(
                unsafe { doodle_foreign_desc_block_param(desc, body.as_ptr(), body.len()) },
                DoodleStatus::Ok
            );
        },
        greet_cb,
    );
    assert_eq!(out, b"world\nmoon\n");
}

#[test]
fn a_host_foreign_fn_reads_args_and_returns_a_constructed_result_through_c() {
    // A `fn` foreign function that reads its integer args through the ctx value API and returns a
    // freshly-constructed sum: add(2, 3) => 5.
    let out = run_with_foreign(
        "print(add(2, 3))\n",
        b"add",
        DoodleBodyKind::Func,
        |desc| {
            let (a, b) = (b"a".as_slice(), b"b".as_slice());
            assert_eq!(
                unsafe { doodle_foreign_desc_param(desc, a.as_ptr(), a.len()) },
                DoodleStatus::Ok
            );
            assert_eq!(
                unsafe { doodle_foreign_desc_param(desc, b.as_ptr(), b.len()) },
                DoodleStatus::Ok
            );
        },
        add_cb,
    );
    assert_eq!(out, b"5\n");
}

#[test]
fn a_host_foreign_callback_raises_a_constructed_value_through_c() {
    // `boom()` raises the string "boom" it builds through the ctx; a `try`/`rescue` binds `e` to
    // that exact value and prints it — proving the raise path and in-callback construction.
    let out = run_with_foreign(
        "try\nboom()\nrescue e\nprint(e)\nend\n",
        b"boom",
        DoodleBodyKind::Proc,
        |_desc| {},
        boom_cb,
    );
    assert_eq!(out, b"boom\n");
}
