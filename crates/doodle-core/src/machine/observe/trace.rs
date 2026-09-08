//! Capturing a raise's [`Trace`] and reading it back post-mortem (engine spec E§9,
//! §8.2/§8.3). [`capture_trace`] snapshots the machine at the raise site (before any
//! unwinding); when the raise is terminal the drive retains that trace on the instance,
//! and the `raised_trace_*` readers here project it — the live frames and the bounded
//! tail-elided history, each as a **position and a callable** (E§9).
//!
//! The retained trace keeps its callables as heap indices (`gc::collect` roots them), so a
//! reader mints a callable's handle and recomputes its declaration position **at read time**
//! from the callable's own home module — correct across modules, and the frames themselves
//! need not survive. This mirrors the live [`Instance::tail_history_entry`]/
//! [`Instance::frame_callable`] readers, only sourced from the retained trace rather than the
//! live stack.

use super::super::error::{Trace, TraceFrame};
use super::super::frame::FrameKind;
use super::super::{Handle, Instance, Machine, Value};
use super::Position;
use crate::resolve::ResolvedModule;
use crate::span::Span;

/// Captures a raise's trace (E§8.2/§9, L§12.1) from the machine state at the raise site,
/// **before any unwinding**: the raising position, the live call stack (innermost first),
/// and the bounded tail-elided history (most-recent first). Deterministic (E§11) and mints
/// no handles — it holds engine-internal spans and callable indices, never host references.
pub(crate) fn capture_trace(
    resolved: &ResolvedModule,
    machine: &Machine,
    raised_at: Option<Span>,
) -> Trace {
    let frames = machine
        .frames
        .iter()
        .rev()
        .map(|frame| TraceFrame {
            callable: match frame.kind {
                FrameKind::Callable { cal } => Some(cal),
                FrameKind::Block { .. } | FrameKind::ModuleTopLevel => None,
            },
            call_site: frame.call_site.map(|node| resolved.ast.span(node)),
            tail_count: frame.tail_count,
        })
        .collect();
    let tail_elided = machine.ring.most_recent_first().collect();
    Trace {
        raised_at,
        frames,
        tail_elided,
    }
}

/// One live frame of a retained raise trace (E§9), read a frame at a time so its callable
/// handle mints lazily — the retained-trace analogue of [`super::FrameInfo`]. Innermost-first
/// `index`, matching [`Instance::stack_walk`]'s order at the raise.
#[derive(Clone, Copy, Debug)]
pub struct TraceFrameInfo {
    /// Whether this frame ran a callable value (fetch it with
    /// [`Instance::raised_trace_frame_callable`]); `false` for the module top level and a
    /// `do … end` block frame.
    pub has_callable: bool,
    /// Where the frame was entered — the call site's [`Span`]. `None` for the module top
    /// level and a block invoked by a native consumer (host code, no call site).
    pub call_site: Option<Span>,
    /// Tail-iterations absorbed into this frame by proper-tail-call reuse (E§8.3).
    pub tail_count: u64,
}

impl Instance {
    /// The number of live frames in the retained trace of the last terminal raise (E§9),
    /// or `None` if the last drive did not end `Raised`. Innermost-first index space for
    /// [`raised_trace_frame`](Self::raised_trace_frame) /
    /// [`raised_trace_frame_callable`](Self::raised_trace_frame_callable).
    pub fn raised_trace_frame_count(&self) -> Option<usize> {
        Some(self.machine.raised_trace.as_ref()?.frames.len())
    }

    /// The [`TraceFrameInfo`] for innermost-first `index` of the retained trace, or `None`
    /// (no retained trace, or index out of range). Mints no handle, like
    /// [`frame_info`](Self::frame_info).
    pub fn raised_trace_frame(&self, index: usize) -> Option<TraceFrameInfo> {
        let frame = self.machine.raised_trace.as_ref()?.frames.get(index)?;
        Some(TraceFrameInfo {
            has_callable: frame.callable.is_some(),
            call_site: frame.call_site,
            tail_count: frame.tail_count,
        })
    }

    /// A fresh **host-owned** handle to frame `index`'s callable in the retained trace (E§9),
    /// or `None` for a block / module-top frame (no callable value), an out-of-range index,
    /// or no retained trace. Mints like [`frame_callable`](Self::frame_callable); the host
    /// [`release`](Self::release)s it. The retained trace roots the callable, so this is valid
    /// for the terminal state's life even though the live frame has unwound.
    pub fn raised_trace_frame_callable(&mut self, index: usize) -> Option<Handle> {
        let cal = self
            .machine
            .raised_trace
            .as_ref()?
            .frames
            .get(index)?
            .callable?;
        Some(self.intern(Value::Callable(cal)))
    }

    /// The number of **tail-elided** entries in the retained trace (E§8.3), or `None` if the
    /// last drive did not end `Raised`.
    pub fn raised_trace_tail_count(&self) -> Option<usize> {
        Some(self.machine.raised_trace.as_ref()?.tail_elided.len())
    }

    /// The `index`-th tail-elided entry of the retained trace (most recent first, E§8.3): a
    /// fresh **host-owned** handle to the elided callable + its declaration [`Position`], or
    /// `None` (out of range, or no retained trace). Mirrors
    /// [`tail_history_entry`](Self::tail_history_entry); the host releases the handle.
    pub fn raised_trace_tail_entry(&mut self, index: usize) -> Option<(Handle, Position)> {
        let cal = *self.machine.raised_trace.as_ref()?.tail_elided.get(index)?;
        let position = {
            let obj = self.heap.callable(cal);
            let resolved = &self.modules[obj.module.0 as usize].resolved;
            Position {
                module: obj.module,
                span: resolved
                    .ast
                    .span(resolved.callables[obj.source_id() as usize].decl),
            }
        };
        Some((self.intern(Value::Callable(cal)), position))
    }

    /// The source span the raise occurred at (E§9), or `None` (unknown span, or the last
    /// drive did not end `Raised`). The module is the raise site's; a host resolves it as for
    /// any [`Span`] against the source it loaded.
    pub fn raised_trace_raised_at(&self) -> Option<Span> {
        self.machine.raised_trace.as_ref()?.raised_at
    }
}
