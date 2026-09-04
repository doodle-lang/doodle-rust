//! The transition proper: [`dispatch`] routes one popped continuation to its handler.
//! Split from `step.rs` (the [`step`](super::step) wrapper and its safe-point bookkeeping) so
//! each file stays within the hygiene length limit. Its error is a plain [`Raise`]; `step`'s
//! `?` lifts it into [`Halt`](super::super::Halt).

use super::super::cont::Cont;
use super::super::error::Raise;
use super::super::{
    LoadedModule, Machine, Value, arith, assign, block, call, compare, control, dict, dynamic,
    eval, modload, protect, protocol, record, strop, types, unwind,
};
use super::{is_arithmetic, return_from_top_frame, seq_step, take_value};
use crate::ast::{BinaryOp, UnaryOp};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;

/// Executes one popped continuation — or, when `None`, returns from a drained
/// frame. This is the transition proper; [`step`](super::step) wraps it with the safe-point
/// limit checks.
pub(super) fn dispatch(
    resolved: &ResolvedModule,
    modules: &mut [LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    cont: Option<Cont>,
) -> Result<(), Raise> {
    // The executing module is the one whose resolved AST we hold (AD5); its namespace is
    // `modules[cur].namespace`. Read arms borrow it inline (a short-lived shared reborrow);
    // the call, field-read, and import arms take `modules` so they can reach another
    // module's AST/namespace (cross-module call, `m.x`) or bind into the importer's.
    let cur = resolved.canonical_id.0 as usize;
    match cont {
        Some(Cont::Seq { block, next }) => {
            seq_step(resolved, machine, block, next);
            Ok(())
        }
        Some(Cont::Eval { node }) => eval::eval(resolved, modules, heap, machine, node),
        Some(Cont::BinRhs { op, rhs, span }) => eval::bin_rhs(machine, op, rhs, span),
        Some(Cont::BinApply { op, lhs, span }) => {
            let rhs = take_value(machine, span)?;
            let result = match op {
                // `x is T`: the right operand is a type value (L§6.5). The callable
                // trio (`Procedure`/`Function`/`Callable`) needs the resolver and the
                // intrinsic registry to read a callable's `to`/`fn` kind (S-37).
                BinaryOp::Is => types::is_op(
                    lhs,
                    rhs,
                    heap,
                    resolved,
                    modules,
                    &machine.protocols,
                    &machine.intrinsics,
                    span,
                )?,
                // String `+`/`*` branch off the numeric path (L§4.4, S-59); a pair with no
                // string operand falls through to numeric arithmetic.
                _ if is_arithmetic(op) => {
                    match strop::try_binary(op, lhs, rhs, heap, machine, span)? {
                        Some(v) => v,
                        None => arith::binary(op, lhs, rhs, heap, machine, span)?,
                    }
                }
                // A comparison or equality operator (`== != < > <= >=`).
                _ => compare::binary(op, lhs, rhs, heap, span)?,
            };
            machine.reg = Some(result);
            Ok(())
        }
        Some(Cont::UnaryApply { op, span }) => {
            let operand = take_value(machine, span)?;
            let result = match op {
                UnaryOp::Not => compare::not(operand, span)?,
                UnaryOp::Neg | UnaryOp::Pos => arith::unary(op, operand, heap, span)?,
            };
            machine.reg = Some(result);
            Ok(())
        }
        Some(Cont::AndRhs { rhs, span }) => eval::logical_rhs(machine, rhs, span, true),
        Some(Cont::OrRhs { rhs, span }) => eval::logical_rhs(machine, rhs, span, false),
        Some(Cont::AssertBool { span }) => {
            let v = take_value(machine, span)?;
            machine.reg = Some(Value::Bool(compare::as_bool(v, "and/or", span)?));
            Ok(())
        }
        Some(Cont::BindLet { decl }) => {
            control::bind_let(resolved, heap, machine, &modules[cur].namespace, decl)
        }
        Some(Cont::AssignTo { assign }) => {
            assign::assign_to(resolved, heap, machine, &modules[cur].namespace, assign)
        }
        Some(Cont::AssignPlaceObj { assign }) => {
            assign::assign_place_obj(resolved, machine, assign)
        }
        Some(Cont::AssignFieldVal { assign, object }) => {
            record::field_set(resolved, heap, machine, assign, object)
        }
        Some(Cont::AssignIndexKey { assign, object }) => {
            assign::assign_index_key(resolved, machine, assign, object)
        }
        Some(Cont::AssignIndexVal {
            assign,
            object,
            key,
        }) => dict::index_set(resolved, modules, heap, machine, assign, object, key),
        Some(Cont::IfChoose { node, index }) => control::if_choose(resolved, machine, node, index),
        Some(Cont::WhileCheck { node }) => control::while_check(resolved, machine, node),
        Some(Cont::LoopReloop { node }) => {
            control::loop_reloop(resolved, machine, node);
            Ok(())
        }
        Some(Cont::CallGotCallee { call }) => {
            call::got_callee(resolved, modules, heap, machine, call)
        }
        Some(Cont::CallGotArg {
            call,
            callee,
            values,
            index,
        }) => call::got_arg(
            resolved, modules, heap, machine, call, callee, values, index,
        ),
        Some(Cont::BlockGotArg {
            call,
            values,
            index,
        }) => block::got_block_arg(resolved, modules, heap, machine, call, values, index),
        Some(Cont::ListGotElem {
            list,
            values,
            index,
        }) => eval::list_got_elem(resolved, heap, machine, list, values, index),
        Some(Cont::StrInterp { node, acc, index }) => {
            eval::str_interp(resolved, modules, heap, machine, node, acc, index)
        }
        Some(Cont::StrInterpRendered { node, acc, index }) => {
            eval::str_interp_rendered(resolved, heap, machine, node, acc, index)
        }
        Some(Cont::DictGotKey {
            dict,
            entries,
            index,
        }) => dict::dict_got_key(resolved, machine, dict, entries, index),
        Some(Cont::DictGotValue {
            dict,
            entries,
            index,
            key,
        }) => dict::dict_got_value(resolved, modules, heap, machine, dict, entries, index, key),
        Some(Cont::DictBuildHashed {
            node,
            dict,
            entries,
            index,
        }) => dict::dict_build_hashed(resolved, modules, heap, machine, node, dict, entries, index),
        Some(Cont::IndexGotObject { index, span }) => dict::index_got_object(machine, index, span),
        Some(Cont::IndexApply {
            object,
            span,
            key_node,
        }) => dict::index_apply(modules, heap, machine, object, key_node, span),
        Some(Cont::IndexReadHashed { dict, key, span }) => {
            dict::index_read_hashed(heap, machine, dict, key, span)
        }
        Some(Cont::IndexAssignHashed {
            dict,
            key,
            value,
            span,
        }) => dict::index_assign_hashed(heap, machine, dict, key, value, span),
        Some(Cont::BindDefault { slot, default }) => {
            call::bind_default(resolved, heap, machine, slot, default)
        }
        Some(Cont::DefineCallable { decl }) => {
            call::define_callable(resolved, heap, machine, &modules[cur].namespace, decl);
            Ok(())
        }
        Some(Cont::DefineRecord { decl }) => {
            record::define(resolved, heap, machine, &modules[cur].namespace, decl);
            Ok(())
        }
        Some(Cont::DefineProtocol { decl }) => {
            protocol::define_protocol(resolved, modules, heap, machine, cur, decl)
        }
        Some(Cont::DefineImplement { decl }) => {
            protocol::define_implement(resolved, modules, heap, machine, cur, decl)
        }
        Some(Cont::FieldRead { field }) => {
            record::field_read(resolved, modules, heap, machine, field)
        }
        // A `with`'s value is now in the register: open its dynamic binding and run the
        // body under a `WithRestore` (dynamic.rs).
        Some(Cont::WithBind { with }) => dynamic::with_bind(resolved, modules, heap, machine, with),
        // A `with` body completed normally: restore its dynamic binding (machine-design
        // §13). The body's value stays in the register as the `with`'s value.
        Some(Cont::WithRestore { dyn_mark }) => {
            unwind::restore(machine, heap, dyn_mark);
            Ok(())
        }
        // A `try` body completed normally: its handler is not run, and the body's value
        // is the `try`'s value (already in the register). Discard the handler cont.
        Some(Cont::TryHandler { .. }) => Ok(()),
        // A `raise` throws its operand (or re-raises the handled exception), arming the
        // Raise unwind (protect.rs).
        Some(Cont::RaiseApply { raise }) => protect::raise_apply(resolved, heap, machine, raise),
        // A rescue body finished normally: pop the exception it was handling (L§12.2).
        Some(Cont::PopHandler) => {
            machine.pop_handling();
            Ok(())
        }
        Some(Cont::ReturnBarrier) => call::return_from_callable(resolved, heap, machine),
        Some(Cont::ExitApply { exit }) => unwind::exit_apply(resolved, heap, machine, exit),
        // Process one target of an `import` (E§6): bind a loaded module, park an import
        // suspension for an unloaded one, or raise a circular/failed import. Binds into the
        // importer's namespace, so it takes `modules` mutably.
        Some(Cont::ImportTargets { import, next }) => {
            modload::step_import_targets(resolved, modules, heap, machine, import, next)
        }
        // The frame's work is drained: return from it.
        None => {
            return_from_top_frame(machine);
            Ok(())
        }
    }
}
