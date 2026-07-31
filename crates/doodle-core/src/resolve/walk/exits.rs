//! Non-local exit handling in the resolver walk (M1.10b): the lexical
//! exit-target annotation and placement checks for `return`/`break`/`continue`
//! (machine-design §12). Split from `walk/mod.rs` for length; part of the same
//! [`Resolver`](super::Resolver) walk. The `Ctrl` stack it consults is pushed by
//! `resolve_callable`/`resolve_block_arg`/`resolve_loop_body`.

use super::Ctrl;
use crate::ast::NodeId;
use crate::diag::code::DiagnosticCode;
use crate::resolve::{BodyKind, ExitTarget};

impl super::Resolver<'_> {
    /// Resolves a `while`/`loop` body: a construct scope plus a [`Ctrl::Loop`]
    /// context so `break`/`continue` inside it target this `loop_node`.
    pub(super) fn resolve_loop_body(&mut self, loop_node: NodeId, body: NodeId) {
        self.ctrl.push(Ctrl::Loop(loop_node));
        self.resolve_construct_body(body);
        self.ctrl.pop();
    }

    /// Annotates a `return` with its lexical target — the nearest enclosing
    /// callable (machine-design §12) — or reports a misplaced `return`. A **valued**
    /// `return` in a procedure (`to`) has no value destination and is rejected
    /// (L§8.4; the value half of the S-10 rule, alongside `valued-exit-in-loop`).
    pub(super) fn resolve_return(&mut self, node: NodeId, operand: Option<NodeId>) {
        if let Some(e) = operand {
            self.resolve(e);
        }
        // `if`/`with`/`try`/loops/blocks are transparent to `return`; the nearest
        // `Ctrl::Callable` is its home.
        let home_kind = self.ctrl.iter().rev().find_map(|c| match c {
            Ctrl::Callable(kind) => Some(*kind),
            _ => None,
        });
        match home_kind {
            Some(kind) => {
                self.set_exit(node, ExitTarget::HomeCallable);
                if operand.is_some() && kind == BodyKind::Proc {
                    self.error(
                        DiagnosticCode::ValuedReturnInProcedure,
                        node,
                        "a procedure yields no value, so `return` here can't take one — \
                         use a plain `return`, or make this a function (`fn`) if it \
                         should yield a value",
                    );
                }
            }
            None => self.exit_error(
                node,
                "`return` can only appear inside a procedure or function",
            ),
        }
    }

    /// Annotates a `break`/`continue` with its target — the nearest enclosing loop
    /// or block (machine-design §12), not crossing a callable boundary — or reports
    /// a misplaced exit.
    pub(super) fn resolve_break_continue(
        &mut self,
        node: NodeId,
        operand: Option<NodeId>,
        is_break: bool,
    ) {
        if let Some(e) = operand {
            self.resolve(e);
        }
        // `if`/`with`/`try` don't push `ctrl`, so the innermost `ctrl` entry IS
        // the nearest enclosing control context. A callable there is a barrier
        // (break/continue can't escape it to an outer loop) → misplaced.
        let target = match self.ctrl.last() {
            Some(Ctrl::Loop(loop_node)) => Some(ExitTarget::ThisLoop(*loop_node)),
            Some(Ctrl::Block) if is_break => Some(ExitTarget::ConsumerCall),
            Some(Ctrl::Block) => Some(ExitTarget::ThisBlock),
            Some(Ctrl::Callable(_)) | None => None,
        };
        // Record a `break` bound to a loop, so the S-5 tail classifier knows that
        // loop can exit (a `loop` with no bound `break` diverges).
        if let (true, Some(ExitTarget::ThisLoop(loop_node))) = (is_break, target) {
            self.loops_with_break.push(loop_node);
        }
        // A `while`/`loop` yields no value, so a valued `break`/`continue` targeting
        // one has no destination — a static error (L§7.10, S-10 loop half). Only a
        // block-consuming call receives a `break`/`continue` value.
        if operand.is_some() && matches!(target, Some(ExitTarget::ThisLoop(_))) {
            let kw = if is_break { "break" } else { "continue" };
            self.error(
                DiagnosticCode::ValuedExitInLoop,
                node,
                &format!(
                    "a loop yields no value, so `{kw}` here can't take one — \
                     assign the value to a variable, `return` it if the enclosing \
                     function should yield it, or use a block-consuming call for a \
                     value-yielding loop"
                ),
            );
        }
        match target {
            Some(t) => self.set_exit(node, t),
            None => {
                let kw = if is_break { "break" } else { "continue" };
                self.exit_error(
                    node,
                    &format!("`{kw}` can only appear inside a loop or a block"),
                );
            }
        }
    }

    fn set_exit(&mut self, node: NodeId, target: ExitTarget) {
        self.exit_targets[node.0 as usize] = Some(target);
    }

    fn exit_error(&mut self, node: NodeId, message: &str) {
        self.error(DiagnosticCode::MisplacedExit, node, message);
    }
}
