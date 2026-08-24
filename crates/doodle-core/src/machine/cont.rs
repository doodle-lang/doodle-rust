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
    /// An assignment to a **name** target: its right-hand value is now in the
    /// register — write it to the binding (a module cell or a frame slot), copying a
    /// value record for binding (L§4.14). The statement yields Void. A `Field`/`Index`
    /// place target instead takes the [`AssignPlaceObj`](Cont::AssignPlaceObj) path.
    AssignTo {
        /// The `Assign` node (its `target` is an `Ident` lvalue).
        assign: NodeId,
    },
    /// A place assignment's target **object** (`a.b` in `a.b.c = v`, `d` in `d[k] = v`)
    /// is now in the register — the *actual* object, navigated with no copy (L§5.3, the
    /// S-38 "no intermediate copies" rule). Branch on the target kind to finish the
    /// store: a `Field` target evaluates the RHS next, an `Index` target the key.
    AssignPlaceObj {
        /// The `Assign` node (its `target` is a `Field`/`Index` lvalue).
        assign: NodeId,
    },
    /// A field place assignment with its target object saved: the RHS is now in the
    /// register. Copy it for binding and write it into `object`'s field (L§5.3/§9).
    AssignFieldVal {
        /// The `Assign` node (its `target` names the field).
        assign: NodeId,
        /// The object being mutated (a record place, no copy).
        object: Value,
    },
    /// An index place assignment with its target object saved: the key is now in the
    /// register. Stash the key, then evaluate the RHS (left-to-right, L§14).
    AssignIndexKey {
        /// The `Assign` node (its `target` is the `Index` lvalue).
        assign: NodeId,
        /// The object being indexed (a dict place, no copy).
        object: Value,
    },
    /// An index place assignment with its object and key saved: the RHS is now in the
    /// register. Copy it for binding and store it under the key (L§5.3/§4.8).
    AssignIndexVal {
        /// The `Assign` node (for its span).
        assign: NodeId,
        /// The object being indexed (a dict place, no copy).
        object: Value,
        /// The already-evaluated key.
        key: Value,
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
    /// A field expression's object (`r` in `r.name`) is now in the register: read the
    /// named field (L§9). The `Field` node names both the object and the field.
    FieldRead {
        /// The `Field` node (for its field name and span).
        field: NodeId,
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
    /// Bind a `record` declaration's type value to its name (L§9) when the statement
    /// runs. The body is docstring-only, so there is nothing to evaluate first.
    DefineRecord {
        /// The `Record` declaration node.
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
    /// **Cleanup** (machine-design §12/§13): restore a dynamic-parameter binding. On
    /// normal completion of a `with` body, and as the unwinder pops past this cont on
    /// **any** exit (break/continue/return/raise/cancel), pop `dyn_stack` down to
    /// `dyn_mark`, writing each saved value back into its cell. The producer (`with`)
    /// is M4.6; the restore mechanism and its execution during unwind are M4.5a.
    // Constructed by the `with` producer (M4.6); the M4.5a mechanism matches it only.
    #[allow(dead_code)]
    WithRestore {
        /// The `dyn_stack` length to restore to — entries above it are this `with`'s.
        dyn_mark: u32,
    },
    /// **Cleanup (raise-only)** (machine-design §12): a `try`'s rescue handler. On
    /// normal completion of the protected body this pops inertly (the body's value is
    /// the `try`'s value). Only a **raise** unwind stops here — binding the exception
    /// and entering the rescue body. The bind-and-enter is M4.5b; M4.5a adds the cont
    /// and the unwinder's recognition of it as the raise catch-point.
    // Constructed by the `try` producer (M4.5b); the M4.5a mechanism matches it only.
    #[allow(dead_code)]
    TryHandler {
        /// The `Try` node (its `rescue_name`/`rescue_body` drive the M4.5b catch).
        try_node: NodeId,
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
            Cont::AssignFieldVal { object, .. } | Cont::AssignIndexKey { object, .. } => f(*object),
            Cont::AssignIndexVal { object, key, .. } => {
                f(*object);
                f(*key);
            }
            // Value-free: NodeIds, slots, spans, operators only.
            Cont::Seq { .. }
            | Cont::AssignPlaceObj { .. }
            | Cont::FieldRead { .. }
            | Cont::DefineRecord { .. }
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
            | Cont::ExitApply { .. }
            | Cont::WithRestore { .. }
            | Cont::TryHandler { .. } => {}
        }
    }
}
