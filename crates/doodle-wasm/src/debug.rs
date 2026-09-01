//! The `#[wasm_bindgen]` marshaling for the debug observation + inspection surface (engine
//! spec E§8 / §4.4): a second `impl DoodleInstance` block that forwards to
//! [`facade::Session`](crate::facade::Session)'s plain-Rust debug/inspect methods and shapes
//! the structured reads (the stack walk, per-frame bindings, breakpoints, the aux-eval outcome)
//! into **plain JS objects/arrays** via `js_sys` — no serializer, so no wasm-size cost beyond
//! the bindings used. Split from `lib.rs` for length.
//!
//! **Object shapes are API** (a rider on D-M6-3): a JS host reads these by field name, and the
//! matching TypeScript contract is pinned in `@doodle-lang/engine`'s `.d.ts`. Field names use
//! E§8.2/§8.3 vocabulary — `callable`, `callSite`, `tailCount`, `elided`. A frame is entirely
//! GC-owned data (nothing to `free`); only the value handles minted by `frameLocal`/
//! `frameDynamic` (and the inspection readers) are host-owned and must be released.

use wasm_bindgen::prelude::*;

use crate::DoodleInstance;
use crate::facade::{AuxOutcomeData, CallableInfo, FrameData};
use crate::value_error;
use doodle_core::machine::Handle;

#[wasm_bindgen]
impl DoodleInstance {
    // --- debug setup (E§8.6/§8.7/§8.8) ---

    /// The entry module's canonical id (E§3.2) — pass it to
    /// [`setBreakpoint`](DoodleInstance::set_breakpoint) to address the user's program.
    #[wasm_bindgen(js_name = entryModule)]
    pub fn entry_module(&self) -> String {
        self.session.entry_module().to_string()
    }

    /// Sets a breakpoint at (`canonical`, 1-based `line`) and returns its id (E§8.6). An
    /// unloaded canonical or a line past the last statement is pending, never an error.
    #[wasm_bindgen(js_name = setBreakpoint)]
    pub fn set_breakpoint(&mut self, canonical: &str, line: u32) -> u32 {
        self.session.set_breakpoint(canonical, line)
    }

    /// Clears the breakpoint `id` (E§8.6); idempotent.
    #[wasm_bindgen(js_name = clearBreakpoint)]
    pub fn clear_breakpoint(&mut self, id: u32) {
        self.session.clear_breakpoint(id);
    }

    /// The installed breakpoints (E§8.6) as `{id, canonicalId, line, resolved}` objects, in
    /// set order — `resolved: false` marks a pending (unhittable) gutter mark.
    #[wasm_bindgen(js_name = breakpoints)]
    pub fn breakpoints(&self) -> js_sys::Array {
        let arr = js_sys::Array::new();
        for bp in self.session.breakpoints() {
            let obj = js_sys::Object::new();
            set(&obj, "id", &JsValue::from(bp.id.0));
            set(&obj, "canonicalId", &JsValue::from_str(&bp.canonical_id));
            set(&obj, "line", &JsValue::from(bp.line));
            set(&obj, "resolved", &JsValue::from_bool(bp.resolved));
            arr.push(&obj.into());
        }
        arr
    }

    /// Enables or disables raise-trapping (E§8.7).
    #[wasm_bindgen(js_name = setRaiseTrapping)]
    pub fn set_raise_trapping(&mut self, enabled: bool) {
        self.session.set_raise_trapping(enabled);
    }

    /// Sets the observation granularity (E§8.8/S-62): `true` = per-subexpression (watch-it-run),
    /// `false` = per-statement (default).
    #[wasm_bindgen(js_name = setObservationMode)]
    pub fn set_observation_mode(&mut self, subexpr: bool) {
        self.session.set_observation_mode(subexpr);
    }

    // --- observation reads (E§8.1/§8.2/§8.3/§8.4) ---

    /// The current pause generation — the token a [`stackWalk`](DoodleInstance::stack_walk)'s
    /// frame indices are valid for. Any `drive`/`resolve` bumps it.
    #[wasm_bindgen(js_name = pauseGeneration)]
    pub fn pause_generation(&self) -> u32 {
        self.session.pause_generation()
    }

    /// The call stack (E§8.2) followed by the tail-elided history (E§8.3), as
    /// `{generation, frames}`. Each frame is plain GC-owned data:
    /// `{callable?: {name?, isFunction?, declSpan?}, callSite?, tailCount, locals, dynamics,
    /// elided?}`. Nothing here needs `release`; expand a binding value with
    /// [`frameLocal`](DoodleInstance::frame_local)/[`frameDynamic`](DoodleInstance::frame_dynamic)
    /// carrying `generation`.
    #[wasm_bindgen(js_name = stackWalk)]
    pub fn stack_walk(&mut self) -> js_sys::Object {
        let generation = self.session.pause_generation();
        let frames = self.session.stack_walk();
        let arr = js_sys::Array::new();
        for frame in &frames {
            arr.push(&frame_value(frame));
        }
        let obj = js_sys::Object::new();
        set(&obj, "generation", &JsValue::from(generation));
        set(&obj, "frames", &arr.into());
        obj
    }

    /// A fresh **host-owned** handle (release it) to frame `frame`'s `slot`-th local value
    /// (§8.2), or `undefined` for an out-of-range/uninitialized slot. Throws if `generation`
    /// is stale (the stack advanced since the walk).
    #[wasm_bindgen(js_name = frameLocal)]
    pub fn frame_local(
        &mut self,
        generation: u32,
        frame: usize,
        slot: usize,
    ) -> Result<Option<u64>, JsError> {
        self.session
            .frame_local(generation, frame, slot)
            .map(|opt| opt.map(Handle::bits))
            .map_err(stale_generation)
    }

    /// A fresh **host-owned** handle (release it) to frame `frame`'s `slot`-th `with` binding
    /// value (§8.2), or `undefined` if out of range/unbound. Throws on a stale `generation`.
    #[wasm_bindgen(js_name = frameDynamic)]
    pub fn frame_dynamic(
        &mut self,
        generation: u32,
        frame: usize,
        slot: usize,
    ) -> Result<Option<u64>, JsError> {
        self.session
            .frame_dynamic(generation, frame, slot)
            .map(|opt| opt.map(Handle::bits))
            .map_err(stale_generation)
    }

    /// The `[start, end)` span of the subexpression just completed at a **fine** safe point
    /// (E§7.4/§8.4), whose value is in the result register; `undefined` at a statement stop.
    #[wasm_bindgen(js_name = completedSpan)]
    pub fn completed_span(&self) -> Option<Vec<u32>> {
        self.session
            .completed_position()
            .map(|s| vec![s.start, s.end])
    }

    /// At a raise-trap pause (E§8.7), a fresh **host-owned** handle (release it) to the raised
    /// value; `undefined` if no raise is trapped. Consuming it marks the trap taken.
    #[wasm_bindgen(js_name = trappedRaise)]
    pub fn trapped_raise(&mut self) -> Option<u64> {
        self.session.trapped_raise().map(|h| h.bits())
    }

    /// At a raise-trap pause, the `[start, end)` span of the raise site (E§8.7); `undefined` if
    /// no raise is trapped.
    #[wasm_bindgen(js_name = trappedRaiseSpan)]
    pub fn trapped_raise_span(&self) -> Option<Vec<u32>> {
        self.session
            .trapped_raise_position()
            .map(|s| vec![s.start, s.end])
    }

    /// Host-driven `to_string` on `handle` at a paused instance (E§8.4/S-22) — see
    /// [`Session::eval_to_string`](crate::facade::Session::eval_to_string). Returns
    /// `{kind: "rendered"|"raised", value: handle}` or `{kind: "faulted", fault}`; a
    /// `rendered`/`raised` `value` is a fresh **host-owned** handle to release.
    #[wasm_bindgen(js_name = evalToString)]
    pub fn eval_to_string(&mut self, handle: u64, fuel: u64) -> js_sys::Object {
        let obj = js_sys::Object::new();
        match self.session.eval_to_string(Handle::from_bits(handle), fuel) {
            AuxOutcomeData::Rendered(h) => {
                set(&obj, "kind", &JsValue::from_str("rendered"));
                set(&obj, "value", &JsValue::from(h.bits()));
            }
            AuxOutcomeData::Raised(h) => {
                set(&obj, "kind", &JsValue::from_str("raised"));
                set(&obj, "value", &JsValue::from(h.bits()));
            }
            AuxOutcomeData::Faulted(tag) => {
                set(&obj, "kind", &JsValue::from_str("faulted"));
                set(&obj, "fault", &JsValue::from_str(tag));
            }
        }
        obj
    }

    // --- structural value inspection (E§4.4/§8.4), flat 1:1 with the native API ---

    /// A record's declared type name.
    #[wasm_bindgen(js_name = recordTypeName)]
    pub fn record_type_name(&self, handle: u64) -> Result<String, JsError> {
        self.session
            .record_type_name(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// A record's field count.
    #[wasm_bindgen(js_name = recordLength)]
    pub fn record_length(&self, handle: u64) -> Result<usize, JsError> {
        self.session
            .record_length(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// A record's `index`-th field name (declaration order).
    #[wasm_bindgen(js_name = recordFieldName)]
    pub fn record_field_name(&self, handle: u64, index: usize) -> Result<String, JsError> {
        self.session
            .record_field_name(Handle::from_bits(handle), index)
            .map_err(value_error)
    }
    /// A fresh host-owned handle to a record's `index`-th field value.
    #[wasm_bindgen(js_name = recordField)]
    pub fn record_field(&mut self, handle: u64, index: usize) -> Result<u64, JsError> {
        self.session
            .record_field(Handle::from_bits(handle), index)
            .map(|h| h.bits())
            .map_err(value_error)
    }
    /// A dict's entry count.
    #[wasm_bindgen(js_name = dictLength)]
    pub fn dict_length(&self, handle: u64) -> Result<usize, JsError> {
        self.session
            .dict_length(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// A fresh host-owned handle to a dict's `index`-th key (insertion order, L§4.7).
    #[wasm_bindgen(js_name = dictKey)]
    pub fn dict_key(&mut self, handle: u64, index: usize) -> Result<u64, JsError> {
        self.session
            .dict_key(Handle::from_bits(handle), index)
            .map(|h| h.bits())
            .map_err(value_error)
    }
    /// A fresh host-owned handle to a dict's `index`-th value (insertion order, L§4.7).
    #[wasm_bindgen(js_name = dictValue)]
    pub fn dict_value(&mut self, handle: u64, index: usize) -> Result<u64, JsError> {
        self.session
            .dict_value(Handle::from_bits(handle), index)
            .map(|h| h.bits())
            .map_err(value_error)
    }
    /// A list's length.
    #[wasm_bindgen(js_name = listLength)]
    pub fn list_length(&self, handle: u64) -> Result<usize, JsError> {
        self.session
            .list_length(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// A fresh host-owned handle to a list's `index`-th element.
    #[wasm_bindgen(js_name = listGet)]
    pub fn list_get(&mut self, handle: u64, index: usize) -> Result<u64, JsError> {
        self.session
            .list_get(Handle::from_bits(handle), index)
            .map(|h| h.bits())
            .map_err(value_error)
    }
    /// A callable's declared name, or `undefined` for an anonymous/sourceless callable.
    #[wasm_bindgen(js_name = callableName)]
    pub fn callable_name(&self, handle: u64) -> Result<Option<String>, JsError> {
        self.session
            .callable_name(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// Whether a callable is a `fn` (`true`) or `to` (`false`); `undefined` if indeterminate.
    #[wasm_bindgen(js_name = callableIsFunction)]
    pub fn callable_is_function(&self, handle: u64) -> Result<Option<bool>, JsError> {
        self.session
            .callable_is_function(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// The `[start, end)` span of a callable's declaration, or `undefined` for a sourceless one.
    #[wasm_bindgen(js_name = callablePosition)]
    pub fn callable_position(&self, handle: u64) -> Result<Option<Vec<u32>>, JsError> {
        self.session
            .callable_position(Handle::from_bits(handle))
            .map(|opt| opt.map(|p| vec![p[0], p[1]]))
            .map_err(value_error)
    }
    /// The `[start, end)` span of a callable's docstring, or `undefined` if it has none.
    #[wasm_bindgen(js_name = callableDocstring)]
    pub fn callable_docstring(&self, handle: u64) -> Result<Option<Vec<u32>>, JsError> {
        self.session
            .callable_docstring(Handle::from_bits(handle))
            .map(|opt| opt.map(|p| vec![p[0], p[1]]))
            .map_err(value_error)
    }
    /// A type value's name.
    #[wasm_bindgen(js_name = typeName)]
    pub fn type_name(&self, handle: u64) -> Result<String, JsError> {
        self.session
            .type_name(Handle::from_bits(handle))
            .map_err(value_error)
    }
    /// A module value's member names (declaration order).
    #[wasm_bindgen(js_name = moduleMemberNames)]
    pub fn module_member_names(&self, handle: u64) -> Result<Vec<String>, JsError> {
        self.session
            .module_member_names(Handle::from_bits(handle))
            .map_err(value_error)
    }
}

/// Builds the JS object for one [`FrameData`] entry of the stack-walk transcript.
fn frame_value(frame: &FrameData) -> JsValue {
    let obj = js_sys::Object::new();
    if let Some(info) = &frame.callable {
        set(&obj, "callable", &callable_info_value(info));
    }
    if let Some(span) = frame.call_site {
        set(&obj, "callSite", &span_value(span));
    }
    set(
        &obj,
        "tailCount",
        &JsValue::from_f64(frame.tail_count as f64),
    );
    set(&obj, "locals", &str_array(&frame.locals));
    set(&obj, "dynamics", &str_array(&frame.dynamics));
    if frame.elided {
        set(&obj, "elided", &JsValue::TRUE);
    }
    obj.into()
}

/// Builds the JS object for a frame's callable reflection ([`CallableInfo`]).
fn callable_info_value(info: &CallableInfo) -> JsValue {
    let obj = js_sys::Object::new();
    if let Some(name) = &info.name {
        set(&obj, "name", &JsValue::from_str(name));
    }
    if let Some(is_function) = info.is_function {
        set(&obj, "isFunction", &JsValue::from_bool(is_function));
    }
    if let Some(span) = info.decl_span {
        set(&obj, "declSpan", &span_value(span));
    }
    obj.into()
}

/// A `[start, end)` span as a JS `[number, number]` array.
fn span_value(pair: [u32; 2]) -> JsValue {
    let arr = js_sys::Array::new();
    arr.push(&JsValue::from(pair[0]));
    arr.push(&JsValue::from(pair[1]));
    arr.into()
}

/// A `&[String]` as a JS array of strings.
fn str_array(items: &[String]) -> JsValue {
    let arr = js_sys::Array::new();
    for s in items {
        arr.push(&JsValue::from_str(s));
    }
    arr.into()
}

/// Sets `obj[key] = value` on a plain JS object (infallible for a fresh object).
fn set(obj: &js_sys::Object, key: &str, value: &JsValue) {
    let _ = js_sys::Reflect::set(obj, &JsValue::from_str(key), value);
}

/// Maps a stale-generation frame read to a thrown `JsError` (E§8.2, the pull model's guard).
fn stale_generation(_: crate::facade::StaleGeneration) -> JsError {
    JsError::new("stale pause generation: the stack advanced since stackWalk() — re-walk")
}
