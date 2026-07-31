//! Execution frames (machine-design §8): one frame per active callable/block body
//! plus the module top level, each carrying a LIFO continuation stack.
//!
//! **Scope (M2a.6).** The module-top-level, callable (`to`/`fn`/lambda), and
//! block (`do … end`) frames. A block frame carries its **defining** link (the
//! static-link parent whose locals it reads, §7) and its **consumer** (who
//! invoked it, for `break`, §12); a callable frame carries the invoked callable
//! value. Every frame carries a `serial` (frame identity, preserved across tail
//! reuse at M2a.7) and, for a callee with a `do name` block parameter, the bound
//! [`BlockDescriptor`]. The dynamic-parameter depth and tail counter join at
//! M4/M2a.7.

use super::Value;
use super::cont::Cont;
use crate::ast::NodeId;
use crate::machine::CalIdx;

/// What a frame is running (machine-design §8 `FrameKind`).
pub(crate) enum FrameKind {
    /// The module top level. A module runs for effect, so it completes Void
    /// (L§6.11); at M1/M2a there is a single module, so the id is implicit.
    ModuleTopLevel,
    /// A `to`/`fn`/lambda body. Holds the **invoked callable value** (`CalIdx`),
    /// not just its `CallableId`: E§8.2 hands the frame's callable back as a
    /// handle and callable equality is identity (L§4.9), so the frame must carry
    /// the same `CalObj` the program called (machine-design §8). Its `CallableId`
    /// (via the `CalObj`) supplies the body kind for the return contract.
    Callable {
        /// The invoked callable value.
        cal: CalIdx,
    },
    /// A `do … end` block body (second-class, §8.5). Reads its enclosing locals
    /// through the **defining** static link (§7), not a capture; `break` exits its
    /// **consumer** (§12). A block yields its last expression's value to its
    /// invoker on each invocation.
    Block {
        /// The static-link parent frame (index into the frame stack) whose locals
        /// the block reads — the frame the block was *defined* in (§7).
        defining: usize,
        /// The defining frame's `serial`, checked on each static-link chase to
        /// catch a stale link (a reused frame slot; always valid until M2a.7).
        defining_serial: u64,
        /// Who invoked this block — the target `break` exits (§12).
        consumer: Consumer,
    },
}

/// Who received/invoked a block — the `break` target (machine-design §8/§12).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Consumer {
    /// A non-tail Doodle consumer: the frame that invoked the block (`break`
    /// unwinds through it inclusive, delivering the value as its call's result).
    DoodleCall {
        /// The invoking frame's index in the frame stack.
        frame: usize,
        /// The invoking frame's `serial` (integrity check).
        serial: u64,
    },
    // `TailReused` (M2a.7) and `Native { drive_depth }` (§14/M2b) join later.
}

/// A bound `do name` block parameter (machine-design §8/§10): a machine-internal
/// handle to a block argument — **never a Doodle value** (§8.5), so it lives here
/// on the frame rather than in `locals`. Invoking it pushes a [`FrameKind::Block`]
/// frame whose `defining`/`defining_serial` come from this descriptor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockDescriptor {
    /// The frame the block was defined in (its static-link parent, §7).
    pub(crate) defining: usize,
    /// That frame's `serial` at capture (integrity check).
    pub(crate) defining_serial: u64,
    /// The block body's `CallableId` (a [`crate::resolve::BodyKind::Block`]).
    pub(crate) callable: u32,
}

/// An execution frame: its kind, its local slots, and its continuation stack —
/// the frame's pending work, top = next to run (machine-design §8).
pub(crate) struct Frame {
    /// What this frame is running.
    pub(crate) kind: FrameKind,
    /// Frame-local slots (machine-design §7), sized by the body's resolver
    /// `slot_count`. `None` = an uninitialized slot (declared but not yet bound).
    /// A cell-boxed slot (closure capture, §7) arrives at M2a.8.
    pub(crate) locals: Vec<Option<Value>>,
    /// Pending work for this frame (LIFO).
    pub(crate) conts: Vec<Cont>,
    /// Frame identity (machine-design §8): distinguishes this activation from a
    /// reused frame slot; preserved across tail reuse (M2a.7).
    pub(crate) serial: u64,
    /// The bound `do name` block parameter, if this callee received one.
    pub(crate) block_param: Option<BlockDescriptor>,
}

impl Frame {
    /// A fresh module-top-level frame with `slot_count` (uninitialized) local
    /// slots, whose only pending work is `body_seq` (the sequencing continuation
    /// over the module's statements).
    pub(crate) fn module_top_level(slot_count: u16, body_seq: Cont, serial: u64) -> Self {
        Frame {
            kind: FrameKind::ModuleTopLevel,
            locals: vec![None; slot_count as usize],
            conts: vec![body_seq],
            serial,
            block_param: None,
        }
    }

    /// A fresh callable frame for invoking `cal`, with `locals` already holding
    /// the bound arguments (unfilled slots `None`) and `block_param` the bound
    /// `do name` block argument, if any. Its pending work is the body `Seq` over a
    /// [`ReturnBarrier`](Cont::ReturnBarrier) (machine-design §8/§10); parameter
    /// defaults are pushed on top by the caller so they run before the body.
    pub(crate) fn callable(
        cal: CalIdx,
        locals: Vec<Option<Value>>,
        body: NodeId,
        serial: u64,
        block_param: Option<BlockDescriptor>,
    ) -> Self {
        Frame {
            kind: FrameKind::Callable { cal },
            locals,
            conts: vec![
                Cont::ReturnBarrier,
                Cont::Seq {
                    block: body,
                    next: 0,
                },
            ],
            serial,
            block_param,
        }
    }

    /// A fresh block frame for one invocation of a `do … end` block, with `locals`
    /// holding the block's bound parameters. Its `defining`/`defining_serial` come
    /// from the [`BlockDescriptor`]; its `consumer` is who invoked it (§12). Like a
    /// callable, its work is the body `Seq` over a [`ReturnBarrier`](Cont::ReturnBarrier),
    /// which delivers the block's value to the invoker (§8.5).
    pub(crate) fn block(
        defining: usize,
        defining_serial: u64,
        consumer: Consumer,
        locals: Vec<Option<Value>>,
        body: NodeId,
        serial: u64,
    ) -> Self {
        Frame {
            kind: FrameKind::Block {
                defining,
                defining_serial,
                consumer,
            },
            locals,
            conts: vec![
                Cont::ReturnBarrier,
                Cont::Seq {
                    block: body,
                    next: 0,
                },
            ],
            serial,
            block_param: None,
        }
    }
}
