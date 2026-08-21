//! The Doodle engine's WebAssembly facade (engine spec E§3–§8), built with wasm-bindgen.
//!
//! A **thin marshaling shell** over [`facade::Session`] (the native-testable core): it
//! exposes an opaque [`DoodleInstance`] JS class — construct from source, drive in fuel
//! slices, resolve capabilities, read the handle boundary, output, and executing
//! position. Values cross as **opaque handle ids** (a `Handle`'s `u64` bits, a `BigInt`
//! in JS), never raw pointers. The pump, capability handlers, and the demo live in
//! `doodle-web` (M3.5); this crate stays pure Rust (Decision #1).
//!
//! **Errors.** Expected failures — a program that fails to load, a stale/wrong-kind
//! handle — return a thrown `JsError`. In the **shipped release build** (what the size
//! script builds; `wasm32` is `panic = "abort"`) doodle-core reports host-contract misuse
//! as a terminal `Faulted(Internal)` outcome rather than a panic, so the drive surface
//! does not trap on misuse — a re-drive of a terminal instance or a resolve of a
//! non-suspended one faults instead. (In a *debug* build those two cases hit a
//! `debug_assert!` first and panic; a genuine engine panic — a bug — aborts either way.)

use wasm_bindgen::prelude::*;

use doodle_core::machine::Handle;
use facade::{DriveOutcome, Session};

mod facade;

/// Returns the version of the underlying doodle-core engine.
#[wasm_bindgen]
pub fn version() -> String {
    doodle_core::version().to_string()
}

/// A running Doodle program behind the wasm boundary (engine spec E§3): an opaque handle
/// to a [`Session`]. Constructed with [`demo`](DoodleInstance::demo) (print-only, for
/// conformance parity) or [`turtle`](DoodleInstance::turtle) (the turtle library +
/// platform primitives).
#[wasm_bindgen]
pub struct DoodleInstance {
    session: Session,
}

#[wasm_bindgen]
impl DoodleInstance {
    /// Loads `source` with only `print` registered and nothing prepended — matching the
    /// native conformance runner (E§11 parity). Throws on a load error.
    pub fn demo(source: &str) -> Result<DoodleInstance, JsError> {
        Session::demo(source)
            .map(|session| DoodleInstance { session })
            .map_err(|e| JsError::new(&e.message))
    }

    /// Loads `source` with the turtle library prepended and the turtle natives registered
    /// (`print`/`sin`/`cos` + `draw_line`/`set_turtle`/`clear_canvas`). Throws on a load
    /// error.
    pub fn turtle(source: &str) -> Result<DoodleInstance, JsError> {
        Session::turtle(source)
            .map(|session| DoodleInstance { session })
            .map_err(|e| JsError::new(&e.message))
    }

    /// Drives at most `fuel` statement safe points (`undefined` = unbounded, S-40),
    /// starting or resuming from a `Paused`. Returns the [`DriveResult`].
    pub fn drive(&mut self, fuel: Option<u64>) -> DriveResult {
        DriveResult {
            outcome: self.session.drive(fuel),
        }
    }

    /// Resolves a pending capability with the value named by handle `value` (or raises it
    /// at the call site when `raise` is true), then resumes for at most `fuel` safe
    /// points (E§7.5).
    pub fn resolve(&mut self, value: u64, raise: bool, fuel: Option<u64>) -> DriveResult {
        DriveResult {
            outcome: self.session.resolve(Handle::from_bits(value), raise, fuel),
        }
    }

    /// Requests cancellation (E§10.1) — the stop button, taking effect at the next safe
    /// point (between slices in the single-threaded pump).
    pub fn cancel(&self) {
        self.session.cancel();
    }

    /// The captured `print` output so far, as raw (NFC UTF-8) bytes.
    pub fn output(&self) -> Vec<u8> {
        self.session.output().to_vec()
    }

    /// The `[start, end)` byte span of the currently-executing statement in the module
    /// source (E§8.1), or `undefined` at a boundary. Offsets index the full module;
    /// subtract [`preludeBytes`](DoodleInstance::prelude_bytes) for a program-relative
    /// offset, and map bytes→line against [`source`](DoodleInstance::source).
    #[wasm_bindgen(js_name = currentSpan)]
    pub fn current_span(&self) -> Option<Vec<u32>> {
        self.session
            .current_position()
            .map(|span| vec![span.start, span.end])
    }

    /// Byte length of the prepended prelude (0 for the demo config).
    #[wasm_bindgen(js_name = preludeBytes)]
    pub fn prelude_bytes(&self) -> u32 {
        self.session.prelude_bytes()
    }

    /// The full module source actually loaded (prelude + program), for span→line mapping.
    pub fn source(&self) -> String {
        self.session.source().to_string()
    }

    // --- the handle boundary (E§4). Handles are `u64` bits (a JS `BigInt`). ---

    /// Interns an `Int` value, returning its host-owned handle bits.
    #[wasm_bindgen(js_name = makeInt)]
    pub fn make_int(&mut self, value: i64) -> u64 {
        self.session.make_int(value).bits()
    }
    /// Interns a `Float` value (NaN canonicalized), returning its handle bits.
    #[wasm_bindgen(js_name = makeFloat)]
    pub fn make_float(&mut self, value: f64) -> u64 {
        self.session.make_float(value).bits()
    }
    /// Interns a `Bool` value, returning its handle bits.
    #[wasm_bindgen(js_name = makeBool)]
    pub fn make_bool(&mut self, value: bool) -> u64 {
        self.session.make_bool(value).bits()
    }
    /// Interns `nil`, returning its handle bits (e.g. to resolve a `to` capability).
    #[wasm_bindgen(js_name = makeNil)]
    pub fn make_nil(&mut self) -> u64 {
        self.session.make_nil().bits()
    }
    /// Interns a `String` (validated + NFC-normalized), returning its handle bits.
    #[wasm_bindgen(js_name = makeString)]
    pub fn make_string(&mut self, text: &str) -> Result<u64, JsError> {
        self.session
            .make_string(text.as_bytes())
            .map(|h| h.bits())
            .map_err(value_error)
    }
    /// Reads an `Int` handle; throws on a stale handle or a non-integer/bignum value.
    #[wasm_bindgen(js_name = asInt)]
    pub fn as_int(&self, handle: u64) -> Result<i64, JsError> {
        self.session
            .as_int(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// Reads a `Float` handle; throws on a stale handle or a non-float value.
    #[wasm_bindgen(js_name = asFloat)]
    pub fn as_float(&self, handle: u64) -> Result<f64, JsError> {
        self.session
            .as_float(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// Reads a `Bool` handle; throws on a stale handle or a non-boolean value.
    #[wasm_bindgen(js_name = asBool)]
    pub fn as_bool(&self, handle: u64) -> Result<bool, JsError> {
        self.session
            .as_bool(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// Whether the handle names `nil`; throws only on a stale handle.
    #[wasm_bindgen(js_name = isNil)]
    pub fn is_nil(&self, handle: u64) -> Result<bool, JsError> {
        self.session
            .is_nil(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// Reads a `String` handle's NFC UTF-8 bytes; throws on a stale/non-string handle.
    #[wasm_bindgen(js_name = stringBytes)]
    pub fn string_bytes(&self, handle: u64) -> Result<Vec<u8>, JsError> {
        self.session
            .string_bytes(Handle::from_bits(handle))
            .map(<[u8]>::to_vec)
            .map_err(value_error)
    }
    /// The value's [`Kind`](doodle_core::machine::Kind), kebab-cased (`"int"`, `"float"`,
    /// `"string"`, `"nil"`, …).
    #[wasm_bindgen(js_name = kindOf)]
    pub fn kind_of(&self, handle: u64) -> Result<String, JsError> {
        self.session
            .kind_of(Handle::from_bits(handle))
            .map(|k| kind_tag(k).to_string())
            .map_err(value_error)
    }
    /// Releases a host-owned handle (a capability argument or a value made here). A
    /// capability request's argument handles must each be released (S-17).
    pub fn release(&mut self, handle: u64) -> Result<(), JsError> {
        self.session
            .release(Handle::from_bits(handle))
            .map_err(|e| JsError::new(&format!("{e:?}")))
    }
}

/// The outcome of a [`drive`](DoodleInstance::drive)/[`resolve`](DoodleInstance::resolve)
/// (engine spec E§7.2), read from JS through its getters. `kind` selects which payload
/// getters are meaningful.
#[wasm_bindgen]
pub struct DriveResult {
    outcome: DriveOutcome,
}

#[wasm_bindgen]
impl DriveResult {
    /// `"completed" | "suspended" | "paused" | "raised" | "faulted"`.
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> String {
        match &self.outcome {
            DriveOutcome::Completed => "completed",
            DriveOutcome::Suspended { .. } => "suspended",
            DriveOutcome::Paused(_) => "paused",
            DriveOutcome::Raised { .. } => "raised",
            DriveOutcome::Faulted(_) => "faulted",
        }
        .to_string()
    }

    /// The suspended capability's registry id (`suspended` only).
    #[wasm_bindgen(getter)]
    pub fn capability(&self) -> Option<u32> {
        match &self.outcome {
            DriveOutcome::Suspended { capability, .. } => Some(*capability),
            _ => None,
        }
    }

    /// The suspended capability's bound-argument handles (`suspended` only, `null`
    /// otherwise — matching the other payload getters, so an empty array is
    /// unambiguously a zero-argument suspend) — **host owned**, release each after
    /// reading (S-17).
    #[wasm_bindgen(getter)]
    pub fn args(&self) -> Option<Vec<u64>> {
        match &self.outcome {
            DriveOutcome::Suspended { args, .. } => Some(args.iter().map(|h| h.bits()).collect()),
            _ => None,
        }
    }

    /// The pause reason (`paused` only): `"slice-end"`, `"step"`, …
    #[wasm_bindgen(getter, js_name = pauseReason)]
    pub fn pause_reason(&self) -> Option<String> {
        match &self.outcome {
            DriveOutcome::Paused(reason) => Some((*reason).to_string()),
            _ => None,
        }
    }

    /// The engine-fault tag (`faulted` only): `"limit:step-budget"`, `"cancelled"`,
    /// `"nested-suspend"`, `"internal"`, …
    #[wasm_bindgen(getter)]
    pub fn fault(&self) -> Option<String> {
        match &self.outcome {
            DriveOutcome::Faulted(tag) => Some((*tag).to_string()),
            _ => None,
        }
    }

    /// The raised exception's kind (`raised` only), kebab-cased.
    #[wasm_bindgen(getter, js_name = exceptionKind)]
    pub fn exception_kind(&self) -> Option<String> {
        match &self.outcome {
            DriveOutcome::Raised { kind, .. } => Some((*kind).to_string()),
            _ => None,
        }
    }

    /// The raised exception's message (`raised` only).
    #[wasm_bindgen(getter)]
    pub fn message(&self) -> Option<String> {
        match &self.outcome {
            DriveOutcome::Raised { message, .. } => Some(message.clone()),
            _ => None,
        }
    }

    /// The raising site's `[start, end)` byte span in the module source (`raised` with a
    /// known site only).
    #[wasm_bindgen(getter, js_name = raiseSpan)]
    pub fn raise_span(&self) -> Option<Vec<u32>> {
        match &self.outcome {
            DriveOutcome::Raised {
                span: Some(span), ..
            } => Some(vec![span.start, span.end]),
            _ => None,
        }
    }
}

/// Maps a [`ValueError`](doodle_core::machine::ValueError) to a thrown `JsError`.
fn value_error(e: doodle_core::machine::ValueError) -> JsError {
    JsError::new(&format!("{e:?}"))
}

/// The kebab-case tag for a value [`Kind`](doodle_core::machine::Kind).
fn kind_tag(kind: doodle_core::machine::Kind) -> &'static str {
    use doodle_core::machine::Kind;
    match kind {
        Kind::Nil => "nil",
        Kind::Bool => "bool",
        Kind::Int => "int",
        Kind::Float => "float",
        Kind::String => "string",
        Kind::Bytes => "bytes",
        Kind::List => "list",
        Kind::Dict => "dict",
        Kind::Record => "record",
        Kind::Callable => "callable",
        Kind::Module => "module",
        Kind::Type => "type",
        Kind::Foreign => "foreign",
    }
}
