//! Execution frames (machine-design §8): one frame per active callable/block body
//! plus the module top level, each carrying a LIFO continuation stack.
//!
//! **Scope (M2a.2).** The module-top-level frame and its continuation stack.
//! Callable and block frames — with `locals`, block parameters, the tail-iteration
//! counter, and the frame `serial` identity — arrive at M2a.5/M2a.6/M2a.7 as the
//! calls/blocks/PTC chunks need them.

use super::cont::Cont;

/// What a frame is running (machine-design §8 `FrameKind`).
pub(crate) enum FrameKind {
    /// The module top level. A module runs for effect, so it completes Void
    /// (L§6.11); at M1/M2a there is a single module, so the id is implicit.
    /// `Callable { .. }` and `Block { .. }` join at M2a.5/M2a.6.
    ModuleTopLevel,
}

/// An execution frame: its kind and its continuation stack — the frame's pending
/// work, top = next to run (machine-design §8).
pub(crate) struct Frame {
    /// What this frame is running.
    pub(crate) kind: FrameKind,
    /// Pending work for this frame (LIFO). `locals`/`serial`/`block_param`/… join
    /// in the chunks that first need them.
    pub(crate) conts: Vec<Cont>,
}

impl Frame {
    /// A fresh module-top-level frame whose only pending work is `body_seq`
    /// (the sequencing continuation over the module's statements).
    pub(crate) fn module_top_level(body_seq: Cont) -> Self {
        Frame {
            kind: FrameKind::ModuleTopLevel,
            conts: vec![body_seq],
        }
    }
}
