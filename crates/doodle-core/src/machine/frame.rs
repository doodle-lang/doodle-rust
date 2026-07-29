//! Execution frames (machine-design §8): one frame per active callable/block body
//! plus the module top level, each carrying a LIFO continuation stack.
//!
//! **Scope (M2a.5).** The module-top-level frame and callable (`to`/`fn`/lambda)
//! frames. Block frames (with `defining`/`consumer` links), the tail-iteration
//! counter, and the frame `serial` identity arrive at M2a.6/M2a.7 as the
//! blocks/PTC chunks need them.

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
    /// (via the `CalObj`) supplies the body kind for the return contract. `Block`
    /// joins at M2a.6.
    Callable {
        /// The invoked callable value.
        cal: CalIdx,
    },
}

/// An execution frame: its kind, its local slots, and its continuation stack —
/// the frame's pending work, top = next to run (machine-design §8).
pub(crate) struct Frame {
    /// What this frame is running.
    pub(crate) kind: FrameKind,
    /// Frame-local slots (machine-design §7), sized by the body's resolver
    /// `slot_count`. `None` = an uninitialized slot (declared but not yet bound).
    /// A cell-boxed slot (closure capture, §7) holds a cell reference; those
    /// arrive at M2a.6/M2a.8. `serial`/`block_param`/… join in later chunks.
    pub(crate) locals: Vec<Option<Value>>,
    /// Pending work for this frame (LIFO).
    pub(crate) conts: Vec<Cont>,
}

impl Frame {
    /// A fresh module-top-level frame with `slot_count` (uninitialized) local
    /// slots, whose only pending work is `body_seq` (the sequencing continuation
    /// over the module's statements).
    pub(crate) fn module_top_level(slot_count: u16, body_seq: Cont) -> Self {
        Frame {
            kind: FrameKind::ModuleTopLevel,
            locals: vec![None; slot_count as usize],
            conts: vec![body_seq],
        }
    }

    /// A fresh callable frame for invoking `cal`, with `locals` already holding
    /// the bound arguments (unfilled slots `None`). Its pending work is the body
    /// `Seq` over a [`ReturnBarrier`](Cont::ReturnBarrier): when the body drains,
    /// the barrier delivers the result (machine-design §8/§10). Any parameter
    /// defaults are pushed on top by the caller so they run before the body.
    pub(crate) fn callable(cal: CalIdx, locals: Vec<Option<Value>>, body: NodeId) -> Self {
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
        }
    }
}
