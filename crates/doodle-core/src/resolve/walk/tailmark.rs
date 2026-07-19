//! Tail-call marking (M1.11): a post-pass over each callable/block body marking
//! every `Call` node in **tail position** (L§8.7, machine-design §11), so the M2a
//! machine can reuse the frame instead of growing the control stack — when the
//! callee's kind matches (that apply-time check is M2a's; marking here is
//! kind-agnostic, S-55).
//!
//! A call is tail iff its completion is the enclosing callable's completion (an
//! `fn`'s value, a `to`'s final action — procedures have tail positions too, S-55)
//! with **no pending work**, it is not inside a `with` body (a dynamic binding must
//! be restored on exit) or a `try` body/handler (a handler must be unwound), and it
//! passes **no block argument** (S-45 — a `do … end` block references the caller's
//! frame, so that frame cannot be reused). Only `to`/`fn`/`do` bodies have tail
//! positions; the module top level is not a callable, so it is skipped.
//!
//! Fall-through is not the only source of a tail position: a `return expr`
//! delivers `expr` as the callable's result *wherever the `return` sits* (it
//! abandons all surrounding work), so the walk visits every child to find
//! `return` operands, but propagates the fall-through `tail` flag only along the
//! fall-through edges (a body's last statement, a tail `if`'s selected branch).

use super::Resolver;
use crate::ast::{Arg, DictKey, Node, NodeId, StrPart};
use crate::resolve::BodyKind;

impl Resolver<'_> {
    /// Marks every tail-position call across all `to`/`fn`/`do` bodies.
    pub(super) fn mark_tail_calls(&mut self) {
        // Collect (body, ret_is_tail) first: marking while borrowing `callables`
        // would alias. `ret_is_tail` is whether a `return` in this body targets
        // *this* frame — true for a `to`/`fn` (the `return`'s home callable), false
        // for a block (a `return` there is a non-local exit to an outer callable,
        // not this block's tail).
        let bodies: Vec<(NodeId, bool)> = self
            .callables
            .iter()
            .filter_map(|c| match c.kind {
                BodyKind::Proc | BodyKind::Func => Some((c.body, true)),
                BodyKind::Block => Some((c.body, false)),
                BodyKind::ModuleTopLevel => None,
            })
            .collect();
        for (body, ret_is_tail) in bodies {
            self.mark_block_tail(body, true, ret_is_tail);
        }
    }

    /// Marks tail calls in a body [`Node::Block`]: only its **last** statement is a
    /// fall-through tail position, and then only if `tail` (the block itself is in
    /// tail position — a body root, or a tail `if`'s branch).
    fn mark_block_tail(&mut self, block: NodeId, tail: bool, ret_is_tail: bool) {
        let Node::Block(stmts) = self.ast.node(block) else {
            return;
        };
        let stmts = stmts.clone();
        let last = stmts.len().saturating_sub(1);
        for (i, &stmt) in stmts.iter().enumerate() {
            self.mark_tail(stmt, tail && i == last, ret_is_tail);
        }
    }

    /// Marks tail calls reachable from `node`. `tail` = `node`'s value falls through
    /// as the frame's result with no work pending after it. The walk descends into
    /// every child (to reach `return` operands, tail wherever they sit) but
    /// propagates `tail` only along fall-through edges; it stops at frame boundaries
    /// (nested callables, block args — each its own [`CallableInfo`]) and at the
    /// `with`/`try` barriers (calls inside them need post-return cleanup).
    fn mark_tail(&mut self, node: NodeId, tail: bool, ret_is_tail: bool) {
        match self.ast.node(node) {
            Node::Call {
                callee,
                args,
                block,
            } => {
                let (callee, args, has_block) = (*callee, args.clone(), block.is_some());
                // Tail iff in a fall-through tail slot and passing no block (S-45).
                if tail && !has_block {
                    self.tail_calls[node.0 as usize] = true;
                }
                // Callee/args evaluate *before* the call, so none is tail — but
                // descend for any `return` nested in them. The `do … end` block arg
                // is a separate frame, not crossed here.
                self.mark_tail(callee, false, ret_is_tail);
                for arg in &args {
                    let e = match arg {
                        Arg::Positional(e) => *e,
                        Arg::Keyword { value, .. } => *value,
                    };
                    self.mark_tail(e, false, ret_is_tail);
                }
            }
            Node::ExprStmt(e) => self.mark_tail(*e, tail, ret_is_tail),
            // `return expr` delivers expr as this callable's result (in a `to`/`fn`
            // frame): expr is a tail position wherever the `return` sits. In a block
            // frame (`ret_is_tail` false) the `return` targets an outer callable, so
            // expr is not this frame's tail.
            Node::Return(Some(e)) => self.mark_tail(*e, ret_is_tail, ret_is_tail),
            Node::If { arms, else_body } => {
                let arms = arms.clone();
                let else_body = *else_body;
                for arm in arms {
                    self.mark_tail(arm.cond, false, ret_is_tail);
                    self.mark_block_tail(arm.body, tail, ret_is_tail);
                }
                if let Some(body) = else_body {
                    self.mark_block_tail(body, tail, ret_is_tail);
                }
            }
            // Barriers (L§8.7): a call in a `with` body (binding to restore) or a
            // `try` body/handler (handler to unwind) is never tail. Don't descend —
            // a nested callable inside is covered by its own `CallableInfo`. The
            // `with` *value* runs before the binding opens, so it is not barred.
            Node::With { value, .. } => self.mark_tail(*value, false, ret_is_tail),
            Node::Try { .. } => {}
            // Same frame, not a fall-through tail slot, but a `return` inside is
            // still a tail position — descend with `tail` false to reach it.
            Node::While { cond, body } => {
                let (cond, body) = (*cond, *body);
                self.mark_tail(cond, false, ret_is_tail);
                self.mark_block_tail(body, false, ret_is_tail);
            }
            Node::Loop { body } => self.mark_block_tail(*body, false, ret_is_tail),
            Node::Let { value, .. } | Node::Const { value, .. } => {
                self.mark_tail(*value, false, ret_is_tail);
            }
            Node::Assign { target, value } => {
                let (target, value) = (*target, *value);
                self.mark_tail(value, false, ret_is_tail);
                self.mark_tail(target, false, ret_is_tail);
            }
            // Operand carriers: never tail themselves; descend for nested `return`s.
            Node::Unary { operand, .. } => self.mark_tail(*operand, false, ret_is_tail),
            Node::Binary { lhs, rhs, .. } => {
                let (lhs, rhs) = (*lhs, *rhs);
                self.mark_tail(lhs, false, ret_is_tail);
                self.mark_tail(rhs, false, ret_is_tail);
            }
            Node::Field { object, .. } => self.mark_tail(*object, false, ret_is_tail),
            Node::Index { object, index } => {
                let (object, index) = (*object, *index);
                self.mark_tail(object, false, ret_is_tail);
                self.mark_tail(index, false, ret_is_tail);
            }
            Node::List(elems) => {
                for e in elems.clone() {
                    self.mark_tail(e, false, ret_is_tail);
                }
            }
            Node::Dict(entries) => {
                for entry in entries.clone() {
                    if let DictKey::Expr(k) = entry.key {
                        self.mark_tail(k, false, ret_is_tail);
                    }
                    self.mark_tail(entry.value, false, ret_is_tail);
                }
            }
            Node::StrLit(parts) => {
                for part in parts.clone() {
                    if let StrPart::Interp(e) = part {
                        self.mark_tail(e, false, ret_is_tail);
                    }
                }
            }
            Node::Break(op) | Node::Continue(op) | Node::Raise(op) => {
                if let Some(e) = *op {
                    self.mark_tail(e, false, ret_is_tail);
                }
            }
            // Leaves, bare `return`, and nested declarations (separate frames) end
            // the walk — nothing here is a tail position of this frame.
            _ => {}
        }
    }
}
