//! Statement scheduling (machine-design §8): [`dispatch_stmt`] turns one statement node
//! into the continuations that execute it, pushed onto the running frame. Split from
//! `step.rs` (which holds the transition and the continuation-`dispatch` loop) so that
//! file stays within the hygiene length limit; `step`'s `seq_step` calls this per
//! statement.

use super::cont::Cont;
use super::frame::Frame;
use super::{control, protect};
use crate::ast::{Node, NodeId};
use crate::resolve::ResolvedModule;

/// Schedules the work for one statement. A statement's value is discarded at the
/// boundary (only `fn` bodies yield, L§6.11): an expression statement evaluates
/// its expression, whose value the next `Seq` step overwrites.
pub(super) fn dispatch_stmt(resolved: &ResolvedModule, frame: &mut Frame, stmt: NodeId) {
    match resolved.ast.node(stmt) {
        Node::ExprStmt(expr) => frame.conts.push(Cont::Eval { node: *expr }),
        // Evaluate the initializer, then bind it (LIFO: the `Eval` runs first). A
        // `parameter`'s default seeds its module cell through the same path (§5.5).
        Node::Let { value, .. } | Node::Const { value, .. } => {
            frame.conts.push(Cont::BindLet { decl: stmt });
            frame.conts.push(Cont::Eval { node: *value });
        }
        Node::Parameter { default, .. } => {
            frame.conts.push(Cont::BindLet { decl: stmt });
            frame.conts.push(Cont::Eval { node: *default });
        }
        // A `with`: evaluate the bound value, then open the dynamic binding for the body
        // (WithBind, dynamic.rs), which runs the body under a restoring cleanup cont.
        Node::With { value, .. } => {
            frame.conts.push(Cont::WithBind { with: stmt });
            frame.conts.push(Cont::Eval { node: *value });
        }
        // A name target evaluates only the RHS, then binds. A `Field`/`Index` place
        // target evaluates its object first (left-to-right, L§14): navigate the object
        // as a place (no copy), then finish the store (control.rs / record.rs / dict.rs).
        Node::Assign { target, value } => match resolved.ast.node(*target) {
            Node::Ident(_) => {
                frame.conts.push(Cont::AssignTo { assign: stmt });
                frame.conts.push(Cont::Eval { node: *value });
            }
            _ => {
                let (Node::Field { object, .. } | Node::Index { object, .. }) =
                    resolved.ast.node(*target)
                else {
                    unreachable!("an assignment target is Ident, Field, or Index (L§5.3)");
                };
                frame.conts.push(Cont::AssignPlaceObj { assign: stmt });
                frame.conts.push(Cont::Eval { node: *object });
            }
        },
        Node::If { .. } => control::schedule_if(frame, resolved, stmt),
        Node::While { cond, .. } => {
            frame.conts.push(Cont::WhileCheck { node: stmt });
            frame.conts.push(Cont::Eval { node: *cond });
        }
        Node::Loop { body } => {
            frame.conts.push(Cont::LoopReloop { node: stmt });
            frame.conts.push(Cont::Seq {
                block: *body,
                next: 0,
            });
        }
        // A named `to`/`fn` declaration: intern and bind its callable value when
        // the statement runs (call.rs). Anonymous `fn` never reaches here — it is
        // an expression, wrapped in an `ExprStmt`.
        Node::Callable { .. } => frame.conts.push(Cont::DefineCallable { decl: stmt }),
        // A `record …` declaration binds its type value when the statement runs (L§9);
        // the body is docstring-only, so nothing is evaluated first (record.rs).
        Node::Record { .. } => frame.conts.push(Cont::DefineRecord { decl: stmt }),
        // A `protocol`/`implement` declaration registers itself when the statement runs
        // (L§10, protocol.rs): the protocol binds its value and member dispatcher cells;
        // the implement records its `(protocol, type, member) → callable` associations.
        Node::Protocol { .. } => frame.conts.push(Cont::DefineProtocol { decl: stmt }),
        Node::Implement { .. } => frame.conts.push(Cont::DefineImplement { decl: stmt }),
        // A non-local exit (§7.10): evaluate its operand (if any), then arm the
        // unwind toward the resolver-annotated target (unwind.rs).
        Node::Return(op) | Node::Break(op) | Node::Continue(op) => {
            frame.conts.push(Cont::ExitApply { exit: stmt });
            if let Some(operand) = op {
                frame.conts.push(Cont::Eval { node: *operand });
            }
        }
        // A `try`: run the protected body under a `TryHandler` (protect.rs).
        Node::Try { .. } => protect::schedule_try(frame, resolved, stmt),
        // A `raise` (L§12.1): evaluate its operand (if any), then throw. A bare `raise`
        // re-raises the exception being handled (RaiseApply, protect.rs).
        Node::Raise(op) => {
            frame.conts.push(Cont::RaiseApply { raise: stmt });
            if let Some(operand) = op {
                frame.conts.push(Cont::Eval { node: *operand });
            }
        }
        // An `import` (L§11, E§6): process its targets one at a time, loading each
        // not-yet-loaded module by suspending for its source (modload.rs).
        Node::Import(_) => frame.conts.push(Cont::ImportTargets {
            import: stmt,
            next: 0,
        }),
        other => unimplemented!("statement not yet in the machine (M5+): {other:?}"),
    }
}
