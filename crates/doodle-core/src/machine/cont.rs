//! Continuations (machine-design §8): defunctionalized pending work on a frame's
//! LIFO stack. `step` pops the top continuation and performs one transition.
//!
//! **Scope (M2a.2).** Statement sequencing (`Seq`) and expression evaluation
//! (`Eval`) for literals. The expression-plumbing, call, binding, control, and
//! cleanup continuation categories (machine-design §8) join in their chunks
//! (M2a.3+); each new variant falls into one of those pinned categories.

use crate::ast::NodeId;

/// One unit of pending work on a frame's continuation stack (machine-design §8).
pub(crate) enum Cont {
    /// Sequence a body's statements — a statement boundary is a safe point
    /// (machine-design §9). Run the statement at index `next` in `block`'s
    /// statement list, then continue with `next + 1`; when `next` reaches the
    /// end the body is done. `block` is a `Module` or `Block` node.
    Seq {
        /// The body node (`Module`/`Block`) being sequenced.
        block: NodeId,
        /// The index of the next statement to run.
        next: u32,
    },
    /// Evaluate the expression `node` into the result register.
    Eval {
        /// The expression node to evaluate.
        node: NodeId,
    },
}
