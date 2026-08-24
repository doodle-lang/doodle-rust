//! The minimal observation surface (engine spec E§8.1/§8.2, MD §17): the current
//! source position and a call-stack walk, for a paused (or otherwise stopped) instance.
//!
//! **Scope (M2b.7 minimum).** [`Instance::current_position`] and
//! [`Instance::stack_walk`] — enough to show "where am I" and "how did I get here". The
//! richer per-frame surface of E§8.2 (named local bindings, dynamic-parameter bindings)
//! and the value-inspection / breakpoint facilities (E§8.4/§8.6) join with dynamic
//! parameters and the debugger at M4/M6. The engine exposes **positions, not source
//! text** (E§8.1): a [`Position`] is a module id + byte [`Span`], and the host renders
//! line/column from the source it holds (as diagnostics do).

use super::frame::FrameKind;
use super::{Handle, Instance, Value};
use crate::ast::Node;
use crate::machine::cont::Cont;
use crate::span::{ModuleId, Span};

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
            .or_else(|| frame.call_site.map(|node| self.resolved.ast.span(node)))
            .unwrap_or_else(|| {
                let end = self.resolved.ast.span(self.resolved.root).end;
                Span::new(end, end)
            });
        Some(Position {
            module: self.resolved.canonical_id,
            span,
        })
    }

    /// The current call stack (E§8.2), innermost frame first: each frame's callable (a
    /// fresh host-owned handle), its call-site position, and its tail-iteration count.
    /// Mints one handle per callable frame — like [`list_get`](Instance::list_get), the
    /// host owns and must [`release`](Instance::release) them.
    pub fn stack_walk(&mut self) -> Vec<FrameObservation> {
        // Collect the raw per-frame data first (an immutable borrow of frames + AST),
        // then mint the callable handles (a mutable borrow) — the two borrows cannot
        // overlap. Innermost first, so reverse the bottom-up frame stack.
        let raw: Vec<(Option<super::CalIdx>, Option<Span>, u64)> = self
            .machine
            .frames
            .iter()
            .rev()
            .map(|frame| {
                let cal = match frame.kind {
                    FrameKind::Callable { cal } => Some(cal),
                    FrameKind::Block { .. } | FrameKind::ModuleTopLevel => None,
                };
                let call_site = frame.call_site.map(|node| self.resolved.ast.span(node));
                (cal, call_site, frame.tail_count)
            })
            .collect();
        raw.into_iter()
            .map(|(cal, call_site, tail_count)| FrameObservation {
                callable: cal.map(|cal| self.intern(Value::Callable(cal))),
                call_site,
                tail_count,
            })
            .collect()
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
            .filter_map(|frame| frame.call_site.map(|node| self.resolved.ast.span(node)))
            .collect()
    }

    /// The span the construct a continuation is at occupies (E§8.1, MD §17), or `None`
    /// for a span-less marker (a `ReturnBarrier`). Conts that carry an operator/operand
    /// span report it directly; those that name a node resolve it through the AST.
    fn cont_span(&self, cont: &Cont) -> Option<Span> {
        let ast = &self.resolved.ast;
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
            Cont::DictGotKey { dict, .. } | Cont::DictGotValue { dict, .. } => ast.span(*dict),
            Cont::FieldRead { field } => ast.span(*field),
            Cont::BindDefault { default, .. } => ast.span(*default),
            Cont::DefineCallable { decl } => ast.span(*decl),
            Cont::DefineRecord { decl } => ast.span(*decl),
            Cont::ExitApply { exit } => ast.span(*exit),
            Cont::TryHandler { try_node } => ast.span(*try_node),
            // A cleanup marker restores a binding; it occupies no construct of its own.
            Cont::WithRestore { .. } => return None,
            // Operator plumbing carries the operator's span directly.
            Cont::BinRhs { span, .. }
            | Cont::BinApply { span, .. }
            | Cont::UnaryApply { span, .. }
            | Cont::AndRhs { span, .. }
            | Cont::OrRhs { span, .. }
            | Cont::IndexGotObject { span, .. }
            | Cont::IndexApply { span, .. }
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
        let stmts = match self.resolved.ast.node(block) {
            Node::Module { stmts, .. } | Node::Block(stmts) => stmts,
            _ => return None,
        };
        stmts.get(next as usize).copied()
    }
}
