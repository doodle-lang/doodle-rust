//! The debug observation surface (engine spec E§8) on the [`Session`], in **plain Rust
//! types** (no `JsValue`) so it is exercised by ordinary `cargo test`; `lib.rs` marshals
//! these into plain JS objects. It bridges the pull-observation reads the browser debugger
//! needs — breakpoints (§8.6), raise-trap (§8.7), observation mode (§8.8), the call-stack
//! walk + per-frame bindings (§8.2/§8.3), the trapped raise, and auxiliary evaluation
//! (§8.4/S-22) — over a paused instance.
//!
//! **Handle discipline (E§4).** The stack walk reflects each callable into *plain data* and
//! frees the handle it mints, and lists binding **names** only — so a frame is data the JS GC
//! owns, with nothing to `release`. Binding **values** are minted lazily, one at a time, by
//! [`frame_local`](Session::frame_local)/[`frame_dynamic`](Session::frame_dynamic) — the only
//! debug reads that hand the host a real handle to release.

use super::{Session, fault_tag};
use doodle_core::drive::{BreakpointId, ObservationMode};
use doodle_core::machine::{AuxOutcome, BreakpointInfo, Handle};
use doodle_core::resolve::GlobalKind;
use doodle_core::span::Span;

/// The reflection of a call frame's callable (E§8.2), as plain data the JS GC owns — the stack
/// panel's label without minting a handle. `None` fields where the callable has no source
/// declaration (an intrinsic or a protocol dispatcher).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallableInfo {
    /// The callable's declared name, or `None` for an anonymous `fn`/a sourceless callable.
    pub name: Option<String>,
    /// `true` for a `fn` (yields a value), `false` for a `to` (procedure), `None` if
    /// indeterminate.
    pub is_function: Option<bool>,
    /// The `[start, end)` span of its `to`/`fn` declaration, or `None` for a sourceless callable.
    pub decl_span: Option<[u32; 2]>,
}

/// One entry of the [`stack_walk`](Session::stack_walk) transcript — a **live** call frame or a
/// **tail-elided** one (E§8.2/§8.3). Everything here is plain, GC-owned data: nothing needs to
/// be released. A live frame carries its callable reflection, call-site span, tail-iteration
/// count, and its in-scope local/dynamic **names**; an elided frame (marked `elided`) carries
/// only its callable reflection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameData {
    /// The frame's callable reflection; `None` for the module top level and `do…end` blocks.
    pub callable: Option<CallableInfo>,
    /// The `[start, end)` call-site span; `None` for the module top and native-invoked blocks.
    pub call_site: Option<[u32; 2]>,
    /// Tail-iterations absorbed into this frame by proper-tail-call reuse (E§8.3).
    pub tail_count: u64,
    /// The frame's in-scope local names, in slot order (empty for an elided frame or a block).
    pub locals: Vec<String>,
    /// The `with`-established dynamic-parameter names in this frame (empty for an elided frame).
    pub dynamics: Vec<String>,
    /// The frame's **home module** index (E§8.2) — the module whose globals are in scope here,
    /// the key for [`module_global_names`](Session::module_global_names). `None` for an elided
    /// frame.
    pub module: Option<u32>,
    /// `true` if this is a tail-elided caller (E§8.3), not a live activation.
    pub elided: bool,
}

/// A module-level binding the debugger lists (E§8.2): its `name`, declaration `kind` tag (the host
/// filters — the *variables* are `let`/`const`/`parameter`), and `slot` (the key for
/// [`module_global_value`](Session::module_global_value)). Value-free; read the value lazily.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalBindingData {
    /// The declared name.
    pub name: String,
    /// The declaration-kind tag: `let`/`const`/`parameter`/`to`/`fn`/`record`/`protocol`/`module`.
    pub kind: &'static str,
    /// The binding's declaration-order index — the `slot` for [`module_global_value`].
    pub slot: usize,
}

/// The outcome of an auxiliary `to_string` evaluation (E§8.4/S-22) — a plain-typed mirror of
/// `doodle_core::machine::AuxOutcome`. `Rendered`/`Raised` carry a fresh **host-owned** handle
/// (release it); `Faulted` carries a kebab-cased engine-fault tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuxOutcomeData {
    /// `to_string` produced a `String` — a handle to it.
    Rendered(Handle),
    /// `to_string` raised — a handle to the raised value.
    Raised(Handle),
    /// A non-resumable engine fault stopped the auxiliary drive (e.g. `limit:stack-depth`).
    Faulted(&'static str),
}

/// A [`frame_local`](Session::frame_local)/[`frame_dynamic`](Session::frame_dynamic) read whose
/// generation token no longer matches the instance's current pause generation: the stack has
/// advanced since the [`stack_walk`](Session::stack_walk) that produced the token, so the frame
/// index would address a different frame. Reported as a clean error, never a wrong answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleGeneration;

impl Session {
    /// Sets a breakpoint at (`canonical_id`, 1-based `line`) and returns its id (E§8.6). A
    /// canonical that is not loaded, or a line past the last statement, is **pending** — never
    /// an error. Address the entry program by [`entry_module`](Session::entry_module).
    pub fn set_breakpoint(&mut self, canonical_id: &str, line: u32) -> u32 {
        self.instance.set_breakpoint(canonical_id, line).0
    }

    /// Clears the breakpoint `id` (E§8.6); idempotent.
    pub fn clear_breakpoint(&mut self, id: u32) {
        self.instance.clear_breakpoint(BreakpointId(id));
    }

    /// The installed breakpoints (E§8.6), in set order, each marked resolved or pending.
    pub fn breakpoints(&self) -> Vec<BreakpointInfo> {
        self.instance.breakpoints()
    }

    /// Enables or disables raise-trapping (E§8.7): when on, a `Continue`/`Step*` drive pauses at
    /// the point an exception is raised, before the stack unwinds.
    pub fn set_raise_trapping(&mut self, enabled: bool) {
        self.instance.set_raise_trapping(enabled);
    }

    /// Sets the observation granularity (E§8.8, S-62): `subexpr` true = per-subexpression fine
    /// stops (watch-it-run), false = per-statement (the default).
    pub fn set_observation_mode(&mut self, subexpr: bool) {
        self.instance.set_observation_mode(if subexpr {
            ObservationMode::Subexpression
        } else {
            ObservationMode::Statement
        });
    }

    /// The current call stack (E§8.2), innermost first, followed by the tail-elided history
    /// (E§8.3, most-recent first, each marked `elided`) — one transcript. Every entry is plain
    /// GC-owned data with nothing to release; expand a binding's value with
    /// [`frame_local`](Self::frame_local)/[`frame_dynamic`](Self::frame_dynamic) carrying the
    /// [`pause_generation`](Self::pause_generation) current at this walk.
    pub fn stack_walk(&mut self) -> Vec<FrameData> {
        let frames = self.instance.stack_walk();
        let mut out = Vec::with_capacity(frames.len());
        for (index, frame) in frames.into_iter().enumerate() {
            let callable = match frame.callable {
                Some(handle) => {
                    let info = self.reflect_callable(handle);
                    let _ = self.instance.release(handle);
                    Some(info)
                }
                None => None,
            };
            out.push(FrameData {
                callable,
                call_site: frame.call_site.map(span_pair),
                tail_count: frame.tail_count,
                locals: self.instance.frame_local_names(index),
                dynamics: self.instance.frame_dynamic_names(index),
                module: Some(frame.module.0),
                elided: false,
            });
        }
        for elided in self.instance.tail_elided_history() {
            // An elided caller has no live call site — its source location is its declaration,
            // carried in the reflected callable's `declSpan` (`callable_position`, which for a
            // source callable equals `ElidedFrameObservation::decl_span`). Leaving `call_site`
            // absent keeps it out of a live-stack shape check (which keys on the call site).
            let info = self.reflect_callable(elided.callable);
            let _ = self.instance.release(elided.callable);
            out.push(FrameData {
                callable: Some(info),
                call_site: None,
                tail_count: 0,
                locals: Vec::new(),
                dynamics: Vec::new(),
                module: None,
                elided: true,
            });
        }
        out
    }

    /// The module-level bindings of module `module` (E§8.2, the home module of a
    /// [`stack_walk`](Self::stack_walk) frame), each `{name, kind, slot}` — the handle-free eager
    /// half; read a value lazily with [`module_global_value`](Self::module_global_value). Module
    /// globals are in scope module-wide, so a host shows them once per module.
    pub fn module_global_names(&self, module: u32) -> Vec<GlobalBindingData> {
        self.instance
            .module_global_names(module as usize)
            .into_iter()
            .map(|g| GlobalBindingData {
                name: g.name,
                kind: global_kind_tag(g.kind),
                slot: g.slot,
            })
            .collect()
    }

    /// A fresh **host-owned** handle to the current value of module `module`'s `slot`-th global
    /// (§8.2), or `None` if out of range or **not yet defined** (the module-level TDZ; never a
    /// fault). A `parameter`'s value is its live `with`-overridden value. `generation` must be the
    /// [`pause_generation`](Self::pause_generation) the read is tied to; a stale one is a
    /// [`StaleGeneration`] error.
    pub fn module_global_value(
        &mut self,
        generation: u32,
        module: u32,
        slot: usize,
    ) -> Result<Option<Handle>, StaleGeneration> {
        self.check_generation(generation)?;
        Ok(self.instance.module_global_value(module as usize, slot))
    }

    /// A fresh **host-owned** handle to frame `frame`'s `slot`-th local value (§8.2), or `None`
    /// for an out-of-range/uninitialized slot. `generation` must be the
    /// [`pause_generation`](Self::pause_generation) the [`stack_walk`](Self::stack_walk) that
    /// listed the names was read at; a stale one is a [`StaleGeneration`] error.
    pub fn frame_local(
        &mut self,
        generation: u32,
        frame: usize,
        slot: usize,
    ) -> Result<Option<Handle>, StaleGeneration> {
        self.check_generation(generation)?;
        Ok(self.instance.frame_local_value(frame, slot))
    }

    /// A fresh **host-owned** handle to frame `frame`'s `slot`-th `with` binding value (§8.2),
    /// or `None` if out of range/unbound. Generation-checked like
    /// [`frame_local`](Self::frame_local).
    pub fn frame_dynamic(
        &mut self,
        generation: u32,
        frame: usize,
        slot: usize,
    ) -> Result<Option<Handle>, StaleGeneration> {
        self.check_generation(generation)?;
        Ok(self.instance.frame_dynamic_value(frame, slot))
    }

    /// The `[start, end)` span of the non-leaf subexpression just completed at a **fine** safe
    /// point (E§7.4/§8.4), whose value is in the result register — `None` at a statement stop.
    pub fn completed_position(&self) -> Option<Span> {
        self.instance.completed_position().map(|p| p.span)
    }

    /// A fresh **host-owned** handle to the current result-register value (E§8.4), or `None` for
    /// Void — the just-produced value at a fine stop, paired with
    /// [`completed_position`](Self::completed_position).
    pub fn current_result(&mut self) -> Option<Handle> {
        self.instance.result_handle()
    }

    /// At a raise-trap pause (E§8.7), a fresh **host-owned** handle to the raised value; `None`
    /// if no raise is trapped. Consuming it marks the trap taken so the resumed drive unwinds.
    pub fn trapped_raise(&mut self) -> Option<Handle> {
        self.instance.trapped_raise()
    }

    /// At a raise-trap pause, the `[start, end)` span of the raise site (E§8.7); `None` if no
    /// raise is trapped.
    pub fn trapped_raise_position(&self) -> Option<Span> {
        self.instance.trapped_raise_position().map(|p| p.span)
    }

    /// Host-driven `to_string` on `handle` at a paused instance (E§8.4/S-22): renders through
    /// the native seam or drives the value's explicit `Stringable`, on its own `fuel` budget,
    /// with breakpoints/raise-trap suppressed and the outer pause restored. See
    /// [`AuxOutcomeData`].
    pub fn eval_to_string(&mut self, handle: Handle, fuel: u64) -> AuxOutcomeData {
        match self.instance.eval_to_string(handle, fuel) {
            AuxOutcome::Rendered(handle) => AuxOutcomeData::Rendered(handle),
            AuxOutcome::Raised(handle) => AuxOutcomeData::Raised(handle),
            AuxOutcome::Faulted(fault) => AuxOutcomeData::Faulted(fault_tag(fault)),
        }
    }

    /// Reflects a callable handle into plain [`CallableInfo`] (minting nothing that escapes).
    /// A freshly-minted callable handle reflects cleanly; a reflection error maps to `None`
    /// rather than surfacing — the panel simply shows less.
    fn reflect_callable(&self, handle: Handle) -> CallableInfo {
        CallableInfo {
            name: self.instance.callable_name(handle).ok().flatten(),
            is_function: self.instance.callable_is_function(handle).ok().flatten(),
            decl_span: self
                .instance
                .callable_position(handle)
                .ok()
                .flatten()
                .map(|p| span_pair(p.span)),
        }
    }

    /// Errors if `generation` is not the current pause generation (a stale frame index).
    fn check_generation(&self, generation: u32) -> Result<(), StaleGeneration> {
        (generation == self.pause_generation())
            .then_some(())
            .ok_or(StaleGeneration)
    }
}

/// A [`Span`] as a `[start, end)` byte-offset pair — the JS-facing span shape.
fn span_pair(span: Span) -> [u32; 2] {
    [span.start, span.end]
}

/// The host-facing tag for a module-global declaration kind (the source keyword).
fn global_kind_tag(kind: GlobalKind) -> &'static str {
    match kind {
        GlobalKind::Let => "let",
        GlobalKind::Const => "const",
        GlobalKind::Parameter => "parameter",
        GlobalKind::Proc => "to",
        GlobalKind::Fn => "fn",
        GlobalKind::Record => "record",
        GlobalKind::Protocol => "protocol",
        GlobalKind::Module => "module",
    }
}
