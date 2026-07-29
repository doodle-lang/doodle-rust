//! Continuations (machine-design §8): defunctionalized pending work on a frame's
//! LIFO stack. `step` pops the top continuation and performs one transition.
//!
//! **Scope (M2a.2).** Statement sequencing (`Seq`) and expression evaluation
//! (`Eval`) for literals. The expression-plumbing, call, binding, control, and
//! cleanup continuation categories (machine-design §8) join in their chunks
//! (M2a.3+); each new variant falls into one of those pinned categories.

use crate::ast::{BinaryOp, NodeId, UnaryOp};
use crate::machine::Value;
use crate::span::Span;

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
    /// A binary operator whose left operand is now in the register: stash it and
    /// evaluate the right operand (machine-design §8, expression plumbing).
    BinRhs {
        /// The operator.
        op: BinaryOp,
        /// The right-operand expression.
        rhs: NodeId,
        /// The operator's span, for a raise's position.
        span: Span,
    },
    /// A binary operator whose right operand is now in the register, with the
    /// left operand saved: apply the operator. Holds a `Value`, so it is a GC
    /// root (machine-design §8).
    BinApply {
        /// The operator.
        op: BinaryOp,
        /// The already-evaluated left operand.
        lhs: Value,
        /// The operator's span, for a raise's position.
        span: Span,
    },
    /// A unary operator whose operand is now in the register: apply the operator.
    UnaryApply {
        /// The operator.
        op: UnaryOp,
        /// The operator's span, for a raise's position.
        span: Span,
    },
    /// `and` whose left operand is now in the register: it must be a `Bool`; if
    /// `false`, short-circuit to `false`, else evaluate the right operand
    /// (L§6.6). The right operand's result becomes the `and`'s value, checked by
    /// [`Cont::AssertBool`].
    AndRhs {
        /// The right-operand expression.
        rhs: NodeId,
        /// The operator's span, for a raise's position.
        span: Span,
    },
    /// `or` whose left operand is now in the register: it must be a `Bool`; if
    /// `true`, short-circuit to `true`, else evaluate the right operand.
    OrRhs {
        /// The right-operand expression.
        rhs: NodeId,
        /// The operator's span, for a raise's position.
        span: Span,
    },
    /// The right operand of an `and`/`or` is now in the register: it must be a
    /// `Bool`, and it is the operator's result (strict booleans, L§4.3).
    AssertBool {
        /// The operator's span, for a raise's position.
        span: Span,
    },
}
