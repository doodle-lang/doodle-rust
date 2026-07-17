//! The S-5 fn-falls-off-end check (M1.10c): a post-pass classifying each `fn`
//! body's tail into the four-way lattice (produces / diverges / value-less /
//! indeterminate) and reporting a body that is statically **value-less** (L§8.4).
//! Only `fn` bodies are checked — a `to` yields no value, and a block's value is
//! its consumer's concern. The classifier is **condition-blind**: it judges
//! syntactic form, not path feasibility, so dead-tail code is rejected by design
//! (per the ruling). Indeterminate tails (a call whose proc/fn nature isn't
//! lexically known) are deferred to the runtime consuming-site check (S-6).

use super::Resolver;
use crate::ast::{IfArm, Node, NodeId};
use crate::diag::code::DiagnosticCode;
use crate::resolve::{BodyKind, GlobalKind, Resolution};

/// The value-production class of a tail statement/expression (S-5).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    Produces,
    Diverges,
    ValueLess,
    Indeterminate,
}

impl Resolver<'_> {
    /// Reports each `fn` whose body statically falls off the end (S-5).
    pub(super) fn check_fn_tails(&mut self) {
        // Collect first: emitting errors while borrowing `callables` would alias.
        let fn_bodies: Vec<NodeId> = self
            .callables
            .iter()
            .filter(|c| c.kind == BodyKind::Func)
            .map(|c| c.body)
            .collect();
        for body in fn_bodies {
            if self.tail_of_block(body) == Tail::ValueLess {
                let node = self.last_stmt(body).unwrap_or(body);
                let msg = if matches!(self.ast.node(node), Node::While { .. }) {
                    "this `fn` ends with a `while`, which yields no value — if you \
                     meant to loop forever use `loop`; otherwise a function must \
                     produce a value"
                } else {
                    "this `fn` can finish without producing a value — a function \
                     must return a value (`to` is the no-value procedure form)"
                };
                self.error(DiagnosticCode::FunctionFallsOffEnd, node, msg);
            }
        }
    }

    /// The tail class of a body [`Node::Block`]: its last statement, or
    /// `ValueLess` if empty (an empty `fn` body produces nothing).
    fn tail_of_block(&self, block: NodeId) -> Tail {
        match self.last_stmt(block) {
            Some(stmt) => self.classify_stmt(stmt),
            None => Tail::ValueLess,
        }
    }

    /// The last statement of a [`Node::Block`], or `None` if it is empty or not a
    /// block. Shared with the S-6 consuming-site check (`voidcheck`).
    pub(super) fn last_stmt(&self, block: NodeId) -> Option<NodeId> {
        match self.ast.node(block) {
            Node::Block(stmts) => stmts.last().copied(),
            _ => None,
        }
    }

    /// Classifies a tail statement (S-5), recursing over `if`/`try` branches.
    fn classify_stmt(&self, node: NodeId) -> Tail {
        match self.ast.node(node) {
            Node::ExprStmt(e) => self.classify_value(*e),
            // `return expr` doesn't fall off the end — it produces (S-5). If the
            // operand is itself value-less (`return p()` for a `to` p), that is a
            // consuming-site error at the `return` operand, owned by S-6 — not this
            // check (mislabeling it "falls off the end" would misdirect).
            Node::Return(Some(_)) => Tail::Produces,
            Node::Raise(_) => Tail::Diverges,
            // A `break`/`continue` at a fn tail is already a misplaced-exit error;
            // classify as diverging so we don't also report falls-off-end.
            Node::Break(_) | Node::Continue(_) => Tail::Diverges,
            // A `loop` diverges unless a `break` is lexically bound to it.
            Node::Loop { .. } => {
                if self.loops_with_break.contains(&node) {
                    Tail::ValueLess
                } else {
                    Tail::Diverges
                }
            }
            Node::Let { .. }
            | Node::Const { .. }
            | Node::Assign { .. }
            | Node::While { .. }
            | Node::With { .. }
            | Node::Return(None) => Tail::ValueLess,
            Node::If { arms, else_body } => self.classify_if(arms, *else_body),
            Node::Try {
                body, rescue_body, ..
            } => combine(self.tail_of_block(*body), self.tail_of_block(*rescue_body)),
            // Declarations and anything else in statement position yield no value.
            _ => Tail::ValueLess,
        }
    }

    /// Classifies a fn body's tail *expression* (an expression statement whose
    /// value is the function's result).
    fn classify_value(&self, node: NodeId) -> Tail {
        match self.ast.node(node) {
            Node::Call { callee, .. } => self.classify_call(*callee),
            Node::If { arms, else_body } => self.classify_if(arms, *else_body),
            Node::Try {
                body, rescue_body, ..
            } => combine(self.tail_of_block(*body), self.tail_of_block(*rescue_body)),
            // Every other expression form (literal, name, operator, anon `fn`, …)
            // yields a value.
            _ => Tail::Produces,
        }
    }

    /// A call's value-ness from its callee's declaration kind: a same-module `to`
    /// yields Void (value-less); a `fn` yields a value; anything else (a local,
    /// param, capture, import, or unknown callable) is indeterminate → runtime.
    fn classify_call(&self, callee: NodeId) -> Tail {
        if !matches!(self.ast.node(callee), Node::Ident(_)) {
            return Tail::Indeterminate;
        }
        match self.resolutions[callee.0 as usize] {
            Some(Resolution::ModuleName(idx)) => {
                let name = self.name_refs[idx as usize].name.as_ref();
                match self
                    .globals
                    .iter()
                    .find(|g| g.name.as_ref() == name)
                    .map(|g| g.kind)
                {
                    Some(GlobalKind::Proc) => Tail::ValueLess,
                    Some(GlobalKind::Fn) => Tail::Produces,
                    _ => Tail::Indeterminate,
                }
            }
            _ => Tail::Indeterminate,
        }
    }

    fn classify_if(&self, arms: &[IfArm], else_body: Option<NodeId>) -> Tail {
        // A missing `else` is an implicit value-less branch.
        let Some(else_body) = else_body else {
            return Tail::ValueLess;
        };
        let mut acc = self.tail_of_block(else_body);
        for arm in arms {
            acc = combine(acc, self.tail_of_block(arm.body));
        }
        acc
    }
}

/// Branch combination (S-5): any value-less branch → value-less; else any
/// indeterminate → indeterminate; else (all produce/diverge) → produces.
fn combine(a: Tail, b: Tail) -> Tail {
    if a == Tail::ValueLess || b == Tail::ValueLess {
        Tail::ValueLess
    } else if a == Tail::Indeterminate || b == Tail::Indeterminate {
        Tail::Indeterminate
    } else {
        Tail::Produces
    }
}
