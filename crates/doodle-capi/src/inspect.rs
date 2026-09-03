//! Structural value inspection + auxiliary evaluation (engine spec E§8.4, M7.3c): reading a
//! value's **structure** through the engine — a record's type name + fields, a dict's entries, a
//! list's elements, a callable's name/kind/position/docstring, a type's name, a module's member
//! names — and rendering a value to its `to_string` (`doodle_eval_to_string`). These take a
//! **handle** (not a frame index), so they are **not** pause-scoped (no generation): a handle is
//! valid until released, across drives. All rendering routes through the engine's structural
//! readers — no handle ids, host formatting, or raw foreign `ptr` ever appears in the output.
//! Child accessors (`_field`/`_key`/`_value`/`_get`) mint fresh **host-owned** handles the host
//! releases. Distinct from the `DoodleCallCtx` `doodle_call_*` readers (those are M7.2b, on a
//! foreign-callback ctx); these are on `*DoodleInstance`.

use crate::abi::{
    self, DoodleAuxOutcome, DoodleAuxOutcomeKind, DoodleHandle, DoodlePosition, DoodleStatus,
};
use crate::guard::catch;
use crate::instance::{DoodleInstance, di_mut, di_ref};
use crate::value::{copy_out, write_out};
use doodle_core::machine::{AuxOutcome, Handle, Instance, ValueError};

/// The engine [`Instance`] behind a `DoodleInstance` (shared), or `None` if NULL.
fn inst_ref<'a>(instance: *const DoodleInstance) -> Option<&'a Instance> {
    di_ref(instance).map(|di| &di.inner)
}

/// The engine [`Instance`] behind a `DoodleInstance` (mutable, for the minting readers).
fn inst_mut<'a>(instance: *mut DoodleInstance) -> Option<&'a mut Instance> {
    di_mut(instance).map(|di| &mut di.inner)
}

/// Runs a `usize`-returning structural reader and writes it as a saturating `u32` count.
fn count(
    instance: *const DoodleInstance,
    reader: impl FnOnce(&Instance) -> Result<usize, ValueError>,
    out_count: *mut u32,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = inst_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match reader(inst) {
            Ok(n) if write_out(out_count, u32::try_from(n).unwrap_or(u32::MAX)) => DoodleStatus::Ok,
            Ok(_) => DoodleStatus::ErrNullPointer,
            Err(err) => abi::value_error(err),
        }
    })
}

/// Runs a handle-minting child reader (`_field`/`_key`/`_value`/`_get`) and writes the fresh
/// host-owned handle.
fn minted(
    instance: *mut DoodleInstance,
    reader: impl FnOnce(&mut Instance) -> Result<Handle, ValueError>,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    catch(|| {
        // Validate the out-param **before** minting: `write_out` is the null check, but a mint
        // that then fails to write would orphan a host-owned handle (a leak that roots its value
        // for the instance's life). Checking first means a NULL out never mints.
        if out_handle.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let Some(inst) = inst_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match reader(inst) {
            Ok(handle) if write_out(out_handle, handle.bits()) => DoodleStatus::Ok,
            Ok(_) => DoodleStatus::ErrNullPointer,
            Err(err) => abi::value_error(err),
        }
    })
}

// ---- records --------------------------------------------------------------------------------

/// Copies a record's **type name** (L§9) into `buf` (copy-out). `ErrWrongKind` if not a record.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_record_type_name(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| match inst_ref(instance) {
        Some(inst) => match inst.record_type_name(Handle::from_bits(handle)) {
            Ok(name) => copy_out(name.as_bytes(), buf, cap, out_len),
            Err(err) => abi::value_error(err),
        },
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Writes a record's field count (L§9). `ErrWrongKind` if not a record.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_record_length(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_count: *mut u32,
) -> DoodleStatus {
    count(
        instance,
        |inst| inst.record_length(Handle::from_bits(handle)),
        out_count,
    )
}

/// Copies the name of a record's `index`-th field (declaration order) into `buf` (copy-out).
/// `ErrWrongKind` if not a record; `ErrIndexOutOfBounds` past the last field.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_record_field_name(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    index: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| match inst_ref(instance) {
        Some(inst) => match inst.record_field_name(Handle::from_bits(handle), index as usize) {
            Ok(name) => copy_out(name.as_bytes(), buf, cap, out_len),
            Err(err) => abi::value_error(err),
        },
        None => DoodleStatus::ErrNullPointer,
    })
}

/// A fresh **host-owned** handle to a record's `index`-th field value (L§9). `ErrWrongKind` if
/// not a record; `ErrIndexOutOfBounds` past the last field.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_record_field(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    minted(
        instance,
        |inst| inst.record_field(Handle::from_bits(handle), index as usize),
        out_handle,
    )
}

// ---- dicts ----------------------------------------------------------------------------------

/// Writes a dict's entry count (L§4.7). `ErrWrongKind` if not a dict.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_dict_length(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_count: *mut u32,
) -> DoodleStatus {
    count(
        instance,
        |inst| inst.dict_length(Handle::from_bits(handle)),
        out_count,
    )
}

/// A fresh **host-owned** handle to a dict's `index`-th key, in insertion order (L§4.7).
/// `ErrWrongKind` if not a dict; `ErrIndexOutOfBounds` past the last entry.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_dict_key(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    minted(
        instance,
        |inst| inst.dict_key(Handle::from_bits(handle), index as usize),
        out_handle,
    )
}

/// A fresh **host-owned** handle to a dict's `index`-th value, in insertion order (L§4.7).
/// `ErrWrongKind` if not a dict; `ErrIndexOutOfBounds` past the last entry.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_dict_value(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    minted(
        instance,
        |inst| inst.dict_value(Handle::from_bits(handle), index as usize),
        out_handle,
    )
}

// ---- lists ----------------------------------------------------------------------------------

/// Writes a list's length (L§4.6). `ErrWrongKind` if not a list.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_list_length(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_count: *mut u32,
) -> DoodleStatus {
    count(
        instance,
        |inst| inst.list_length(Handle::from_bits(handle)),
        out_count,
    )
}

/// A fresh **host-owned** handle to a list's `index`-th element (L§4.6). `ErrWrongKind` if not a
/// list; `ErrIndexOutOfBounds` past the end.
///
/// # Safety
/// `instance` live; `out_handle` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_list_get(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
    index: u32,
    out_handle: *mut DoodleHandle,
) -> DoodleStatus {
    minted(
        instance,
        |inst| inst.list_get(Handle::from_bits(handle), index as usize),
        out_handle,
    )
}

// ---- callables / types / modules ------------------------------------------------------------

/// Copies a callable's name into `buf` (copy-out) when it has one; `out_has` reports whether it
/// does (an anonymous `fn` has none). `ErrWrongKind` if the value is not a callable.
///
/// # Safety
/// `instance` live; `out_has` writable; `buf` readable for `cap` (or NULL with `cap` 0);
/// `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_callable_name(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_has: *mut bool,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = inst_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.callable_name(Handle::from_bits(handle)) {
            Ok(Some(name)) => {
                if !write_out(out_has, true) {
                    return DoodleStatus::ErrNullPointer;
                }
                copy_out(name.as_bytes(), buf, cap, out_len)
            }
            Ok(None) => {
                if !write_out(out_has, false) {
                    return DoodleStatus::ErrNullPointer;
                }
                copy_out(&[], buf, cap, out_len)
            }
            Err(err) => abi::value_error(err),
        }
    })
}

/// Writes whether a callable is a function (`fn`, yields a value) vs a procedure (`to`) into
/// `out_is_function`; `out_has` is `false` if the kind is indeterminate. `ErrWrongKind` if not a
/// callable.
///
/// # Safety
/// `instance` live; `out_has`/`out_is_function` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_callable_is_function(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_has: *mut bool,
    out_is_function: *mut bool,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = inst_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.callable_is_function(Handle::from_bits(handle)) {
            Ok(opt) => {
                let has = opt.is_some();
                if write_out(out_has, has) && write_out(out_is_function, opt.unwrap_or(false)) {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            Err(err) => abi::value_error(err),
        }
    })
}

/// Writes a callable's definition position into `out_position` when known; `out_has` reports
/// whether it is. `ErrWrongKind` if not a callable.
///
/// # Safety
/// `instance` live; `out_position`/`out_has` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_callable_position(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    optional_position(instance, out_position, out_has, |inst| {
        inst.callable_position(Handle::from_bits(handle))
    })
}

/// Writes a callable's **docstring** position into `out_position` when it has one; `out_has`
/// reports whether it does. `ErrWrongKind` if not a callable.
///
/// # Safety
/// `instance` live; `out_position`/`out_has` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_callable_docstring(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
) -> DoodleStatus {
    optional_position(instance, out_position, out_has, |inst| {
        inst.callable_docstring(Handle::from_bits(handle))
    })
}

/// Shared body of the optional-position callable readers.
fn optional_position(
    instance: *const DoodleInstance,
    out_position: *mut DoodlePosition,
    out_has: *mut bool,
    reader: impl FnOnce(&Instance) -> Result<Option<doodle_core::machine::Position>, ValueError>,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = inst_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match reader(inst) {
            Ok(Some(pos)) => {
                if write_out(out_position, abi::position(pos)) && write_out(out_has, true) {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            Ok(None) => {
                if write_out(out_has, false) {
                    DoodleStatus::Ok
                } else {
                    DoodleStatus::ErrNullPointer
                }
            }
            Err(err) => abi::value_error(err),
        }
    })
}

/// Copies a type value's name (L§4.12) into `buf` (copy-out). `ErrWrongKind` if not a type value.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_type_name(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| match inst_ref(instance) {
        Some(inst) => match inst.type_name(Handle::from_bits(handle)) {
            Ok(name) => copy_out(name.as_bytes(), buf, cap, out_len),
            Err(err) => abi::value_error(err),
        },
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Writes a module value's public member count (L§9). `ErrWrongKind` if not a module value.
///
/// # Safety
/// `instance` live; `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_member_count(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    out_count: *mut u32,
) -> DoodleStatus {
    count(
        instance,
        |inst| {
            inst.module_member_names(Handle::from_bits(handle))
                .map(|m| m.len())
        },
        out_count,
    )
}

/// Copies the name of a module value's `index`-th public member into `buf` (copy-out).
/// `ErrWrongKind` if not a module value; `ErrIndexOutOfBounds` past the last member.
///
/// # Safety
/// `instance` live; `buf` readable for `cap` (or NULL with `cap` 0); `out_len` may be NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_module_member_name(
    instance: *const DoodleInstance,
    handle: DoodleHandle,
    index: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> DoodleStatus {
    catch(|| {
        let Some(inst) = inst_ref(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        match inst.module_member_names(Handle::from_bits(handle)) {
            Ok(members) => match members.get(index as usize) {
                Some(name) => copy_out(name.as_bytes(), buf, cap, out_len),
                None => DoodleStatus::ErrIndexOutOfBounds,
            },
            Err(err) => abi::value_error(err),
        }
    })
}

// ---- auxiliary evaluation -------------------------------------------------------------------

/// Renders `handle`'s value to its `to_string` (E§8.4) with its **own** `fuel` budget, writing
/// the result to `out_outcome` — **without disturbing the instance's pause** (S-22). It runs
/// Doodle code, so it may render, raise, or fault (see [`DoodleAuxOutcome`]); breakpoints and the
/// raise-trap are suppressed, and the pause generation is **not** bumped (frame addressing stays
/// valid across it). `Rendered`/`Raised` carry host-owned handles the host releases.
///
/// # Safety
/// `instance` live; `out_outcome` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_eval_to_string(
    instance: *mut DoodleInstance,
    handle: DoodleHandle,
    fuel: u64,
    out_outcome: *mut DoodleAuxOutcome,
) -> DoodleStatus {
    catch(|| {
        // Validate the out-param before the (effectful, handle-minting) eval, so a NULL out never
        // orphans the Rendered/Raised handle.
        if out_outcome.is_null() {
            return DoodleStatus::ErrNullPointer;
        }
        let Some(inst) = inst_mut(instance) else {
            return DoodleStatus::ErrNullPointer;
        };
        let outcome = match inst.eval_to_string(Handle::from_bits(handle), fuel) {
            AuxOutcome::Rendered(h) => DoodleAuxOutcome {
                kind: DoodleAuxOutcomeKind::Rendered,
                value: h.bits(),
                fault: abi::fault(doodle_core::drive::EngineFault::Internal),
                reserved: [0; 2],
            },
            AuxOutcome::Raised(h) => DoodleAuxOutcome {
                kind: DoodleAuxOutcomeKind::Raised,
                value: h.bits(),
                fault: abi::fault(doodle_core::drive::EngineFault::Internal),
                reserved: [0; 2],
            },
            AuxOutcome::Faulted(f) => DoodleAuxOutcome {
                kind: DoodleAuxOutcomeKind::Faulted,
                value: crate::abi::DOODLE_NULL_HANDLE,
                fault: abi::fault(f),
                reserved: [0; 2],
            },
        };
        if write_out(out_outcome, outcome) {
            DoodleStatus::Ok
        } else {
            DoodleStatus::ErrNullPointer
        }
    })
}
