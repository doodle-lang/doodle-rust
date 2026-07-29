//! Execution frames (machine-design §8): one frame per active callable/block body
//! plus the module top level, each carrying a LIFO continuation stack.
//!
//! **Scope (M2a.2).** The module-top-level frame and its continuation stack.
//! Callable and block frames — with `locals`, block parameters, the tail-iteration
//! counter, and the frame `serial` identity — arrive at M2a.5/M2a.6/M2a.7 as the
//! calls/blocks/PTC chunks need them.

use super::Value;
use super::cont::Cont;

/// What a frame is running (machine-design §8 `FrameKind`).
pub(crate) enum FrameKind {
    /// The module top level. A module runs for effect, so it completes Void
    /// (L§6.11); at M1/M2a there is a single module, so the id is implicit.
    /// `Callable { .. }` and `Block { .. }` join at M2a.5/M2a.6.
    ModuleTopLevel,
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
}
