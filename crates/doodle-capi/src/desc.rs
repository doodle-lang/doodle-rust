//! The host foreign-function descriptor (E§5.1, D-M7-7): an opaque builder a C host fills in —
//! name, kind, parameters (required / immutable-default / trailing block), and the callback —
//! then consumes with `doodle_registry_add_foreign`. Opaque + builder functions so the surface
//! grows additively (freeze convention 2). Defaults are **immutable only** (D-M7-8): built from
//! `ConstValue`, which has no list/dict/record variant, so a mutable default is unrepresentable.
//! A string default is UTF-8-validated + NFC-normalized here — the C ABI is an untrusted
//! injection point, so it upholds the `ConstValue::Str`-is-NFC contract the way `make_string`
//! does (a float default's NaN is canonicalized when the recipe materializes, S-28).

use crate::abi::{self, DoodleBodyKind, DoodleStatus};
use crate::call::{DoodleForeignFn, SendPtr, trampoline};
use crate::guard::catch;
use doodle_core::machine::{ConstValue, ForeignBuilder, Intrinsic};
use doodle_core::resolve::BodyKind;
use std::ffi::c_void;

/// A parameter of a foreign descriptor under construction (L§8.2/§8.3).
enum ParamSpec {
    /// A required ordinary parameter.
    Required(Box<str>),
    /// An ordinary parameter with an immutable default (materialized per call, D-M7-8).
    Default(Box<str>, ConstValue),
    /// The trailing block parameter (invoked reentrantly).
    Block(Box<str>),
}

/// A host foreign-function descriptor under construction (E§5.1). Built with
/// `doodle_foreign_desc_new`, filled with the `doodle_foreign_desc_*` builders, then
/// **consumed** by `doodle_registry_add_foreign` (or discarded with `doodle_foreign_desc_free`).
/// Opaque to C.
pub struct DoodleForeignDesc {
    name: Box<str>,
    kind: BodyKind,
    params: Vec<ParamSpec>,
    callback: Option<(DoodleForeignFn, SendPtr)>,
}

impl DoodleForeignDesc {
    /// Consumes the descriptor into an engine [`Intrinsic`] with a host-callback body, or
    /// `ErrContract` if no callback was set (`doodle_foreign_desc_set_callback`). The parameter
    /// order is the order the builders added them.
    pub(crate) fn into_intrinsic(self) -> Result<Intrinsic, DoodleStatus> {
        let Some((callback, user_data)) = self.callback else {
            return Err(DoodleStatus::ErrContract);
        };
        let mut builder = ForeignBuilder::new(self.name, self.kind);
        for param in self.params {
            builder = match param {
                ParamSpec::Required(name) => builder.param(name),
                ParamSpec::Default(name, value) => builder.default_param(name, value),
                ParamSpec::Block(name) => builder.block_param(name),
            };
        }
        Ok(builder.host(trampoline(callback, user_data)))
    }
}

/// Reads a `(ptr, len)` UTF-8 parameter name into an owned string, or an error status: NULL
/// (with a non-zero length) is `ErrNullPointer`; non-UTF-8 is `ErrInvalidUtf8`. `len == 0` is
/// the empty name.
fn name_arg(ptr: *const u8, len: usize) -> Result<Box<str>, DoodleStatus> {
    if len == 0 {
        return Ok(Box::from(""));
    }
    if ptr.is_null() {
        return Err(DoodleStatus::ErrNullPointer);
    }
    // SAFETY: `len != 0` and `ptr` is non-null (checked), so by the caller's contract it points
    // to `len` readable bytes; the slice is copied into an owned string, not held.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(Box::from(text)),
        Err(_) => Err(DoodleStatus::ErrInvalidUtf8),
    }
}

/// Reads a `(ptr, len)` byte-value argument as a borrowed slice, or an error status: NULL (with
/// a non-zero length) is `ErrNullPointer`. `len == 0` is the empty slice.
fn bytes_arg<'a>(ptr: *const u8, len: usize) -> Result<&'a [u8], DoodleStatus> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(DoodleStatus::ErrNullPointer);
    }
    // SAFETY: `len != 0` and `ptr` is non-null (checked), so by the caller's contract it points
    // to `len` readable bytes; the slice's use does not outlive the call that reads it.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Borrows the descriptor behind a raw pointer, or `None` if NULL.
fn desc_mut<'a>(desc: *mut DoodleForeignDesc) -> Option<&'a mut DoodleForeignDesc> {
    // SAFETY: `as_mut` returns None for NULL; a non-null `desc` is a live `DoodleForeignDesc`
    // from `doodle_foreign_desc_new` (not consumed/freed) by the caller's contract.
    unsafe { desc.as_mut() }
}

/// Appends a parameter to `desc`, reading its name: the shared body of the param builders.
fn push_param(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
    make: impl FnOnce(Box<str>) -> ParamSpec,
) -> DoodleStatus {
    let Some(desc) = desc_mut(desc) else {
        return DoodleStatus::ErrNullPointer;
    };
    match name_arg(name, name_len) {
        Ok(name) => {
            desc.params.push(make(name));
            DoodleStatus::Ok
        }
        Err(status) => status,
    }
}

/// Creates a foreign-function descriptor for a function named `name` (UTF-8) of `kind`. Returns
/// NULL on allocation failure, a NULL name (with a non-zero length), or a non-UTF-8 name.
/// Populate it with the `doodle_foreign_desc_*` builders (in the parameter order wanted), then
/// pass it to `doodle_registry_add_foreign` (which consumes it) or free it with
/// `doodle_foreign_desc_free`.
///
/// # Safety
/// `name` must point to `name_len` readable bytes (or be NULL with `name_len` 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_new(
    name: *const u8,
    name_len: usize,
    kind: DoodleBodyKind,
) -> *mut DoodleForeignDesc {
    let Ok(name) = name_arg(name, name_len) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(DoodleForeignDesc {
        name,
        kind: abi::body_kind(kind),
        params: Vec::new(),
        callback: None,
    }))
}

/// Appends a **required** ordinary parameter `name` (L§8.3).
///
/// # Safety
/// `desc` a live descriptor; `name` points to `name_len` readable bytes (or NULL with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_param(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
) -> DoodleStatus {
    catch(|| push_param(desc, name, name_len, ParamSpec::Required))
}

/// Appends the **trailing block** parameter `name` (L§8.2): a `do … end` the callback invokes
/// reentrantly (`doodle_call_block`).
///
/// # Safety
/// `desc` a live descriptor; `name` points to `name_len` readable bytes (or NULL with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_block_param(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
) -> DoodleStatus {
    catch(|| push_param(desc, name, name_len, ParamSpec::Block))
}

/// Appends an ordinary parameter `name` with an integer default (L§8.3, D-M7-8).
///
/// # Safety
/// `desc` a live descriptor; `name` points to `name_len` readable bytes (or NULL with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_default_int(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
    value: i64,
) -> DoodleStatus {
    catch(|| {
        push_param(desc, name, name_len, |n| {
            ParamSpec::Default(n, ConstValue::Int(value))
        })
    })
}

/// Appends an ordinary parameter `name` with a float default (L§8.3, D-M7-8). Any NaN is
/// canonicalized when the recipe materializes (S-28).
///
/// # Safety
/// `desc` a live descriptor; `name` points to `name_len` readable bytes (or NULL with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_default_float(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
    value: f64,
) -> DoodleStatus {
    catch(|| {
        push_param(desc, name, name_len, |n| {
            ParamSpec::Default(n, ConstValue::Float(value))
        })
    })
}

/// Appends an ordinary parameter `name` with a boolean default (L§8.3, D-M7-8).
///
/// # Safety
/// `desc` a live descriptor; `name` points to `name_len` readable bytes (or NULL with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_default_bool(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
    value: bool,
) -> DoodleStatus {
    catch(|| {
        push_param(desc, name, name_len, |n| {
            ParamSpec::Default(n, ConstValue::Bool(value))
        })
    })
}

/// Appends an ordinary parameter `name` with a `nil` default (L§8.3, D-M7-8).
///
/// # Safety
/// `desc` a live descriptor; `name` points to `name_len` readable bytes (or NULL with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_default_nil(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
) -> DoodleStatus {
    catch(|| {
        push_param(desc, name, name_len, |n| {
            ParamSpec::Default(n, ConstValue::Nil)
        })
    })
}

/// Appends an ordinary parameter `name` with a **string** default (L§8.3, D-M7-8) from UTF-8
/// `value` bytes: validated (`ErrInvalidUtf8`) and NFC-normalized (the untrusted-boundary
/// counterpart of `make_string`, upholding the NFC contract for `ConstValue::Str`).
///
/// # Safety
/// `desc` a live descriptor; `name`/`value` point to their `*_len` readable bytes (NULL allowed
/// only with a 0 length).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_default_string(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
    value: *const u8,
    value_len: usize,
) -> DoodleStatus {
    catch(|| {
        let bytes = match bytes_arg(value, value_len) {
            Ok(bytes) => bytes,
            Err(status) => return status,
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return DoodleStatus::ErrInvalidUtf8;
        };
        let nfc = doodle_core::unicode::nfc(text)
            .into_owned()
            .into_boxed_str();
        push_param(desc, name, name_len, |n| {
            ParamSpec::Default(n, ConstValue::Str(nfc))
        })
    })
}

/// Appends an ordinary parameter `name` with a **byte-string** default (L§8.3, D-M7-8) from the
/// raw `value` bytes (no encoding or normalization).
///
/// # Safety
/// `desc` a live descriptor; `name`/`value` point to their `*_len` readable bytes (NULL allowed
/// only with a 0 length).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_default_bytes(
    desc: *mut DoodleForeignDesc,
    name: *const u8,
    name_len: usize,
    value: *const u8,
    value_len: usize,
) -> DoodleStatus {
    catch(|| {
        let bytes = match bytes_arg(value, value_len) {
            Ok(bytes) => Box::<[u8]>::from(bytes),
            Err(status) => return status,
        };
        push_param(desc, name, name_len, |n| {
            ParamSpec::Default(n, ConstValue::Bytes(bytes))
        })
    })
}

/// Sets the descriptor's callback (E§5.2): the C function the engine runs when the foreign
/// function is called, and an opaque `user_data` passed to it verbatim. A foreign function must
/// have a callback (else `doodle_registry_add_foreign` returns `ErrContract`).
///
/// # Safety
/// `desc` a live descriptor; `callback` a valid, non-NULL function pointer; `user_data` is
/// opaque (never dereferenced by the engine).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_set_callback(
    desc: *mut DoodleForeignDesc,
    callback: DoodleForeignFn,
    user_data: *mut c_void,
) -> DoodleStatus {
    catch(|| match desc_mut(desc) {
        Some(desc) => {
            desc.callback = Some((callback, SendPtr(user_data)));
            DoodleStatus::Ok
        }
        None => DoodleStatus::ErrNullPointer,
    })
}

/// Frees a descriptor from `doodle_foreign_desc_new` that was **not** consumed by
/// `doodle_registry_add_foreign`. NULL is a no-op. Do not call this on a descriptor already
/// passed to `doodle_registry_add_foreign` (that consumed it).
///
/// # Safety
/// `desc` must be a pointer from `doodle_foreign_desc_new` not already freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn doodle_foreign_desc_free(desc: *mut DoodleForeignDesc) {
    if !desc.is_null() {
        // SAFETY: non-null (checked); a `Box::into_raw` pointer from `doodle_foreign_desc_new`,
        // not consumed/freed, by the caller's contract.
        drop(unsafe { Box::from_raw(desc) });
    }
}
