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

// `doodle-core` is a dependency of the `doodle-capi` library but this test crate reaches it only
// through re-exports; naming it here marks it used so `cargo miri test`'s
// `unused-crate-dependencies` force-warn stays quiet (a no-op for ordinary builds).
use doodle_core as _;

use doodle_capi::abi::{
    DOODLE_NULL_HANDLE, DoodleBlockOutcome, DoodleBodyKind, DoodleDirective, DoodleFault,
    DoodleHandle, DoodleKind, DoodleOutcome, DoodleOutcomeKind, DoodleStatus,
};
use doodle_capi::abi::{
    DoodleAuxOutcome, DoodleAuxOutcomeKind, DoodleBreakpoint, DoodleDiagnostic, DoodleFrame,
    DoodleGlobal, DoodleGlobalKind, DoodleObservationMode, DoodlePauseReason, DoodlePosition,
    DoodleSeverity,
};
use doodle_capi::call::{
    DoodleCallCtx, DoodleForeignFn, doodle_call_arg, doodle_call_arg_count, doodle_call_block,
    doodle_call_set_raise, doodle_call_set_result,
};
use doodle_capi::call_read::{doodle_call_as_int, doodle_call_release};
use doodle_capi::call_value::{doodle_call_make_int, doodle_call_make_string};
use doodle_capi::config::{
    doodle_config_free, doodle_config_new, doodle_config_set_limits,
    doodle_config_set_target_unicode,
};
use doodle_capi::control::{doodle_control, doodle_control_cancel, doodle_control_free};
use doodle_capi::desc::{
    DoodleForeignDesc, doodle_foreign_desc_block_param, doodle_foreign_desc_default_string,
    doodle_foreign_desc_new, doodle_foreign_desc_param, doodle_foreign_desc_set_callback,
};
use doodle_capi::inspect::{
    doodle_callable_name, doodle_dict_key, doodle_dict_length, doodle_dict_value,
    doodle_eval_to_string, doodle_list_get, doodle_list_length, doodle_record_field,
    doodle_record_field_name, doodle_record_length, doodle_record_type_name,
};
use doodle_capi::instance::{
    DoodleInstance, doodle_capability_arg, doodle_drive, doodle_drive_slice, doodle_free,
    doodle_import_path_segment, doodle_load, doodle_load_with_registry, doodle_output,
    doodle_raised_kind, doodle_resolve, doodle_resolve_import, doodle_resolve_import_not_found,
};
use doodle_capi::observe::{
    doodle_breakpoint_at, doodle_breakpoint_canonical_id, doodle_breakpoint_count,
    doodle_clear_breakpoint, doodle_current_position, doodle_current_result, doodle_diagnostic_at,
    doodle_diagnostic_count, doodle_frame_at, doodle_frame_callable, doodle_frame_dynamic_count,
    doodle_frame_local_count, doodle_frame_local_name, doodle_frame_local_value,
    doodle_module_canonical_id, doodle_module_global, doodle_module_global_count,
    doodle_module_global_name, doodle_module_global_value, doodle_observation_mode, doodle_pause,
    doodle_raise_trapping, doodle_raised_trace_frame_at, doodle_raised_trace_frame_callable,
    doodle_raised_trace_frame_count, doodle_raised_trace_tail_at, doodle_raised_trace_tail_count,
    doodle_set_breakpoint, doodle_set_observation_mode, doodle_set_raise_trapping,
    doodle_stack_frame_count, doodle_tail_history_count, doodle_trapped_raise,
    doodle_trapped_raise_position,
};
use doodle_capi::registry::{
    DoodleBuiltin, doodle_registry_add_builtin, doodle_registry_add_foreign, doodle_registry_free,
    doodle_registry_new,
};
use doodle_capi::value::{
    doodle_as_bool, doodle_as_int, doodle_as_int_decimal, doodle_foreign_ptr, doodle_foreign_tag,
    doodle_kind_of, doodle_make_foreign, doodle_make_int, doodle_make_nil, doodle_make_string,
    doodle_release, doodle_retain, doodle_string_bytes,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering::Relaxed};

// Records the most recent foreign-value finalizer call (only the foreign-value test uses
// these, so no cross-test race). `extern "C"` — the exact type a C host supplies.
static FINALIZED_COUNT: AtomicU32 = AtomicU32::new(0);
static FINALIZED_PTR: AtomicU64 = AtomicU64::new(0);
extern "C" fn record_finalizer(ptr: u64) {
    FINALIZED_PTR.store(ptr, Relaxed);
    FINALIZED_COUNT.fetch_add(1, Relaxed);
}

// R4 (NULL-out leak): a separate finalizer + counter, so only this test writes them.
static NULLOUT_FINALIZED: AtomicU32 = AtomicU32::new(0);
extern "C" fn nullout_finalizer(_ptr: u64) {
    NULLOUT_FINALIZED.fetch_add(1, Relaxed);
}

/// Loads `source` with a registry of `builtins` (in order), asserting success.
fn load_with(source: &str, builtins: &[DoodleBuiltin]) -> *mut DoodleInstance {
    let registry = doodle_registry_new();
    for &b in builtins {
        assert_eq!(
            unsafe { doodle_registry_add_builtin(registry, b as u32) },
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
    let status = unsafe { doodle_drive(inst, DoodleDirective::RunToCompletion as u32, &mut out) };
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
fn a_terminal_raise_exposes_the_exception_value_for_structural_inspection() {
    // R6: after an uncaught raise, DoodleOutcome.value is a host-owned handle to the exception
    // value (E§3.3/§8.4): the same value `rescue e` binds, for structural inspection alongside
    // the described kind/message. Here the raised value is a string; in general it's an Error
    // record whose `details` dict the host reads by handle.
    let inst = load("raise \"boom\"\n");
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Raised);
    assert_ne!(
        out.value, DOODLE_NULL_HANDLE,
        "the exception handle is populated"
    );
    let mut kind = DoodleKind::Nil;
    assert_eq!(
        unsafe { doodle_kind_of(inst, out.value, &mut kind) },
        DoodleStatus::Ok
    );
    assert_eq!(kind, DoodleKind::String);
    let mut buf = [0u8; 8];
    let mut len = 0usize;
    assert_eq!(
        unsafe { doodle_string_bytes(inst, out.value, buf.as_mut_ptr(), buf.len(), &mut len) },
        DoodleStatus::Ok
    );
    assert_eq!(&buf[..len], b"boom");
    // The handle is host-owned — release it.
    assert_eq!(unsafe { doodle_release(inst, out.value) }, DoodleStatus::Ok);
    unsafe { doodle_free(inst) };
}

#[test]
fn doodle_retain_shares_ownership_and_does_not_resurrect() {
    // Freeze decision (E§4.2 refcounted model): `doodle_retain` adds a reference and returns the
    // same handle; a handle stays live until released as many times as obtained (mint + retain),
    // and retain-after-final-release does NOT resurrect it (a contract failure → null handle).
    let inst = load("raise \"boom\"\n");
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Raised);
    let h = out.value; // one reference: the mint
    assert_ne!(h, DOODLE_NULL_HANDLE);

    // Retain returns the same handle (chainable), now 2 references.
    assert_eq!(unsafe { doodle_retain(inst, h) }, h);

    // One release drops to 1 reference; the handle is still live (readable).
    assert_eq!(unsafe { doodle_release(inst, h) }, DoodleStatus::Ok);
    let mut kind = DoodleKind::Nil;
    assert_eq!(
        unsafe { doodle_kind_of(inst, h, &mut kind) },
        DoodleStatus::Ok
    );
    assert_eq!(kind, DoodleKind::String);

    // The second release drops to 0 — freed. A third release is a stale-handle error (S-35),
    // and using the handle after the final release is a stale error, never a read of freed memory.
    assert_eq!(unsafe { doodle_release(inst, h) }, DoodleStatus::Ok);
    assert_eq!(
        unsafe { doodle_release(inst, h) },
        DoodleStatus::ErrStaleHandle
    );
    assert_eq!(
        unsafe { doodle_kind_of(inst, h, &mut kind) },
        DoodleStatus::ErrStaleHandle
    );
    // Retain-after-release does not resurrect: it returns the null handle.
    assert_eq!(unsafe { doodle_retain(inst, h) }, DOODLE_NULL_HANDLE);

    unsafe { doodle_free(inst) };
}

#[test]
fn a_bad_target_unicode_is_reported_even_with_a_parse_error() {
    // MINOR fix (S-41): the pure config field is validated BEFORE parse/resolve, so a mismatched
    // target Unicode version surfaces as `ErrUnsupportedUnicode` rather than being masked by a
    // co-occurring parse error (which would otherwise return `ErrLoad`).
    let config = doodle_config_new();
    // A major of 99 is not the engine's pinned Unicode version.
    unsafe { doodle_config_set_target_unicode(config, 99, 0, 0) };
    let src = "print(\n"; // also a parse error
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
    assert_eq!(
        status,
        DoodleStatus::ErrUnsupportedUnicode,
        "the config mismatch is reported, not masked by the parse error"
    );
    assert!(inst.is_null(), "no instance on a failed load");
    unsafe { doodle_config_free(config) };
}

/// Reads a callable handle's declared name via `doodle_callable_name` (copy-out), or `None`.
fn callable_name_of(inst: *mut DoodleInstance, handle: DoodleHandle) -> Option<String> {
    let mut has = false;
    let mut buf = [0u8; 64];
    let mut len = 0usize;
    assert_eq!(
        unsafe {
            doodle_callable_name(
                inst,
                handle,
                &mut has,
                buf.as_mut_ptr(),
                buf.len(),
                &mut len,
            )
        },
        DoodleStatus::Ok
    );
    has.then(|| String::from_utf8(buf[..len].to_vec()).unwrap())
}

#[test]
fn a_terminal_raise_exposes_its_trace_for_post_mortem_reading() {
    // R6 Part B: the retained trace (E§9) is readable post-mortem through the C surface — the
    // live frames and the tail-elided history, each carrying its callable. `countdown` tail-calls
    // itself then raises; the trace shows a live `countdown` frame and an elided one, and every
    // callable it mints is live (rooted by the retained trace) after the stack has unwound.
    let inst = load(
        "to countdown(n)\n\
         if n == 0 then\n\
         1 + true\n\
         else\n\
         countdown(n - 1)\n\
         end\n\
         end\n\
         countdown(5)\n",
    );
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Raised);

    let mut frames = 0u32;
    assert_eq!(
        unsafe { doodle_raised_trace_frame_count(inst, &mut frames) },
        DoodleStatus::Ok
    );
    assert!(frames > 0, "the retained trace has live frames");

    let mut saw_countdown = false;
    for i in 0..frames {
        let mut frame = zero_frame();
        assert_eq!(
            unsafe { doodle_raised_trace_frame_at(inst, i, &mut frame) },
            DoodleStatus::Ok
        );
        if frame.has_callable {
            let mut handle = DOODLE_NULL_HANDLE;
            assert_eq!(
                unsafe { doodle_raised_trace_frame_callable(inst, i, &mut handle) },
                DoodleStatus::Ok
            );
            assert_ne!(handle, DOODLE_NULL_HANDLE);
            if callable_name_of(inst, handle).as_deref() == Some("countdown") {
                saw_countdown = true;
            }
            assert_eq!(unsafe { doodle_release(inst, handle) }, DoodleStatus::Ok);
        }
    }
    assert!(
        saw_countdown,
        "a live `countdown` frame survives in the trace"
    );

    // Out-of-range frame index → ErrIndexOutOfBounds (no pause generation to gate a trace).
    let mut frame = zero_frame();
    assert_eq!(
        unsafe { doodle_raised_trace_frame_at(inst, frames, &mut frame) },
        DoodleStatus::ErrIndexOutOfBounds
    );

    // The tail-elided history (countdown tail-called itself) is captured, its callable live.
    let mut tail = 0u32;
    assert_eq!(
        unsafe { doodle_raised_trace_tail_count(inst, &mut tail) },
        DoodleStatus::Ok
    );
    assert!(tail > 0, "the tail-elided history is captured");
    let mut handle = DOODLE_NULL_HANDLE;
    let mut pos = zero_pos();
    assert_eq!(
        unsafe { doodle_raised_trace_tail_at(inst, 0, &mut handle, &mut pos) },
        DoodleStatus::Ok
    );
    assert_eq!(callable_name_of(inst, handle).as_deref(), Some("countdown"));
    assert_eq!(unsafe { doodle_release(inst, handle) }, DoodleStatus::Ok);

    // A clean completion retains no trace: the frame count is 0.
    let clean = load("1 + 1\n");
    let _ = drive(clean);
    let mut n = 99u32;
    assert_eq!(
        unsafe { doodle_raised_trace_frame_count(clean, &mut n) },
        DoodleStatus::Ok
    );
    assert_eq!(n, 0, "a non-raised drive retains no trace");
    unsafe { doodle_free(clean) };
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

/// A handle minted by one instance and used on another is caught as a *contract* error in
/// debug builds (MD §16) — a host bug the embedder should fix, distinct from the
/// `ErrStaleHandle` a released/forged handle yields (which a host may routinely retry).
/// Release builds do not encode the per-instance id and cannot detect the collision, so this
/// asserts only under `debug_assertions` — where the M7 exit criterion places the guard and
/// where the sanitizer/Miri gates run. The deterministic guarantee lives in the engine unit
/// test `handle::tests::a_handle_from_another_instance_is_caught_as_foreign` (which pins the
/// ids); this end-to-end test drives it through the real C ABI.
#[cfg(debug_assertions)]
#[test]
fn a_handle_from_another_instance_is_a_contract_error() {
    let a = load("let a = 1\n");
    let b = load("let b = 1\n");
    // Neither instance has been driven, so each `make_int` is its table's first host handle:
    // `ha` and `hb` collide in index+generation (slot 0, generation 1) and differ only in the
    // debug instance-id. This is the exact look-alike a value-keyed minted-set could not catch.
    let mut ha = 0u64;
    assert_eq!(unsafe { doodle_make_int(a, 10, &mut ha) }, DoodleStatus::Ok);
    let mut hb = 0u64;
    assert_eq!(unsafe { doodle_make_int(b, 20, &mut hb) }, DoodleStatus::Ok);

    // `b` reads its own handle fine.
    let mut n = 0i64;
    assert_eq!(unsafe { doodle_as_int(b, hb, &mut n) }, DoodleStatus::Ok);
    assert_eq!(n, 20);

    // `a`'s look-alike handle on `b` is a cross-instance contract violation, not a stale handle,
    // and `b`'s own value is untouched.
    assert_eq!(
        unsafe { doodle_as_int(b, ha, &mut n) },
        DoodleStatus::ErrContract
    );
    assert_eq!(n, 20, "the rejected read must not overwrite the out-param");
    assert_eq!(
        unsafe { doodle_release(b, ha) },
        DoodleStatus::ErrContract,
        "releasing a foreign handle is a contract error, not a stale-handle no-op"
    );

    // `a` still owns its handle after `b` rejected it.
    assert_eq!(unsafe { doodle_as_int(a, ha, &mut n) }, DoodleStatus::Ok);
    assert_eq!(n, 10);

    unsafe { doodle_free(a) };
    unsafe { doodle_free(b) };
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
        unsafe {
            doodle_drive(
                ptr::null_mut(),
                DoodleDirective::RunToCompletion as u32,
                &mut out,
            )
        },
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
fn an_invalid_call_is_rejected_as_contract_and_changes_nothing() {
    // E§7.5: an invalid call (a stale resolution handle, a re-drive of a terminal instance, a
    // resolve at the wrong time) is rejected as `ErrContract`, changing nothing; the host retries.
    // Under Miri this also exercises the use-after-release handle path on resolve.
    let inst = load_with(
        "draw_line(1, 2, 3, 4, 5, 6, 7, 8)\n",
        &[DoodleBuiltin::DrawLine],
    );
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Suspended);

    // Resolve with a RELEASED (stale) handle: the handle is validated before the suspension is
    // consumed, so the call is rejected and the suspension is left intact.
    let stale = make_nil(inst);
    assert_eq!(unsafe { doodle_release(inst, stale) }, DoodleStatus::Ok);
    let mut rejected = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, stale, &mut rejected) },
        DoodleStatus::ErrContract
    );
    // The instance is unchanged, so a fresh valid resolve still completes the program.
    let nil = make_nil(inst);
    let mut done = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, nil, &mut done) },
        DoodleStatus::Ok
    );
    assert_eq!(done.kind, DoodleOutcomeKind::Completed);
    unsafe { doodle_release(inst, nil) };

    // The instance is now terminal: re-driving it is rejected (not faulted).
    let mut redrive = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, DoodleDirective::RunToCompletion as u32, &mut redrive) },
        DoodleStatus::ErrContract
    );
    // Resolving a non-suspended instance is likewise rejected.
    let v = make_nil(inst);
    let mut r2 = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, v, &mut r2) },
        DoodleStatus::ErrContract
    );
    unsafe { doodle_release(inst, v) };
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
fn time_and_random_builtins_register_and_suspend() {
    // The M7.5e additive builtins `time`/`random` are suspending capabilities (S-19), registerable
    // by identity like `read_line`. Registration order is replay identity (§11): print=0, time=1,
    // random=2, so `time()` suspends as capability id 1 and its resolved value flows into `print`.
    let inst = load_with(
        "print(time())\n",
        &[
            DoodleBuiltin::Print,
            DoodleBuiltin::Time,
            DoodleBuiltin::Random,
        ],
    );
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Suspended);
    assert_eq!(out.capability, 1, "time is the second registered builtin");
    assert_eq!(out.request_count, 0, "time takes no arguments");
    let mut h = 0u64;
    assert_eq!(
        unsafe { doodle_make_int(inst, 42, &mut h) },
        DoodleStatus::Ok
    );
    let mut done = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_resolve(inst, h, &mut done) },
        DoodleStatus::Ok
    );
    assert_eq!(done.kind, DoodleOutcomeKind::Completed);
    unsafe { doodle_release(inst, h) };
    assert_eq!(read_output(inst), b"42\n");
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

// R2 regression: a foreign callback that reaches back through the *instance pointer* (rather than
// the ctx) must be refused with `ErrContract`, never form a second `&mut Instance` (UB). Only this
// test writes these statics, so there is no cross-test race.
static REENTER_INST: AtomicPtr<DoodleInstance> = AtomicPtr::new(ptr::null_mut());
static REENTER_STATUS: AtomicU32 = AtomicU32::new(u32::MAX);

/// A host `to reenter()`: while the drive holds `&mut Instance`, it tries an instance-pointer call
/// (`doodle_output`) — which would alias that borrow. The drive-scoped guard must turn it into a
/// defined `ErrContract` instead. Records the status and returns Ok so the drive completes.
extern "C" fn reenter_cb(_ctx: *mut DoodleCallCtx, _user: *mut c_void) -> DoodleStatus {
    let inst = REENTER_INST.load(Relaxed);
    let mut buf = [0u8; 8];
    let mut len = 0usize;
    let status = unsafe { doodle_output(inst, buf.as_mut_ptr(), buf.len(), &mut len) };
    REENTER_STATUS.store(status as u32, Relaxed);
    DoodleStatus::Ok
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
        unsafe { doodle_registry_add_builtin(registry, DoodleBuiltin::Print as u32) },
        DoodleStatus::Ok
    );
    let desc = unsafe { doodle_foreign_desc_new(name.as_ptr(), name.len(), kind as u32) };
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
fn an_out_of_range_enum_discriminant_is_a_contract_error_not_ub() {
    // R3: host-supplied enum params cross as u32 and are validated. An unknown value (version skew
    // against a newer header, or a host bug) is a defined error, never UB (freeze convention 5).
    let inst = load("1\n");
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, 999, &mut out) },
        DoodleStatus::ErrContract,
        "an unknown directive is ErrContract"
    );
    assert_eq!(
        unsafe { doodle_drive_slice(inst, 999, 10, &mut out) },
        DoodleStatus::ErrContract,
        "an unknown directive (slice) is ErrContract"
    );
    assert_eq!(
        unsafe { doodle_set_observation_mode(inst, 999) },
        DoodleStatus::ErrContract,
        "an unknown observation mode is ErrContract"
    );
    unsafe { doodle_free(inst) };

    // A foreign descriptor with an unknown body kind fails to NULL (its failure signal).
    let name = b"x";
    assert!(
        unsafe { doodle_foreign_desc_new(name.as_ptr(), name.len(), 999) }.is_null(),
        "an unknown body kind yields a NULL descriptor"
    );
    // An unknown built-in is ErrContract.
    let registry = doodle_registry_new();
    assert_eq!(
        unsafe { doodle_registry_add_builtin(registry, 999) },
        DoodleStatus::ErrContract,
        "an unknown built-in is ErrContract"
    );
    unsafe { doodle_registry_free(registry) };
}

#[test]
fn a_constructor_with_a_null_out_does_not_leak_a_handle() {
    // R4: a `make_*` given a NULL out-pointer must NOT mint — a mint that then failed to write
    // would orphan a host-owned handle, rooting its value for the instance's life. Witnessed via a
    // foreign value's finalizer: if `make_foreign` created the value despite the NULL out, that
    // value would finalize when the instance is freed. It must not run at all.
    NULLOUT_FINALIZED.store(0, Relaxed);
    let inst = load("1\n");
    assert_eq!(
        unsafe { doodle_make_foreign(inst, 1, 42, Some(nullout_finalizer), ptr::null_mut()) },
        DoodleStatus::ErrNullPointer,
        "a NULL out is ErrNullPointer"
    );
    // Also exercise a scalar and a heap constructor on the NULL-out path (they must be inert).
    assert_eq!(
        unsafe { doodle_make_int(inst, 7, ptr::null_mut()) },
        DoodleStatus::ErrNullPointer
    );
    assert_eq!(
        unsafe { doodle_make_string(inst, b"leak".as_ptr(), 4, ptr::null_mut()) },
        DoodleStatus::ErrNullPointer
    );
    unsafe { doodle_free(inst) };
    assert_eq!(
        NULLOUT_FINALIZED.load(Relaxed),
        0,
        "no foreign value was created, so nothing finalizes at destroy (no leaked handle)"
    );
}

#[test]
fn a_reentrant_instance_pointer_call_from_a_callback_is_a_contract_error_not_ub() {
    // During a foreign callback the engine holds `&mut Instance`; an instance-pointer entry called
    // from inside the callback would form a second, aliasing reference (UB). The drive-scoped guard
    // must turn it into a defined `ErrContract`. (Under Miri this is also the no-aliasing proof.)
    REENTER_STATUS.store(u32::MAX, Relaxed);
    let registry = doodle_registry_new();
    let name = b"reenter";
    let desc =
        unsafe { doodle_foreign_desc_new(name.as_ptr(), name.len(), DoodleBodyKind::Proc as u32) };
    assert!(!desc.is_null());
    assert_eq!(
        unsafe { doodle_foreign_desc_set_callback(desc, reenter_cb, ptr::null_mut()) },
        DoodleStatus::Ok
    );
    assert_eq!(
        unsafe { doodle_registry_add_foreign(registry, desc) },
        DoodleStatus::Ok
    );

    let source = "reenter()\n";
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
    // Publish the instance so the callback can *try* to use it mid-drive.
    REENTER_INST.store(inst, Relaxed);
    let outcome = drive(inst);
    assert_eq!(outcome.kind, DoodleOutcomeKind::Completed, "{outcome:?}");
    assert_eq!(
        REENTER_STATUS.load(Relaxed),
        DoodleStatus::ErrContract as u32,
        "a reentrant instance-pointer call must be a defined ErrContract, not UB"
    );
    REENTER_INST.store(ptr::null_mut(), Relaxed);
    unsafe { doodle_free(inst) };
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

// The stashed ancestor ctx + the status `inner` got trying to touch it (reentrant-aliasing test).
static STASHED_CTX: AtomicPtr<DoodleCallCtx> = AtomicPtr::new(ptr::null_mut());
static REENTRANT_STATUS: AtomicU32 = AtomicU32::new(u32::MAX);

/// `outer(body)`: stashes its own ctx (so a reentrant callback can try to touch this *ancestor*
/// activation), then invokes its block — which reenters foreign code.
extern "C" fn outer_cb(ctx: *mut DoodleCallCtx, _user: *mut c_void) -> DoodleStatus {
    STASHED_CTX.store(ctx, Relaxed);
    let mut outcome = DoodleBlockOutcome::Completed;
    unsafe { doodle_call_block(ctx, ptr::null(), 0, &mut outcome) }
}

/// `inner()`: while `outer`'s block is mid-drive, adversarially touches the STASHED ancestor ctx.
/// That ctx's `&mut IntrinsicCtx`/`&mut Machine` is reborrowed into this very drive, so honoring
/// the call would form a second, aliasing `&mut` (instantaneous UB). The engine must reject it.
extern "C" fn inner_cb(_ctx: *mut DoodleCallCtx, _user: *mut c_void) -> DoodleStatus {
    let stashed = STASHED_CTX.load(Relaxed);
    let mut count = 0u32;
    let status = unsafe { doodle_call_arg_count(stashed, &mut count) };
    REENTRANT_STATUS.store(status as u32, Relaxed);
    DoodleStatus::Ok
}

#[test]
fn touching_an_ancestor_ctx_from_a_reentrant_call_is_rejected_not_ub() {
    // Registers `outer(body)` + `inner()` and runs `outer() do inner() end`. `inner` runs inside
    // `outer`'s block invocation (a nested drive) and tries to use `outer`'s stashed ctx. The
    // innermost-ctx gate must make that a defined `ErrContract`, never two live `&mut` to the same
    // activation. (Under the old `live`-flag gate this returned `Ok` and was UB; Miri would trap.)
    STASHED_CTX.store(ptr::null_mut(), Relaxed);
    REENTRANT_STATUS.store(u32::MAX, Relaxed);
    let registry = doodle_registry_new();
    let outer =
        unsafe { doodle_foreign_desc_new(b"outer".as_ptr(), 5, DoodleBodyKind::Proc as u32) };
    assert!(!outer.is_null());
    assert_eq!(
        unsafe { doodle_foreign_desc_block_param(outer, b"body".as_ptr(), 4) },
        DoodleStatus::Ok
    );
    assert_eq!(
        unsafe { doodle_foreign_desc_set_callback(outer, outer_cb, ptr::null_mut()) },
        DoodleStatus::Ok
    );
    assert_eq!(
        unsafe { doodle_registry_add_foreign(registry, outer) },
        DoodleStatus::Ok
    );
    let inner =
        unsafe { doodle_foreign_desc_new(b"inner".as_ptr(), 5, DoodleBodyKind::Proc as u32) };
    assert!(!inner.is_null());
    assert_eq!(
        unsafe { doodle_foreign_desc_set_callback(inner, inner_cb, ptr::null_mut()) },
        DoodleStatus::Ok
    );
    assert_eq!(
        unsafe { doodle_registry_add_foreign(registry, inner) },
        DoodleStatus::Ok
    );
    let src = "outer() do\ninner()\nend\n";
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    assert_eq!(
        unsafe {
            doodle_load_with_registry(
                src.as_ptr(),
                src.len(),
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
    unsafe { doodle_free(inst) };
    assert_eq!(
        REENTRANT_STATUS.load(Relaxed),
        DoodleStatus::ErrContract as u32,
        "touching the ancestor ctx mid-drive must be rejected"
    );
}

// ---- M7.3a: observation surface (positions, stack walk, generation) ------------------------

fn zero_pos() -> DoodlePosition {
    DoodlePosition {
        span_start: 0,
        span_end: 0,
        module: 0,
        reserved: [0; 1],
    }
}

fn zero_frame() -> DoodleFrame {
    DoodleFrame {
        has_callable: false,
        has_call_site: false,
        call_site: zero_pos(),
        tail_count: 0,
        module: 0,
        reserved: [0; 4],
    }
}

/// Loads `source` and drives one `Step`, asserting it paused at a safe point; returns the instance.
fn load_and_pause(source: &str) -> *mut DoodleInstance {
    let inst = load(source);
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, DoodleDirective::Step as u32, &mut out) },
        DoodleStatus::Ok
    );
    assert_eq!(out.kind, DoodleOutcomeKind::Paused, "expected a Step pause");
    inst
}

#[test]
fn observation_stack_walk_positions_and_generation_staleness() {
    let inst = load_and_pause("let x = 1\nlet y = 2\n");
    // The stack has at least the module top-level frame; the walk hands back the generation token.
    let (mut count, mut walk_gen) = (0u32, 0u32);
    assert_eq!(
        unsafe { doodle_stack_frame_count(inst, &mut count, &mut walk_gen) },
        DoodleStatus::Ok
    );
    assert!(count >= 1, "at least the module-top frame");
    // The outermost frame is the module top: no callable value.
    let mut frame = zero_frame();
    assert_eq!(
        unsafe { doodle_frame_at(inst, walk_gen, count - 1, &mut frame) },
        DoodleStatus::Ok
    );
    assert!(!frame.has_callable, "the module top has no callable");
    let mut handle = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_frame_callable(inst, walk_gen, count - 1, &mut handle) },
        DoodleStatus::Ok
    );
    assert_eq!(
        handle, DOODLE_NULL_HANDLE,
        "no callable handle for the module top"
    );
    // A wrong generation is a benign ErrStale (re-walk), not a contract error.
    assert_eq!(
        unsafe { doodle_frame_at(inst, walk_gen.wrapping_add(1), 0, &mut frame) },
        DoodleStatus::ErrStale
    );
    // A live generation with an out-of-range index is a bounds error (checked AFTER the walk_gen).
    assert_eq!(
        unsafe { doodle_frame_at(inst, walk_gen, count + 10, &mut frame) },
        DoodleStatus::ErrIndexOutOfBounds
    );
    // A paused instance has a current position; its module token resolves to a canonical id.
    let (mut pos, mut has) = (zero_pos(), false);
    assert_eq!(
        unsafe { doodle_current_position(inst, &mut pos, &mut has) },
        DoodleStatus::Ok
    );
    assert!(has, "a paused instance has a current position");
    let mut buf = [0u8; 256];
    let mut len = 0usize;
    assert_eq!(
        unsafe {
            doodle_module_canonical_id(inst, pos.module, buf.as_mut_ptr(), buf.len(), &mut len)
        },
        DoodleStatus::Ok,
        "the position's module token resolves"
    );
    // A fabricated module token is a contract error, not a resolve.
    assert_eq!(
        unsafe { doodle_module_canonical_id(inst, 9999, buf.as_mut_ptr(), buf.len(), &mut len) },
        DoodleStatus::ErrContract
    );
    // The result register reads cleanly (Void or a value) at a pause.
    let mut result = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_current_result(inst, &mut result) },
        DoodleStatus::Ok
    );
    // Advancing the drive bumps the generation → the old token is now stale.
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Completed);
    assert_eq!(out.value, 0, "a module drive completes Void");
    assert_eq!(
        unsafe { doodle_frame_at(inst, walk_gen, 0, &mut frame) },
        DoodleStatus::ErrStale,
        "the pre-drive generation is stale after a drive"
    );
    unsafe { doodle_free(inst) };
}

#[test]
fn a_zero_fuel_slice_preserves_the_pause_generation() {
    // MINOR fix: `doodle_drive_slice(fuel=0)` yields Paused(SliceEnd) without running anything, so
    // it must NOT bump the pause generation: a frame token a debugger holds stays valid across it
    // (a nonzero-fuel slice, which does run, bumps as usual).
    let inst = load_and_pause("let x = 1\nlet y = 2\n");
    let (mut count, mut gen0) = (0u32, 0u32);
    assert_eq!(
        unsafe { doodle_stack_frame_count(inst, &mut count, &mut gen0) },
        DoodleStatus::Ok
    );

    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive_slice(inst, DoodleDirective::RunToCompletion as u32, 0, &mut out) },
        DoodleStatus::Ok
    );
    assert_eq!(out.kind, DoodleOutcomeKind::Paused);
    assert_eq!(out.pause_reason, DoodlePauseReason::SliceEnd);

    let (mut count2, mut gen1) = (0u32, 0u32);
    assert_eq!(
        unsafe { doodle_stack_frame_count(inst, &mut count2, &mut gen1) },
        DoodleStatus::Ok
    );
    assert_eq!(
        gen1, gen0,
        "a zero-fuel slice did no work, so the generation is unchanged"
    );
    // The pre-slice frame token is therefore still live, not stale.
    let mut frame = zero_frame();
    assert_eq!(
        unsafe { doodle_frame_at(inst, gen0, count - 1, &mut frame) },
        DoodleStatus::Ok,
        "a frame token from before a zero-fuel slice is still valid"
    );

    // A nonzero-fuel slice DOES run, so it bumps the generation as usual.
    assert_eq!(
        unsafe { doodle_drive_slice(inst, DoodleDirective::RunToCompletion as u32, 1, &mut out) },
        DoodleStatus::Ok
    );
    let (mut count3, mut gen2) = (0u32, 0u32);
    assert_eq!(
        unsafe { doodle_stack_frame_count(inst, &mut count3, &mut gen2) },
        DoodleStatus::Ok
    );
    assert_ne!(
        gen2, gen1,
        "a nonzero-fuel slice ran, so the generation bumped"
    );
    unsafe { doodle_free(inst) };
}

#[test]
fn observation_frame_callable_for_a_function_frame() {
    // StepInto descends into f(); its frame is a callable frame with a mintable callable handle.
    let inst = load("to f()\nlet z = 1\nend\nf()\n");
    let mut out = DoodleOutcome::blank();
    let mut reached = false;
    for _ in 0..20 {
        assert_eq!(
            unsafe { doodle_drive(inst, DoodleDirective::StepInto as u32, &mut out) },
            DoodleStatus::Ok
        );
        if out.kind != DoodleOutcomeKind::Paused {
            break;
        }
        let (mut count, mut walk_gen) = (0u32, 0u32);
        assert_eq!(
            unsafe { doodle_stack_frame_count(inst, &mut count, &mut walk_gen) },
            DoodleStatus::Ok
        );
        if count >= 2 {
            // Frame 0 (innermost) is f's callable frame.
            let mut frame = zero_frame();
            assert_eq!(
                unsafe { doodle_frame_at(inst, walk_gen, 0, &mut frame) },
                DoodleStatus::Ok
            );
            assert!(frame.has_callable, "the innermost frame runs f");
            let mut handle = DOODLE_NULL_HANDLE;
            assert_eq!(
                unsafe { doodle_frame_callable(inst, walk_gen, 0, &mut handle) },
                DoodleStatus::Ok
            );
            assert_ne!(
                handle, DOODLE_NULL_HANDLE,
                "f's frame yields a callable handle"
            );
            let _ = unsafe { doodle_release(inst, handle) };
            reached = true;
            break;
        }
    }
    assert!(reached, "StepInto reached f's callable frame");
    unsafe { doodle_free(inst) };
}

// ---- M7.3b: frame bindings + module globals ------------------------------------------------

/// Copies out a name via a `(buf, cap, out_len)` copy-out accessor into a `String`.
fn read_name(mut copy: impl FnMut(*mut u8, usize, *mut usize) -> DoodleStatus) -> String {
    let mut len = 0usize;
    let _ = copy(ptr::null_mut(), 0, &mut len);
    let mut buf = vec![0u8; len];
    assert_eq!(
        copy(buf.as_mut_ptr(), buf.len(), &mut len),
        DoodleStatus::Ok
    );
    String::from_utf8(buf).unwrap()
}

#[test]
fn observation_frame_locals_and_module_globals() {
    // g (a module `let`) + f(p) with a local z; pause inside f(5) and read its locals and the
    // module globals through the pull surface.
    let inst = load("let g = 10\nto f(p)\nlet z = p + 1\nend\nf(5)\n");
    let mut out = DoodleOutcome::blank();
    let (mut count, mut walk_gen) = (0u32, 0u32);
    let mut inside = false;
    for _ in 0..30 {
        assert_eq!(
            unsafe { doodle_drive(inst, DoodleDirective::StepInto as u32, &mut out) },
            DoodleStatus::Ok
        );
        if out.kind != DoodleOutcomeKind::Paused {
            break;
        }
        assert_eq!(
            unsafe { doodle_stack_frame_count(inst, &mut count, &mut walk_gen) },
            DoodleStatus::Ok
        );
        if count >= 2 {
            inside = true;
            break;
        }
    }
    assert!(inside, "paused inside f");

    // f's locals include the parameter `p` = 5.
    let mut local_count = 0u32;
    assert_eq!(
        unsafe { doodle_frame_local_count(inst, walk_gen, 0, &mut local_count) },
        DoodleStatus::Ok
    );
    assert!(local_count >= 1, "f has at least the parameter p");
    let mut found_p = false;
    for slot in 0..local_count {
        let name = read_name(|buf, cap, out_len| unsafe {
            doodle_frame_local_name(inst, walk_gen, 0, slot, buf, cap, out_len)
        });
        if name == "p" {
            let mut h = DOODLE_NULL_HANDLE;
            assert_eq!(
                unsafe { doodle_frame_local_value(inst, walk_gen, 0, slot, &mut h) },
                DoodleStatus::Ok
            );
            assert_ne!(h, DOODLE_NULL_HANDLE, "p is bound");
            let mut n = 0i64;
            assert_eq!(unsafe { doodle_as_int(inst, h, &mut n) }, DoodleStatus::Ok);
            assert_eq!(n, 5, "p == 5");
            let _ = unsafe { doodle_release(inst, h) };
            found_p = true;
        }
    }
    assert!(found_p, "found the parameter p among f's locals");

    // Dynamic-parameter bindings: f established none, so the count is 0 (accessor works).
    let mut dyn_count = 99u32;
    assert_eq!(
        unsafe { doodle_frame_dynamic_count(inst, walk_gen, 0, &mut dyn_count) },
        DoodleStatus::Ok
    );
    assert_eq!(dyn_count, 0, "f has no `with` bindings");

    // The module globals of f's frame include g (a Let) = 10.
    let mut frame = zero_frame();
    assert_eq!(
        unsafe { doodle_frame_at(inst, walk_gen, 0, &mut frame) },
        DoodleStatus::Ok
    );
    let module = frame.module;
    let mut global_count = 0u32;
    assert_eq!(
        unsafe { doodle_module_global_count(inst, walk_gen, module, &mut global_count) },
        DoodleStatus::Ok
    );
    assert!(global_count >= 1, "the module has globals (g, f)");
    let mut found_g = false;
    for index in 0..global_count {
        let name = read_name(|buf, cap, out_len| unsafe {
            doodle_module_global_name(inst, walk_gen, module, index, buf, cap, out_len)
        });
        if name == "g" {
            let mut global = DoodleGlobal {
                kind: DoodleGlobalKind::Let,
                decl_span: zero_pos(),
                reserved: [0; 2],
            };
            assert_eq!(
                unsafe { doodle_module_global(inst, walk_gen, module, index, &mut global) },
                DoodleStatus::Ok
            );
            assert_eq!(global.kind, DoodleGlobalKind::Let, "g is a `let`");
            let mut h = DOODLE_NULL_HANDLE;
            assert_eq!(
                unsafe { doodle_module_global_value(inst, walk_gen, module, index, &mut h) },
                DoodleStatus::Ok
            );
            assert_ne!(h, DOODLE_NULL_HANDLE, "g is defined");
            let mut n = 0i64;
            assert_eq!(unsafe { doodle_as_int(inst, h, &mut n) }, DoodleStatus::Ok);
            assert_eq!(n, 10, "g == 10");
            let _ = unsafe { doodle_release(inst, h) };
            found_g = true;
        }
    }
    assert!(found_g, "found the module global g");

    // An out-of-range slot/index on the VALUE accessors is `ErrIndexOutOfBounds` (matching the
    // `_name`/`_count` siblings), so a `NULL` handle unambiguously means "not yet initialized",
    // never "no such slot".
    let mut h = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_frame_local_value(inst, walk_gen, 0, local_count, &mut h) },
        DoodleStatus::ErrIndexOutOfBounds,
        "an out-of-range local slot is a bounds error, not a NULL value"
    );
    assert_eq!(
        unsafe { doodle_module_global_value(inst, walk_gen, module, 10_000, &mut h) },
        DoodleStatus::ErrIndexOutOfBounds,
        "an out-of-range global index is a bounds error, not a NULL value"
    );

    // A stale generation is rejected on the binding/global accessors too.
    assert_eq!(
        unsafe { doodle_frame_local_count(inst, walk_gen.wrapping_add(1), 0, &mut local_count) },
        DoodleStatus::ErrStale
    );
    assert_eq!(
        unsafe {
            doodle_module_global_count(inst, walk_gen.wrapping_add(1), module, &mut global_count)
        },
        DoodleStatus::ErrStale
    );
    unsafe { doodle_free(inst) };
}

// ---- M7.3c: structural value inspection + aux eval ------------------------------------------

/// The value handle of the entry module's global named `target` (found by scanning), asserting it
/// exists. `walk_gen` is the current pause generation; the entry module's token is 0.
fn global_handle(inst: *mut DoodleInstance, walk_gen: u32, target: &str) -> DoodleHandle {
    let mut count = 0u32;
    assert_eq!(
        unsafe { doodle_module_global_count(inst, walk_gen, 0, &mut count) },
        DoodleStatus::Ok
    );
    for i in 0..count {
        let name = read_name(|buf, cap, out_len| unsafe {
            doodle_module_global_name(inst, walk_gen, 0, i, buf, cap, out_len)
        });
        if name == target {
            let mut h = DOODLE_NULL_HANDLE;
            assert_eq!(
                unsafe { doodle_module_global_value(inst, walk_gen, 0, i, &mut h) },
                DoodleStatus::Ok
            );
            assert_ne!(h, DOODLE_NULL_HANDLE, "global {target} is defined");
            return h;
        }
    }
    panic!("global {target} not found");
}

fn as_int(inst: *const DoodleInstance, h: DoodleHandle) -> i64 {
    let mut n = 0i64;
    assert_eq!(unsafe { doodle_as_int(inst, h, &mut n) }, DoodleStatus::Ok);
    n
}

#[test]
fn inspection_reads_records_lists_dicts_and_renders() {
    let inst = load(concat!(
        "record Point with x, y end\n",
        "let p = Point(x: 1, y: 2)\n",
        "let xs = [10, 20, 30]\n",
        "let d = {a: 7}\n",
        "let n = 42\n",
    ));
    let out = drive(inst);
    assert_eq!(out.kind, DoodleOutcomeKind::Completed);
    // The module completed; its globals persist. Read the current generation to address them.
    let (mut count, mut walk_gen) = (0u32, 0u32);
    assert_eq!(
        unsafe { doodle_stack_frame_count(inst, &mut count, &mut walk_gen) },
        DoodleStatus::Ok
    );

    // Record: type name, field count, field names + values.
    let p = global_handle(inst, walk_gen, "p");
    let type_name = read_name(|buf, cap, out_len| unsafe {
        doodle_record_type_name(inst, p, buf, cap, out_len)
    });
    assert_eq!(type_name, "Point");
    let mut fields = 0u32;
    assert_eq!(
        unsafe { doodle_record_length(inst, p, &mut fields) },
        DoodleStatus::Ok
    );
    assert_eq!(fields, 2);
    let f0 = read_name(|buf, cap, out_len| unsafe {
        doodle_record_field_name(inst, p, 0, buf, cap, out_len)
    });
    assert_eq!(f0, "x");
    let mut fv = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_record_field(inst, p, 0, &mut fv) },
        DoodleStatus::Ok
    );
    assert_eq!(as_int(inst, fv), 1);
    let _ = unsafe { doodle_release(inst, fv) };
    let _ = unsafe { doodle_release(inst, p) };

    // List: length + element.
    let xs = global_handle(inst, walk_gen, "xs");
    let mut len = 0u32;
    assert_eq!(
        unsafe { doodle_list_length(inst, xs, &mut len) },
        DoodleStatus::Ok
    );
    assert_eq!(len, 3);
    let mut e1 = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_list_get(inst, xs, 1, &mut e1) },
        DoodleStatus::Ok
    );
    assert_eq!(as_int(inst, e1), 20);
    let _ = unsafe { doodle_release(inst, e1) };
    let _ = unsafe { doodle_release(inst, xs) };

    // Dict: length + key/value in insertion order.
    let d = global_handle(inst, walk_gen, "d");
    let mut dlen = 0u32;
    assert_eq!(
        unsafe { doodle_dict_length(inst, d, &mut dlen) },
        DoodleStatus::Ok
    );
    assert_eq!(dlen, 1);
    let mut key = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_dict_key(inst, d, 0, &mut key) },
        DoodleStatus::Ok
    );
    let mut klen = 0usize;
    let _ = unsafe { doodle_string_bytes(inst, key, ptr::null_mut(), 0, &mut klen) };
    let mut kbuf = vec![0u8; klen];
    assert_eq!(
        unsafe { doodle_string_bytes(inst, key, kbuf.as_mut_ptr(), kbuf.len(), &mut klen) },
        DoodleStatus::Ok
    );
    assert_eq!(kbuf, b"a");
    let mut val = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_dict_value(inst, d, 0, &mut val) },
        DoodleStatus::Ok
    );
    assert_eq!(as_int(inst, val), 7);
    let _ = unsafe { doodle_release(inst, key) };
    let _ = unsafe { doodle_release(inst, val) };
    let _ = unsafe { doodle_release(inst, d) };

    // Auxiliary evaluation: render n (42) to its to_string.
    let n = global_handle(inst, walk_gen, "n");
    let mut aux = DoodleAuxOutcome {
        kind: DoodleAuxOutcomeKind::Faulted,
        value: DOODLE_NULL_HANDLE,
        fault: DoodleFault::Internal,
        reserved: [0; 2],
    };
    assert_eq!(
        unsafe { doodle_eval_to_string(inst, n, 100_000, &mut aux) },
        DoodleStatus::Ok
    );
    assert_eq!(aux.kind, DoodleAuxOutcomeKind::Rendered);
    let mut slen = 0usize;
    let _ = unsafe { doodle_string_bytes(inst, aux.value, ptr::null_mut(), 0, &mut slen) };
    let mut sbuf = vec![0u8; slen];
    assert_eq!(
        unsafe { doodle_string_bytes(inst, aux.value, sbuf.as_mut_ptr(), sbuf.len(), &mut slen) },
        DoodleStatus::Ok
    );
    assert_eq!(sbuf, b"42");
    let _ = unsafe { doodle_release(inst, aux.value) };
    let _ = unsafe { doodle_release(inst, n) };
    unsafe { doodle_free(inst) };
}

// ---- M7.3d: debug setup (breakpoints, raise-trap, pause, mode, tail, diagnostics) ----------

#[test]
fn debug_breakpoint_set_hit_list_and_clear() {
    let inst = load("let a = 1\nlet b = 2\nlet c = 3\n");
    // The entry module's canonical id (breakpoints are addressed by canonical string).
    let canonical = read_name(|buf, cap, out_len| unsafe {
        doodle_module_canonical_id(inst, 0, buf, cap, out_len)
    });
    let mut id = 0u32;
    assert_eq!(
        unsafe { doodle_set_breakpoint(inst, canonical.as_ptr(), canonical.len(), 2, &mut id) },
        DoodleStatus::Ok
    );
    // The breakpoint list reflects it.
    let mut bc = 0u32;
    assert_eq!(
        unsafe { doodle_breakpoint_count(inst, &mut bc) },
        DoodleStatus::Ok
    );
    assert_eq!(bc, 1);
    let mut bp = DoodleBreakpoint {
        id: 0,
        line: 0,
        resolved: false,
        reserved: [0; 2],
    };
    assert_eq!(
        unsafe { doodle_breakpoint_at(inst, 0, &mut bp) },
        DoodleStatus::Ok
    );
    assert_eq!(bp.id, id);
    assert_eq!(bp.line, 2);
    assert!(
        bp.resolved,
        "the entry module is loaded, so the breakpoint resolves"
    );
    let bp_canonical = read_name(|buf, cap, out_len| unsafe {
        doodle_breakpoint_canonical_id(inst, 0, buf, cap, out_len)
    });
    assert_eq!(bp_canonical, canonical);
    // Driving under `Continue` stops at the breakpoint (RunToCompletion would ignore it).
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, DoodleDirective::Continue as u32, &mut out) },
        DoodleStatus::Ok
    );
    assert_eq!(out.kind, DoodleOutcomeKind::Paused);
    assert_eq!(out.pause_reason, DoodlePauseReason::Breakpoint);
    assert_eq!(out.breakpoint_id, id);
    // Clear it and confirm the list empties.
    assert_eq!(
        unsafe { doodle_clear_breakpoint(inst, id) },
        DoodleStatus::Ok
    );
    assert_eq!(
        unsafe { doodle_breakpoint_count(inst, &mut bc) },
        DoodleStatus::Ok
    );
    assert_eq!(bc, 0);
    unsafe { doodle_free(inst) };
}

#[test]
fn debug_raise_trap_pauses_and_exposes_the_trapped_value() {
    let inst = load("1 / 0\n");
    assert_eq!(
        unsafe { doodle_set_raise_trapping(inst, true) },
        DoodleStatus::Ok
    );
    let mut on = false;
    assert_eq!(
        unsafe { doodle_raise_trapping(inst, &mut on) },
        DoodleStatus::Ok
    );
    assert!(on);
    // Under `Continue`, the armed raise pauses before propagating.
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, DoodleDirective::Continue as u32, &mut out) },
        DoodleStatus::Ok
    );
    assert_eq!(out.kind, DoodleOutcomeKind::Paused);
    assert_eq!(out.pause_reason, DoodlePauseReason::RaiseTrap);
    // The trapped value + its position are exposed.
    let mut h = DOODLE_NULL_HANDLE;
    assert_eq!(
        unsafe { doodle_trapped_raise(inst, &mut h) },
        DoodleStatus::Ok
    );
    assert_ne!(h, DOODLE_NULL_HANDLE, "a trapped raise has a value");
    let _ = unsafe { doodle_release(inst, h) };
    let (mut pos, mut has) = (
        DoodlePosition {
            span_start: 0,
            span_end: 0,
            module: 0,
            reserved: [0; 1],
        },
        false,
    );
    assert_eq!(
        unsafe { doodle_trapped_raise_position(inst, &mut pos, &mut has) },
        DoodleStatus::Ok
    );
    assert!(has, "the trapped raise has a position");
    unsafe { doodle_free(inst) };
}

#[test]
fn debug_mode_pause_tail_and_diagnostics() {
    let inst = load("let a = 1\nlet b = 2\n");
    // Observation mode round-trips at runtime.
    assert_eq!(
        unsafe { doodle_set_observation_mode(inst, DoodleObservationMode::Subexpression as u32) },
        DoodleStatus::Ok
    );
    let mut mode = DoodleObservationMode::Statement;
    assert_eq!(
        unsafe { doodle_observation_mode(inst, &mut mode) },
        DoodleStatus::Ok
    );
    assert_eq!(mode, DoodleObservationMode::Subexpression);

    // A host pause stops the next drive at a safe point, regardless of directive.
    unsafe { doodle_pause(inst) };
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, DoodleDirective::RunToCompletion as u32, &mut out) },
        DoodleStatus::Ok
    );
    assert_eq!(out.kind, DoodleOutcomeKind::Paused);
    assert_eq!(out.pause_reason, DoodlePauseReason::HostPause);

    // Tail-elided history is pause-scoped: the accessor works at a pause, and rejects a stale gen.
    let (mut count, mut walk_gen) = (0u32, 0u32);
    assert_eq!(
        unsafe { doodle_stack_frame_count(inst, &mut count, &mut walk_gen) },
        DoodleStatus::Ok
    );
    let mut tail = 99u32;
    assert_eq!(
        unsafe { doodle_tail_history_count(inst, walk_gen, &mut tail) },
        DoodleStatus::Ok
    );
    assert_eq!(tail, 0, "no tail recursion in this program");
    assert_eq!(
        unsafe { doodle_tail_history_count(inst, walk_gen.wrapping_add(1), &mut tail) },
        DoodleStatus::ErrStale
    );

    // The load-diagnostics record reads (clean program → 0); out-of-range is a bounds error.
    let mut diags = 7u32;
    assert_eq!(
        unsafe { doodle_diagnostic_count(inst, 0, &mut diags) },
        DoodleStatus::Ok
    );
    assert_eq!(diags, 0, "a clean program has no diagnostics");
    let mut diag = DoodleDiagnostic {
        severity: DoodleSeverity::Error,
        has_span: false,
        span_start: 0,
        span_end: 0,
        reserved: [0; 2],
    };
    assert_eq!(
        unsafe { doodle_diagnostic_at(inst, 0, 0, &mut diag) },
        DoodleStatus::ErrIndexOutOfBounds
    );
    unsafe { doodle_free(inst) };
}

// A raw pointer handed to exactly one other thread, which then owns the pointee exclusively for the
// duration of the drive — the C-host pattern of moving an instance to its drive thread. Sound
// because only the receiving thread touches the pointee while the drive runs, and the sender only
// after `join`.
struct Handoff<T>(*mut T);
// SAFETY: the pointee (a `DoodleInstance`/`DoodleControl`) is handed to one thread that owns it for
// the drive; the C ABI's own contract (single-threaded-per-instance) is what the test upholds.
unsafe impl<T> Send for Handoff<T> {}

impl<T> Handoff<T> {
    /// Unwraps the pointer. Takes `self` by value so a closure calling it captures the whole
    /// (`Send`) `Handoff`, not just the bare `*mut` field (disjoint closure capture would otherwise
    /// take only the field, which is not `Send`).
    fn into_inner(self) -> *mut T {
        self.0
    }
}

#[test]
fn two_instances_run_independently_on_two_threads() {
    // Instances are `Send` (M7.0): each is created here, moved into its own thread, and driven
    // there with no shared state, so the two runs are independent with their own output.
    let handles: Vec<_> = ["print(1)\n", "print(2)\n"]
        .iter()
        .map(|src| {
            let handoff = Handoff(load_with(src, &[DoodleBuiltin::Print]));
            std::thread::spawn(move || {
                let inst = handoff.into_inner();
                assert_eq!(drive(inst).kind, DoodleOutcomeKind::Completed);
                let out = read_output(inst);
                unsafe { doodle_free(inst) };
                out
            })
        })
        .collect();
    let outputs: Vec<Vec<u8>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(outputs[0], b"1\n");
    assert_eq!(outputs[1], b"2\n");
}

#[test]
fn a_drive_is_cancelled_from_another_thread_via_the_control() {
    // The D-M7-5 cross-thread control: `to spin() spin() end` (unbounded tail recursion, with a
    // huge step budget so it never faults StepBudget) is driven on one thread and cancelled from
    // this one through a `DoodleControl` — which owns the cancel token's `Arc` and never re-forms
    // `&Instance` (the M7.6 fix for the cross-thread `&Instance` aliasing). The loop can only end
    // when the cancel flag is seen, so the outcome is deterministically Faulted(Cancelled).
    let src = "to spin()\n  spin()\nend\nspin()\n";
    let config = doodle_config_new();
    unsafe { doodle_config_set_limits(config, u64::MAX, 1 << 26, 10_000, 1 << 26) };
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    assert_eq!(
        unsafe {
            doodle_load(
                src.as_ptr(),
                src.len(),
                config,
                &mut inst,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        },
        DoodleStatus::Ok
    );
    unsafe { doodle_config_free(config) };

    let control = unsafe { doodle_control(inst) };
    assert!(!control.is_null());
    let driver = {
        let handoff = Handoff(inst);
        std::thread::spawn(move || drive(handoff.into_inner()))
    };
    // Cancel while the driver holds `&mut Instance`; the control touches only the shared atomic.
    unsafe { doodle_control_cancel(control) };
    let out = driver.join().unwrap();
    assert_eq!(out.kind, DoodleOutcomeKind::Faulted);
    assert_eq!(out.fault, DoodleFault::Cancelled);

    unsafe { doodle_control_free(control) };
    unsafe { doodle_free(inst) };
}
