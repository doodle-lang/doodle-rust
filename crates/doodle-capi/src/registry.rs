//! Host-extension registration (E§5.5): a [`DoodleRegistry`] the host builds **before**
//! loading, then hands to `doodle_load_with_registry` (registration order is replay identity,
//! §11). M7.2a registers the engine's **built-in** intrinsics (`print`, capabilities like
//! `read_line`, …) — the set the wasm/native conformance hosts install, so a C host can run the
//! same programs with the same namespace. `doodle_registry_add_foreign` registers an arbitrary
//! **host callback** (a C foreign function, built as a [`DoodleForeignDesc`]) — the
//! control-inverting M7.2b piece.

use crate::abi::DoodleStatus;
use crate::desc::DoodleForeignDesc;
use crate::guard::catch;
use doodle_core::machine::{
    Intrinsic, Registry, clear_canvas_intrinsic, cos_intrinsic, decode_intrinsic,
    draw_line_intrinsic, each_intrinsic, encode_intrinsic, length_intrinsic, print_intrinsic,
    random_intrinsic, read_line_intrinsic, set_turtle_intrinsic, sin_intrinsic, time_intrinsic,
};

/// An engine-provided built-in intrinsic a host can register by identity (E§5.5), without
/// supplying a callback. The synchronous ones (`Print`/`Length`/…) run inline; the
/// capabilities (`ReadLine`/`DrawLine`/`SetTurtle`/`ClearCanvas`) suspend to the host.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoodleBuiltin {
    /// `print(value)` — renders a value and a newline to the output sink (synchronous).
    Print = 0,
    /// `length(x)` — the length of a string/list/dict/bytes (synchronous).
    Length = 1,
    /// `each(list) do (x) … end` — a native block-consumer (synchronous, reentrant).
    Each = 2,
    /// `encode(string) -> bytes` — UTF-8 encode (synchronous).
    Encode = 3,
    /// `decode(bytes) -> string` — UTF-8 decode (synchronous).
    Decode = 4,
    /// `read_line() -> string` — a suspending input capability.
    ReadLine = 5,
    /// `sin(x) -> float` (synchronous, deterministic libm).
    Sin = 6,
    /// `cos(x) -> float` (synchronous, deterministic libm).
    Cos = 7,
    /// `draw_line(x0,y0,x1,y1,r,g,b,a)` — a suspending turtle capability.
    DrawLine = 8,
    /// `set_turtle(x,y,heading,visible)` — a suspending turtle capability.
    SetTurtle = 9,
    /// `clear_canvas()` — a suspending turtle capability.
    ClearCanvas = 10,
    /// `time() -> number` — a suspending clock capability (E§5.3/S-19: a clock read crosses the
    /// recordable boundary, never a synchronous foreign function).
    Time = 11,
    /// `random() -> number` — a suspending entropy capability (E§5.3/S-19).
    Random = 12,
}

/// A host-extension registry under construction (E§5.5). Built with `doodle_registry_new`,
/// populated in order with `doodle_registry_add_builtin`, then **consumed** by
/// `doodle_load_with_registry`. Opaque to C.
pub struct DoodleRegistry {
    pub(crate) inner: Registry,
}

/// Creates an empty registry. Returns NULL only on allocation failure. Populate it in the
/// order the host wants capability ids assigned (registration order is replay identity, §11),
/// then pass it to `doodle_load_with_registry` (which consumes it) — or free an unused one
/// with `doodle_registry_free`.
#[unsafe(no_mangle)]
pub extern "C" fn doodle_registry_new() -> *mut DoodleRegistry {
    Box::into_raw(Box::new(DoodleRegistry {
        inner: Registry::new(),
    }))
}

/// Frees a registry created by `doodle_registry_new` that was **not** consumed by
/// `doodle_load_with_registry`. NULL is a no-op. Do not call this on a registry already
/// passed to `doodle_load_with_registry` (that consumed it).
///
/// # Safety
/// `registry` must be a pointer from `doodle_registry_new` not already freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_registry_free(registry: *mut DoodleRegistry) {
    if !registry.is_null() {
        // SAFETY: non-null (checked); a `Box::into_raw` pointer from `doodle_registry_new`,
        // not consumed/freed, by the caller's contract.
        drop(unsafe { Box::from_raw(registry) });
    }
}

/// Registers an engine built-in by identity (E§5.5), appending it to the registry. Returns
/// `DoodleStatus_ErrContract` if the name is already registered (a host bug — each built-in
/// registers at most once). No-op returning `ErrNullPointer` on a NULL registry.
///
/// # Safety
/// `registry` must be a live pointer from `doodle_registry_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_registry_add_builtin(
    registry: *mut DoodleRegistry,
    builtin: DoodleBuiltin,
) -> DoodleStatus {
    // SAFETY: `as_mut` returns None for NULL; a non-null `registry` is a live `DoodleRegistry`
    // from `doodle_registry_new` (not consumed) by the caller's contract.
    let Some(registry) = (unsafe { registry.as_mut() }) else {
        return DoodleStatus::ErrNullPointer;
    };
    match registry.inner.register(intrinsic_for(builtin)) {
        Ok(()) => DoodleStatus::Ok,
        // The only registration failure is a duplicate/reserved name — a host setup bug.
        Err(_) => DoodleStatus::ErrContract,
    }
}

/// Registers a **host foreign function** from a descriptor (E§5.1/§5.2, M7.2b), appending it to
/// the registry. **Consumes** `desc` (the pointer is invalid after this call, success or
/// failure — do not free or reuse it). `ErrContract` if the descriptor has no callback set
/// (`doodle_foreign_desc_set_callback`) or its name duplicates a prior registration/reserved
/// name; `ErrNullPointer` on a NULL descriptor (nothing consumed) or a NULL registry.
///
/// # Safety
/// `registry` a live pointer from `doodle_registry_new` (not consumed); `desc` a live pointer
/// from `doodle_foreign_desc_new` (not consumed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_registry_add_foreign(
    registry: *mut DoodleRegistry,
    desc: *mut DoodleForeignDesc,
) -> DoodleStatus {
    catch(|| {
        if desc.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        // Consume the descriptor now (this call owns it, success or failure): dropping the `Box`
        // frees it on any early return below, so the pointer is single-use as documented.
        // SAFETY: non-null (checked); a `Box::into_raw` pointer from `doodle_foreign_desc_new`,
        // not consumed/freed, by the caller's contract.
        let desc = unsafe { Box::from_raw(desc) };
        // SAFETY: `as_mut` returns None for NULL; a non-null `registry` is a live `DoodleRegistry`
        // from `doodle_registry_new` (not consumed) by the caller's contract.
        let Some(registry) = (unsafe { registry.as_mut() }) else {
            return DoodleStatus::ErrNullPointer;
        };
        match desc.into_intrinsic() {
            // The only registration failure is a duplicate/reserved name — a host setup bug.
            Ok(intrinsic) => match registry.inner.register(intrinsic) {
                Ok(()) => DoodleStatus::Ok,
                Err(_) => DoodleStatus::ErrContract,
            },
            Err(status) => status,
        }
    })
}

/// The engine intrinsic for a built-in identity.
fn intrinsic_for(builtin: DoodleBuiltin) -> Intrinsic {
    match builtin {
        DoodleBuiltin::Print => print_intrinsic(),
        DoodleBuiltin::Length => length_intrinsic(),
        DoodleBuiltin::Each => each_intrinsic(),
        DoodleBuiltin::Encode => encode_intrinsic(),
        DoodleBuiltin::Decode => decode_intrinsic(),
        DoodleBuiltin::ReadLine => read_line_intrinsic(),
        DoodleBuiltin::Sin => sin_intrinsic(),
        DoodleBuiltin::Cos => cos_intrinsic(),
        DoodleBuiltin::DrawLine => draw_line_intrinsic(),
        DoodleBuiltin::SetTurtle => set_turtle_intrinsic(),
        DoodleBuiltin::ClearCanvas => clear_canvas_intrinsic(),
        DoodleBuiltin::Time => time_intrinsic(),
        DoodleBuiltin::Random => random_intrinsic(),
    }
}
