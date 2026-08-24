//! Continuations (machine-design §8): defunctionalized pending work on a frame's
//! LIFO stack. `step` pops the top continuation and performs one transition.
//!
//! Every variant falls into one of the machine-design §8 categories: sequencing
//! (`Seq`), expression plumbing (`BinRhs`/`BinApply`/`UnaryApply`/`AndRhs`/…),
//! calls (`CallGotCallee`/`CallGotArg`/`BindDefault`), binding/assignment
//! (`BindLet`/`AssignTo`/`DefineCallable`), control (`IfChoose`/`WhileCheck`/
//! `LoopReloop`), and markers (`ReturnBarrier`). The cleanup category
//! (`WithRestore`/`TryHandler`) and the block/unwind machinery join at M2a.6.

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
    /// A `let`/`const` initializer is now in the register: bind it to the
    /// declaration's target (a module cell or a frame slot, per the resolver).
    /// The statement yields Void.
    BindLet {
        /// The `Let`/`Const` declaration node.
        decl: NodeId,
    },
    /// An assignment's right-hand value is now in the register: write it to the
    /// target lvalue (a module cell or a frame slot). The statement yields Void.
    AssignTo {
        /// The `Assign` node (its `target` is the lvalue).
        assign: NodeId,
    },
    /// An `if` arm's condition is now in the register: it must be a `Bool`; if
    /// true run that arm's body, else advance to the next arm / `else` / nothing
    /// (L§6.8/§7.5). Carries the `If` node + the arm being tested.
    IfChoose {
        /// The `If` node.
        node: NodeId,
        /// The arm index whose condition is in the register.
        index: u32,
    },
    /// A `while` condition is now in the register: it must be a `Bool`; if true
    /// run the body then re-check, else the loop is done (L§7.6). Carries the
    /// `While` node (so a `break`/`continue` can target it, M2a.6).
    WhileCheck {
        /// The `While` node.
        node: NodeId,
    },
    /// Re-enter a `loop` body (L§7.7): run the body, then loop again. Carries the
    /// `Loop` node (a `break`/`continue` target, M2a.6). Unbounded until a
    /// `break`/`return` (M2a.5/M2a.6) or a limit (M2a.9).
    LoopReloop {
        /// The `Loop` node.
        node: NodeId,
    },
    /// A call's callee is now in the register: begin evaluating its arguments
    /// left to right (L§8.3, L§14), or, with no arguments, apply immediately
    /// (machine-design §8/§10, calls).
    CallGotCallee {
        /// The `Call` node (its `args`/`block` drive binding).
        call: NodeId,
    },
    /// A call's argument at `index` is now in the register: stash it and evaluate
    /// the next argument, or, when the last argument is in, apply. Holds the
    /// already-evaluated callee and prior argument values, so it is a GC root
    /// (machine-design §8).
    CallGotArg {
        /// The `Call` node.
        call: NodeId,
        /// The already-evaluated callee (a `Callable`).
        callee: Value,
        /// The argument values evaluated so far, in source order.
        values: Vec<Value>,
        /// The index of the argument now in the register.
        index: u32,
    },
    /// A **block invocation**'s argument at `index` is now in the register: stash
    /// it and evaluate the next, or invoke the block once the last is in (§8.5).
    /// Unlike [`CallGotArg`](Cont::CallGotArg) there is no callee value — the block
    /// descriptor lives on the invoking frame (machine-design §8/§10).
    BlockGotArg {
        /// The `Call` node invoking the block parameter.
        call: NodeId,
        /// The argument values evaluated so far, in source order.
        values: Vec<Value>,
        /// The index of the argument now in the register.
        index: u32,
    },
    /// A list literal's element at `index` is now in the register: stash it, then
    /// evaluate the next element or allocate the list once the last is in (L§4.6).
    ListGotElem {
        /// The `List` literal node.
        list: NodeId,
        /// The element values evaluated so far, in order.
        values: Vec<Value>,
        /// The index of the element now in the register.
        index: u32,
    },
    /// A dict literal entry's **computed** key is now in the register: pair it with
    /// the entry's value expression to evaluate next (L§4.8). (A bare-word key needs
    /// no eval, so it skips straight to [`DictGotValue`](Cont::DictGotValue).)
    DictGotKey {
        /// The `Dict` literal node.
        dict: NodeId,
        /// The `(key, value)` pairs completed so far, in insertion order.
        entries: Vec<(Value, Value)>,
        /// The index of the entry whose key is now in the register.
        index: u32,
    },
    /// A dict literal entry's value is now in the register: record `key → value`, then
    /// evaluate the next entry or build the dict once the last is in (L§4.8).
    DictGotValue {
        /// The `Dict` literal node.
        dict: NodeId,
        /// The `(key, value)` pairs completed so far, in insertion order.
        entries: Vec<(Value, Value)>,
        /// The index of the entry now completing.
        index: u32,
        /// This entry's already-evaluated key.
        key: Value,
    },
    /// An index expression's object (`d` in `d[k]`) is now in the register: stash it,
    /// then evaluate the key `k` (L§4.8).
    IndexGotObject {
        /// The key expression `k`.
        index: NodeId,
        /// The whole `Index` node's span, for a raise.
        span: Span,
    },
    /// An index expression's key is now in the register: look it up in the stashed
    /// object (L§4.8).
    IndexApply {
        /// The object being indexed.
        object: Value,
        /// The whole `Index` node's span, for a raise.
        span: Span,
    },
    /// A parameter default's value is now in the register: write it into the
    /// callee frame's slot (defaults are evaluated in the callee activation, L§8.2).
    BindDefault {
        /// The frame slot the default fills.
        slot: u16,
        /// The default expression (for its span, if it yields Void).
        default: NodeId,
    },
    /// Intern and bind a named `to`/`fn` declaration (machine-design §8): allocate
    /// its one canonical callable value and write it to the declaration's target
    /// (a module cell or a frame slot). The statement yields Void.
    DefineCallable {
        /// The `Callable` declaration node.
        decl: NodeId,
    },
    /// The root of a callable body's continuation stack (machine-design §8/§10):
    /// when it is reached the body is done, so the frame returns, delivering its
    /// result (a `fn`'s value; Void for a `to`). Also where a block delivers its
    /// yielded value to its invoker (§8.5).
    ReturnBarrier,
    /// A `return`/`break`/`continue` operand (if any) is now in the register: begin
    /// the non-local transfer to the exit's resolver-annotated target (§12).
    ExitApply {
        /// The `Return`/`Break`/`Continue` node.
        exit: NodeId,
    },
}

impl Cont {
    /// Invokes `f` with each [`Value`] this continuation holds — its GC roots
    /// (machine-design §15). Only the plumbing variants that stash an
    /// already-evaluated operand carry values; every other variant holds just
    /// `NodeId`s, slots, and spans. The match is **exhaustive on purpose**: a new
    /// value-carrying variant will fail to compile here until its values are
    /// enumerated, so a live value can never silently escape the root set.
    pub(crate) fn each_value(&self, mut f: impl FnMut(Value)) {
        match self {
            Cont::BinApply { lhs, .. } => f(*lhs),
            Cont::CallGotArg { callee, values, .. } => {
                f(*callee);
                values.iter().copied().for_each(&mut f);
            }
            Cont::BlockGotArg { values, .. } => values.iter().copied().for_each(f),
            Cont::ListGotElem { values, .. } => values.iter().copied().for_each(f),
            Cont::DictGotKey { entries, .. } => entries.iter().for_each(|(k, v)| {
                f(*k);
                f(*v);
            }),
            Cont::DictGotValue { entries, key, .. } => {
                entries.iter().for_each(|(k, v)| {
                    f(*k);
                    f(*v);
                });
                f(*key);
            }
            Cont::IndexApply { object, .. } => f(*object),
            // Value-free: NodeIds, slots, spans, operators only.
            Cont::Seq { .. }
            | Cont::IndexGotObject { .. }
            | Cont::Eval { .. }
            | Cont::BinRhs { .. }
            | Cont::UnaryApply { .. }
            | Cont::AndRhs { .. }
            | Cont::OrRhs { .. }
            | Cont::AssertBool { .. }
            | Cont::BindLet { .. }
            | Cont::AssignTo { .. }
            | Cont::IfChoose { .. }
            | Cont::WhileCheck { .. }
            | Cont::LoopReloop { .. }
            | Cont::CallGotCallee { .. }
            | Cont::BindDefault { .. }
            | Cont::DefineCallable { .. }
            | Cont::ReturnBarrier
            | Cont::ExitApply { .. } => {}
        }
    }
}
