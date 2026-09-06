//! GC-stress determinism through the C ABI (M7.6, D-M7-11) — the one assertion the
//! corpus-through-C gate cannot pin down: a **host foreign value's finalizer fires exactly once,
//! at GC time, across the C trampoline**, when the engine collects at every safe point. This is
//! precisely what driving through a real C host adds over the in-crate GC tests (which have no
//! trampoline and no C-driven handle lifecycle).
//!
//! It lives in its own test binary so it can set the `DOODLE_GC_STRESS` environment hook without
//! racing other tests: this binary runs exactly one test, on one thread, and reads the variable
//! (via `doodle_load`) on that same thread before anything else could observe it.

// Each `unsafe` block below is a single FFI call made exactly as a C host would, with its
// preconditions (a live instance, valid out-params) established by the surrounding setup — a
// per-call SAFETY comment would only restate that (as in `tests/abi.rs`). The one `set_var` is
// justified inline.
#![allow(clippy::undocumented_unsafe_blocks)]

// `doodle-core` is a dependency of the library but this test reaches it only through re-exports;
// naming it keeps Miri's `unused-crate-dependencies` force-warn quiet (a no-op for normal builds).
use doodle_core as _;

use doodle_capi::abi::{DoodleDirective, DoodleOutcome, DoodleOutcomeKind, DoodleStatus};
use doodle_capi::instance::{DoodleInstance, doodle_drive, doodle_free, doodle_load};
use doodle_capi::value::{doodle_make_foreign, doodle_release};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

static FINALIZED: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_final(_ptr: u64) {
    FINALIZED.fetch_add(1, Relaxed);
}

#[test]
fn a_foreign_value_finalizes_once_at_gc_under_stress_through_c() {
    // SAFETY: one test on one thread in this binary; nothing else reads or writes the environment
    // concurrently, and the variable is set before the first `doodle_load` reads it.
    unsafe { std::env::set_var("DOODLE_GC_STRESS", "1") };

    // A loop gives many safe points; under GC-stress every safe point collects. `doodle_load`
    // latches the stress hook from the env before the first drive.
    let src = "let n = 0\nwhile n < 50 do n = n + 1 end\nn\n";
    let mut inst: *mut DoodleInstance = ptr::null_mut();
    assert_eq!(
        unsafe {
            doodle_load(
                src.as_ptr(),
                src.len(),
                ptr::null(),
                &mut inst,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        },
        DoodleStatus::Ok
    );
    assert!(!inst.is_null());

    // Make a host foreign value with a finalizer, then release its only handle: now unrooted, it
    // dies at the next collection.
    let mut handle = 0u64;
    assert_eq!(
        unsafe { doodle_make_foreign(inst, 1, 99, Some(on_final), &mut handle) },
        DoodleStatus::Ok
    );
    assert_eq!(
        FINALIZED.load(Relaxed),
        0,
        "not finalized while the handle roots it"
    );
    assert_eq!(unsafe { doodle_release(inst, handle) }, DoodleStatus::Ok);

    // Drive: under GC-stress the loop's safe points each collect, so the unrooted foreign value is
    // reclaimed *during the drive* — its finalizer runs at GC time, through the C trampoline.
    let mut out = DoodleOutcome::blank();
    assert_eq!(
        unsafe { doodle_drive(inst, DoodleDirective::RunToCompletion, &mut out) },
        DoodleStatus::Ok
    );
    assert_eq!(out.kind, DoodleOutcomeKind::Completed);
    assert_eq!(
        FINALIZED.load(Relaxed),
        1,
        "finalized exactly once, at GC, before destroy"
    );

    // Destroy must not double-finalize an already-collected value.
    unsafe { doodle_free(inst) };
    assert_eq!(
        FINALIZED.load(Relaxed),
        1,
        "still finalized exactly once after destroy (no double-free)"
    );
}
