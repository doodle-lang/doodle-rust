//! The observation surface (engine spec E§8.1/§8.2/§8.3, MD §17), for a paused or
//! otherwise stopped instance: the current source position ([`Instance::current_position`]),
//! the call-stack walk ([`Instance::stack_walk`]), and — the M6.2 rich per-frame surface —
//! a frame's named **local bindings** ([`Instance::frame_locals`]) and `with`
//! **dynamic-parameter bindings** ([`Instance::frame_dynamic_bindings`]), plus the bounded
//! **tail-elided history** ([`Instance::tail_elided_history`], E§8.3). The engine exposes
//! **positions, not source text** (E§8.1): a [`Position`] is a module id + byte [`Span`],
//! and the host renders line/column from the source it holds (as diagnostics do). Value
//! inspection of the handles these mint is `machine/inspect.rs` (E§8.4).

use super::error::{Trace, TraceFrame};
use super::frame::FrameKind;
use super::{CalIdx, CellIdx, Handle, Instance, Machine, ObservationMode, Value};
use crate::ast::Node;
use crate::diag::Diagnostic;
use crate::heap::Heap;
use crate::machine::cont::Cont;
use crate::resolve::{BodyKind, ResolvedModule};
use crate::span::{ModuleId, Span};

mod bindings;

/// A name→value binding the debugger shows for a frame (E§8.2): a **local** (a parameter or
/// a `let`/`const` in scope) or a **dynamic parameter** bound by `with`. `value` is `None`
/// for a local whose declaration has not executed yet (the temporal dead zone) — the name is
/// in scope but unbound. A `Some` handle is a fresh **host-owned** handle (release it).
#[derive(Clone, Debug)]
pub struct Binding {
    /// The bound name.
    pub name: String,
    /// The current value, or `None` if the slot is not yet initialized.
    pub value: Option<Handle>,
}

/// One **tail-elided** frame in the bounded history (E§8.3): a caller a proper tail call
/// overwrote, kept so a visualizer can show recursion depth. Distinct from the live frames
/// of [`stack_walk`](Instance::stack_walk) — a host must **not** present these as live
/// activations, only as evidence that tail recursion occurred.
#[derive(Clone, Debug)]
pub struct ElidedFrameObservation {
    /// The elided callable, as a fresh host-owned handle (release it).
    pub callable: Handle,
    /// The elided callable's declaration span (its source location).
    pub decl_span: Span,
}

/// A source position the engine exposes (E§8.1): which module, and a byte [`Span`] into
/// its NFC-normalized source. The host maps the span to 1-based line/column (columns in
/// code points, L§3.1) using the source it loaded — the engine holds positions, not text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Position {
    /// The module the position is in (its canonical id, E§6).
    pub module: ModuleId,
    /// The byte range of the construct at this position.
    pub span: Span,
}

/// Per-frame data read **one frame at a time** (E§8.2), for a pull surface that mints the
/// callable handle lazily ([`frame_callable`](Instance::frame_callable)) rather than eagerly
/// like the bulk [`stack_walk`](Instance::stack_walk) — the granularity the C ABI observation
/// surface wants (D-M7-13). Innermost-first `index`, matching `stack_walk`'s order.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// Whether this frame runs a callable value (fetch it with
    /// [`frame_callable`](Instance::frame_callable)); `false` for the module top level and
    /// `do … end` block frames.
    pub has_callable: bool,
    /// Where this frame was entered — the call site's [`Span`]. `None` for the module top level
    /// and a block invoked by a native consumer (as [`FrameObservation::call_site`]).
    pub call_site: Option<Span>,
    /// Tail-iterations absorbed into this frame by proper-tail-call reuse (E§8.3).
    pub tail_count: u64,
    /// The frame's home module (E§8.2), as [`FrameObservation::module`].
    pub module: ModuleId,
}

/// One frame in a [`stack_walk`](Instance::stack_walk) (E§8.2), innermost first.
#[derive(Clone, Debug)]
pub struct FrameObservation {
    /// The callable this frame is running, as a fresh **host-owned** handle (the host
    /// must [`release`](Instance::release) it) — identity-correct for closures. `None`
    /// for the module top level and for `do … end` block frames (neither is a callable
    /// value).
    pub callable: Option<Handle>,
    /// Where this frame was entered — the call site's [`Span`]. `None` for the module top
    /// level (no call site) and for a block invoked by a native consumer (host code).
    pub call_site: Option<Span>,
    /// Tail-iterations absorbed into this frame by proper-tail-call reuse (E§8.3): `0`
    /// for a fresh frame, `n` after `n` tail calls reused the same slot.
    pub tail_count: u64,
    /// The frame's **home module** (E§8.2): the module whose globals are in scope here, so a host
    /// shows the module-level bindings ([`module_global_names`](Instance::module_global_names)) of
    /// the selected frame's module — once per module (they are module-scoped, not per-frame).
    pub module: ModuleId,
}

impl Instance {
    /// The current source position (E§8.1): the span of the construct the active frame is
    /// about to execute (or, at a return, the call it just returned through — MD §17).
    /// `None` only if the instance has no active frame (already terminal). Meaningful at a
    /// stopped instance (a `Paused`/`Suspended`); on a running one it is simply the last
    /// transition's position.
    pub fn current_position(&self) -> Option<Position> {
        let frame = self.machine.frames.last()?;
        // The active cont (top of the frame's work) is what is about to run; its span is
        // the position. Some conts (a bare `ReturnBarrier`) carry no span, so scan down to
        // the first that does; then fall back to where the frame was entered, and finally
        // — a frame drained to the point of returning, with no call site (the module top,
        // or a native-invoked block) — to a zero-width position at the **end of the
        // module**, so a host renders "end of program" rather than the file's start.
        let span = frame
            .conts
            .iter()
            .rev()
            .find_map(|cont| self.cont_span(cont))
            .or_else(|| {
                frame
                    .call_site
                    .map(|node| self.current_resolved().ast.span(node))
            })
            .unwrap_or_else(|| {
                let end = self
                    .current_resolved()
                    .ast
                    .span(self.current_resolved().root)
                    .end;
                Span::new(end, end)
            });
        Some(Position {
            module: self.current_resolved().canonical_id,
            span,
        })
    }

    /// Sets the observation-mode granularity (E§8.8, S-62): `Subexpression` turns on
    /// fine safe points (a `Step*`/host-pause may then stop at each non-leaf subexpression
    /// completion); `Statement` (the default) turns them off. Adjustable between drives, so a
    /// host runs coarse when nobody is watching and switches to fine when the debugger opens.
    /// Changes only *where* stepping may stop — never what the program computes or when a
    /// limit trips (accounting stays at statement safe points, §7.4).
    pub fn set_observation_mode(&mut self, mode: ObservationMode) {
        self.machine.observation_mode = mode;
    }

    /// The current observation-mode granularity (E§8.8).
    pub fn observation_mode(&self) -> ObservationMode {
        self.machine.observation_mode
    }

    /// The position of the non-leaf subexpression just completed at a **fine** safe point
    /// (E§7.4/§8.4): `Some` only at a fine stop (`Subexpression` mode), where the value it
    /// produced is in the result register ([`result`](Self::result)) — together the
    /// "watch your expression evaluate" primitive. `None` at a statement stop (use
    /// [`current_position`](Self::current_position)) or when not stopped at a fine point.
    pub fn completed_position(&self) -> Option<Position> {
        let span = self.machine.fine_span?;
        let module = self.machine.frames.last()?.module;
        Some(Position { module, span })
    }

    /// A fresh **host-owned** handle to the current **result register** value (E§8.4), or `None`
    /// if the register is empty (Void). At a **fine** safe point (`Subexpression` mode) this is the
    /// value the just-completed subexpression produced — together with
    /// [`completed_position`](Self::completed_position), the "watch your expression evaluate"
    /// primitive (S-62). Mints a handle like [`stack_walk`](Self::stack_walk); the host frees it.
    pub fn result_handle(&mut self) -> Option<Handle> {
        let value = self.result()?;
        Some(self.intern(value))
    }

    /// The current call stack (E§8.2), innermost frame first: each frame's callable (a
    /// fresh host-owned handle), its call-site position, and its tail-iteration count.
    /// Mints one handle per callable frame — like [`list_get`](Instance::list_get), the
    /// host owns and must [`release`](Instance::release) them.
    pub fn stack_walk(&mut self) -> Vec<FrameObservation> {
        // Collect the raw per-frame data first (an immutable borrow of frames + AST),
        // then mint the callable handles (a mutable borrow) — the two borrows cannot
        // overlap. Innermost first, so reverse the bottom-up frame stack.
        let raw: Vec<(Option<super::CalIdx>, Option<Span>, u64, ModuleId)> = self
            .machine
            .frames
            .iter()
            .rev()
            .map(|frame| {
                let cal = match frame.kind {
                    FrameKind::Callable { cal } => Some(cal),
                    FrameKind::Block { .. } | FrameKind::ModuleTopLevel => None,
                };
                let call_site = frame
                    .call_site
                    .map(|node| self.current_resolved().ast.span(node));
                (cal, call_site, frame.tail_count, frame.module)
            })
            .collect();
        raw.into_iter()
            .map(|(cal, call_site, tail_count, module)| FrameObservation {
                callable: cal.map(|cal| self.intern(Value::Callable(cal))),
                call_site,
                tail_count,
                module,
            })
            .collect()
    }

    /// The number of live frames (E§8.2) — the innermost-first index space the per-frame
    /// accessors ([`frame_info`](Self::frame_info)/[`frame_callable`](Self::frame_callable))
    /// address, for a host that reads the stack one frame at a time rather than via the bulk
    /// [`stack_walk`](Self::stack_walk).
    pub fn frame_count(&self) -> usize {
        self.machine.frames.len()
    }

    /// The [`FrameInfo`] for innermost-first `index` (E§8.2), minting no handle, or `None` if
    /// out of range. The callable is fetched separately ([`frame_callable`](Self::frame_callable)),
    /// so a host that only needs positions/counts pays for no handles (D-M7-13). Matches
    /// [`stack_walk`](Self::stack_walk)'s per-frame fields.
    pub fn frame_info(&self, index: usize) -> Option<FrameInfo> {
        let actual = self.frame_at(index)?;
        let frame = &self.machine.frames[actual];
        Some(FrameInfo {
            has_callable: matches!(frame.kind, FrameKind::Callable { .. }),
            call_site: frame
                .call_site
                .map(|node| self.current_resolved().ast.span(node)),
            tail_count: frame.tail_count,
            module: frame.module,
        })
    }

    /// A fresh **host-owned** handle to frame `index`'s callable (E§8.2), or `None` for a block /
    /// module-top frame (no callable value) or an out-of-range index. Mints like
    /// [`stack_walk`](Self::stack_walk); the host releases it. Once minted the handle is an
    /// ordinary handle (valid across resumes) — only the `index` addressing is pause-scoped
    /// (D-M7-13).
    pub fn frame_callable(&mut self, index: usize) -> Option<Handle> {
        let actual = self.frame_at(index)?;
        let cal = match self.machine.frames[actual].kind {
            FrameKind::Callable { cal } => cal,
            FrameKind::Block { .. } | FrameKind::ModuleTopLevel => return None,
        };
        Some(self.intern(Value::Callable(cal)))
    }

    /// The host canonical id a module was loaded under (E§6), or `None` for an unknown token —
    /// the reverse of the load-time string→id mapping, so a host resolves a [`Position`]'s opaque
    /// module token to the source it holds (D-M7-14).
    pub fn module_canonical_id(&self, module: ModuleId) -> Option<&str> {
        self.machine.load.canonical_of(module)
    }

    /// The call-site spans of the active callable frames (E§8.2), innermost first — like
    /// [`stack_walk`](Self::stack_walk) but returning only source positions, minting no
    /// callable handles. Frames with no call site (a block, the module top) are skipped. A
    /// host uses this to find the outer (e.g. user-program) line currently executing while
    /// inner library frames run on top of it — a live line highlight.
    pub fn call_site_spans(&self) -> Vec<Span> {
        self.machine
            .frames
            .iter()
            .rev()
            .filter_map(|frame| {
                frame
                    .call_site
                    .map(|node| self.current_resolved().ast.span(node))
            })
            .collect()
    }

    /// The instance **load-diagnostics record** (E§3.2/§8, S-63), read by pull: the
    /// front-end diagnostics for every module the instance has loaded or attempted, from
    /// `since` onward. The host tracks its cursor as `since + result.len()`, so polling at
    /// successive safe points stays linear. Warnings on a successful load (prelude
    /// shadowing, L§5.1) and every imported module's load-time diagnostics (parse +
    /// resolve, **errors included**) accumulate here in deterministic order — load order
    /// across modules, then producer order (nondecreasing span start) within a module. It
    /// is the one *display* surface: errors still drive control flow through their own
    /// channels (a `LoadError` for the entry module; a `module-load-error` raised in the
    /// importer for a broken import). The record is engine-owned, not program data — not
    /// heap-charged and not visible to Doodle code. Readable right after `load` (a `Ready`
    /// instance) and at any stopped state; a host reads it when stopped, never mid-drive.
    pub fn load_diagnostics(&self, since: usize) -> &[Diagnostic] {
        let record = &self.machine.load_diagnostics;
        &record[since.min(record.len())..]
    }

    /// The **tail-elided history** (E§8.3): the callers proper tail calls overwrote, most
    /// recent first, each a callable handle + its declaration span. Bounded by the ring
    /// capacity (`config`); distinct from the live [`stack_walk`](Self::stack_walk) frames.
    pub fn tail_elided_history(&mut self) -> Vec<ElidedFrameObservation> {
        let ordered: Vec<CalIdx> = self.machine.ring.most_recent_first().collect();
        let raw: Vec<(CalIdx, Span)> = ordered
            .into_iter()
            .map(|cal| {
                let obj = self.heap.callable(cal);
                let resolved = &self.modules[obj.module.0 as usize].resolved;
                let decl = resolved.callables[obj.source_id() as usize].decl;
                (cal, resolved.ast.span(decl))
            })
            .collect();
        raw.into_iter()
            .map(|(cal, decl_span)| ElidedFrameObservation {
                callable: self.intern(Value::Callable(cal)),
                decl_span,
            })
            .collect()
    }

    /// Maps an innermost-first frame `index` (as [`stack_walk`](Self::stack_walk) yields) to
    /// its index in the bottom-up `frames` stack, or `None` if out of range.
    fn frame_at(&self, index: usize) -> Option<usize> {
        self.machine.frames.len().checked_sub(index + 1)
    }

    /// The `(module index, CallableInfo index)` whose `slot_names` name frame `actual`'s
    /// locals: a callable frame's invoked callable, or the top-level body. `None` for a
    /// block (no own named-local surface — it reads its enclosing frame's, §7).
    fn frame_body(&self, actual: usize) -> Option<(usize, usize)> {
        match self.machine.frames[actual].kind {
            FrameKind::Callable { cal } => {
                let obj = self.heap.callable(cal);
                Some((obj.module.0 as usize, obj.source_id() as usize))
            }
            FrameKind::ModuleTopLevel => {
                let m = self.machine.frames[actual].module.0 as usize;
                let id = self.modules[m]
                    .resolved
                    .callables
                    .iter()
                    .position(|c| matches!(c.kind, BodyKind::ModuleTopLevel))?;
                Some((m, id))
            }
            FrameKind::Block { .. } => None,
        }
    }

    /// The name a module dynamic-parameter `cell` is bound to (E§8.2): a reverse scan of the
    /// loaded modules' namespaces (small, deterministic), or `"?"` if none names it.
    fn cell_name(&self, cell: CellIdx) -> String {
        for m in &self.modules {
            for (name, c) in &m.namespace {
                if *c == cell {
                    return name.to_string();
                }
            }
        }
        "?".to_string()
    }

    /// The span the construct a continuation is at occupies (E§8.1, MD §17), or `None`
    /// for a span-less marker (a `ReturnBarrier`). Conts that carry an operator/operand
    /// span report it directly; those that name a node resolve it through the AST.
    fn cont_span(&self, cont: &Cont) -> Option<Span> {
        let ast = &self.current_resolved().ast;
        Some(match cont {
            Cont::Eval { node } => ast.span(*node),
            Cont::BindLet { decl } => ast.span(*decl),
            Cont::AssignTo { assign }
            | Cont::AssignPlaceObj { assign }
            | Cont::AssignFieldVal { assign, .. }
            | Cont::AssignIndexKey { assign, .. }
            | Cont::AssignIndexVal { assign, .. } => ast.span(*assign),
            Cont::IfChoose { node, .. } => ast.span(*node),
            Cont::WhileCheck { node } => ast.span(*node),
            Cont::LoopReloop { node } => ast.span(*node),
            Cont::CallGotCallee { call } => ast.span(*call),
            Cont::CallGotArg { call, .. } => ast.span(*call),
            Cont::BlockGotArg { call, .. } => ast.span(*call),
            Cont::ListGotElem { list, .. } => ast.span(*list),
            Cont::StrInterp { node, .. } | Cont::StrInterpRendered { node, .. } => ast.span(*node),
            Cont::DictGotKey { dict, .. } | Cont::DictGotValue { dict, .. } => ast.span(*dict),
            Cont::DictBuildHashed { node, .. } => ast.span(*node),
            Cont::FieldRead { field } => ast.span(*field),
            Cont::BindDefault { default, .. } => ast.span(*default),
            Cont::DefineCallable { decl } => ast.span(*decl),
            Cont::DefineRecord { decl }
            | Cont::DefineProtocol { decl }
            | Cont::DefineImplement { decl } => ast.span(*decl),
            Cont::ExitApply { exit } => ast.span(*exit),
            Cont::WithBind { with } => ast.span(*with),
            Cont::TryHandler { try_node } => ast.span(*try_node),
            Cont::RaiseApply { raise } => ast.span(*raise),
            // While parked on an import (E§6), the importer's position is the `import`
            // statement itself (S-17): a host observing a suspended-on-import instance sees
            // it there.
            Cont::ImportTargets { import, .. } => ast.span(*import),
            // A cleanup marker restores a binding / clears a handler; no construct of its own.
            Cont::WithRestore { .. } | Cont::PopHandler => return None,
            // Operator plumbing carries the operator's span directly.
            Cont::BinRhs { span, .. }
            | Cont::BinApply { span, .. }
            | Cont::UnaryApply { span, .. }
            | Cont::AndRhs { span, .. }
            | Cont::OrRhs { span, .. }
            | Cont::IndexGotObject { span, .. }
            | Cont::IndexApply { span, .. }
            | Cont::IndexReadHashed { span, .. }
            | Cont::IndexAssignHashed { span, .. }
            | Cont::AssertBool { span } => *span,
            // A `Seq` is about to run its next statement (or has drained the body).
            Cont::Seq { block, next } => match self.stmt_at(*block, *next) {
                Some(stmt) => ast.span(stmt),
                None => ast.span(*block),
            },
            // A return marker has no construct of its own (MD §17): the caller's next cont
            // supplies the position instead, so scan past it.
            Cont::ReturnBarrier => return None,
        })
    }

    /// The `next`-th statement node of a `Module`/`Block` body, if in range.
    fn stmt_at(&self, block: crate::ast::NodeId, next: u32) -> Option<crate::ast::NodeId> {
        let stmts = match self.current_resolved().ast.node(block) {
            Node::Module { stmts, .. } | Node::Block(stmts) => stmts,
            _ => return None,
        };
        stmts.get(next as usize).copied()
    }
}

/// Captures a raise's trace (E§8.2/§9, L§12.1) from the machine state at the raise site,
/// **before any unwinding**: the raising position, the live call stack (innermost first),
/// and the bounded tail-elided history (most-recent first, its callables' decl spans).
/// Deterministic (E§11) and mints no handles (unlike [`Instance::stack_walk`]) — the trace
/// holds only engine-internal spans, so it never leaks host-owned references.
pub(crate) fn capture_trace(
    resolved: &ResolvedModule,
    heap: &Heap,
    machine: &Machine,
    raised_at: Option<Span>,
) -> Trace {
    let frames = machine
        .frames
        .iter()
        .rev()
        .map(|frame| TraceFrame {
            call_site: frame.call_site.map(|node| resolved.ast.span(node)),
            tail_count: frame.tail_count,
        })
        .collect();
    let tail_elided = machine
        .ring
        .most_recent_first()
        .map(|cal| {
            let id = heap.callable(cal).source_id() as usize;
            resolved.ast.span(resolved.callables[id].decl)
        })
        .collect();
    Trace {
        raised_at,
        frames,
        tail_elided,
    }
}
