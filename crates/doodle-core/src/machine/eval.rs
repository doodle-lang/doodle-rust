//! Expression evaluation (machine-design §8): turning an expression node into a
//! value in the register, or scheduling the continuations that will. [`eval`] is the
//! leaf dispatcher; the rest handle the operand-plumbing continuations `step`'s
//! `dispatch` routes here (`and`/`or` short-circuit, list-literal building, and the
//! binary right-operand step). The statement-level transitions stay in `step`.

use super::cont::Cont;
use super::control::{self, Namespace};
use super::error::Raise;
use super::step::take_value;
use super::{Machine, Value, arith, call, compare, dict, record};
use crate::ast::{BinaryOp, Node, NodeId, StrPart};
use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use num_bigint::BigInt;

/// The `and`/`or` short-circuit transition, with the left operand in the register.
/// `and` (is_and) short-circuits to `false` when the left is `false`; `or`
/// short-circuits to `true` when the left is `true`. Otherwise the right operand
/// is evaluated and must itself be a `Bool` (L§6.6), enforced by `AssertBool`.
pub(super) fn logical_rhs(
    machine: &mut Machine,
    rhs: NodeId,
    span: crate::span::Span,
    is_and: bool,
) -> Result<(), Raise> {
    let op = if is_and { "and" } else { "or" };
    let lhs = compare::as_bool(take_value(machine, span)?, op, span)?;
    if lhs == is_and {
        // `and` with a true left, or `or` with a false left: the result is rhs.
        let frame = machine
            .frames
            .last_mut()
            .expect("logical_rhs with no frame");
        frame.conts.push(Cont::AssertBool { span });
        frame.conts.push(Cont::Eval { node: rhs });
    } else {
        // Short-circuit: `and` false → false; `or` true → true.
        machine.reg = Some(Value::Bool(!is_and));
    }
    Ok(())
}

/// A list literal's element at `index` is now in the register: stash it, then evaluate
/// the next element or allocate the list once the last is in (L§4.6).
pub(super) fn list_got_elem(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    list: NodeId,
    mut values: Vec<Value>,
    index: u32,
) -> Result<(), Raise> {
    // A list element is a place: a value-record element is copied on store (L§4.14),
    // exactly as `dict::insert` and record construction copy theirs.
    let elem = record::copy_on_bind(take_value(machine, resolved.ast.span(list))?, heap);
    values.push(elem);
    let Node::List(elements) = resolved.ast.node(list) else {
        unreachable!("ListGotElem over a non-List node");
    };
    match elements.get(index as usize + 1) {
        Some(&next) => {
            let frame = machine.frames.last_mut().expect("a frame is active");
            frame.conts.push(Cont::ListGotElem {
                list,
                values,
                index: index + 1,
            });
            frame.conts.push(Cont::Eval { node: next });
            Ok(())
        }
        None => {
            machine.reg = Some(Value::List(heap.alloc_list(values)));
            Ok(())
        }
    }
}

/// Evaluates one expression, either producing a value into the register (a leaf)
/// or scheduling continuations that will (a compound operator). Returns `Err` if
/// reading a name raised.
pub(super) fn eval(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &Namespace,
    node: NodeId,
) -> Result<(), Raise> {
    let value = match resolved.ast.node(node) {
        Node::IntLit(n) => Value::Int(*n),
        Node::FloatLit(x) => Value::Float(*x),
        Node::BoolLit(b) => Value::Bool(*b),
        Node::NilLit => Value::Nil,
        Node::BytesLit(bytes) => Value::Bytes(heap.alloc_bytes(bytes.as_slice().into())),
        // A **non-interpolated** string literal allocates its NFC string value. The
        // decoded text can be non-NFC (e.g. a `\u{301}` combining escape), and every
        // heap string is NFC (L§4.4), so normalize before allocating. Interpolation
        // (`{expr}`) needs the L§15 Stringable dispatcher — M4.
        Node::StrLit(parts) => {
            if parts.iter().any(|p| matches!(p, StrPart::Interp(_))) {
                unimplemented!("string interpolation needs the Stringable dispatcher (M4)");
            }
            let mut text = String::new();
            for part in parts {
                if let StrPart::Text(run) = part {
                    text.push_str(run);
                }
            }
            let nfc = crate::unicode::nfc(&text).into_owned();
            Value::Str(heap.alloc_string(nfc.into_boxed_str()))
        }
        Node::BigIntLit { radix, digits } => {
            let n = BigInt::parse_bytes(digits.as_bytes(), u32::from(*radix))
                .expect("lexer-validated bignum digits");
            arith::int_value(n, heap)
        }
        // A list literal `[a, b, …]` (L§4.6): evaluate its elements left to right, then
        // allocate the list; an empty `[]` allocates immediately.
        Node::List(elements) => match elements.first() {
            None => Value::List(heap.alloc_list(Vec::new())),
            Some(&first) => {
                let frame = machine.frames.last_mut().expect("eval with no frame");
                frame.conts.push(Cont::ListGotElem {
                    list: node,
                    values: Vec::new(),
                    index: 0,
                });
                frame.conts.push(Cont::Eval { node: first });
                return Ok(());
            }
        },
        Node::Ident(_) => control::read_ref(resolved, heap, machine, namespace, node)?,
        // `if` in expression position: same machinery as the statement form; the
        // selected branch's value stays in the register for the consumer (L§6.8).
        Node::If { .. } => {
            let frame = machine.frames.last_mut().expect("eval with no frame");
            control::schedule_if(frame, resolved, node);
            return Ok(());
        }
        Node::Binary { op, lhs, rhs } => {
            let op = *op;
            let (lhs, rhs) = (*lhs, *rhs);
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            match op {
                // `and`/`or` short-circuit: after lhs, decide whether to run rhs.
                BinaryOp::And => frame.conts.push(Cont::AndRhs { rhs, span }),
                BinaryOp::Or => frame.conts.push(Cont::OrRhs { rhs, span }),
                // Arithmetic, comparison/equality, and `is` strict-evaluate both
                // operands, then apply at `BinApply`.
                _ => frame.conts.push(Cont::BinRhs { op, rhs, span }),
            }
            // Evaluate lhs first (top of the LIFO stack); the pushed cont resumes.
            frame.conts.push(Cont::Eval { node: lhs });
            return Ok(());
        }
        Node::Unary { op, operand } => {
            let op = *op;
            let operand = *operand;
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            frame.conts.push(Cont::UnaryApply { op, span });
            frame.conts.push(Cont::Eval { node: operand });
            return Ok(());
        }
        // A dict literal `{k: v, …}` (L§4.8): evaluate entries left to right (bare
        // keys are literal strings), then build the dict applying first-key-wins.
        // `dict_advance` handles the empty `{}` (it allocates immediately).
        Node::Dict(_) => return dict::dict_advance(resolved, heap, machine, node, Vec::new(), 0),
        // A field read `object.name` (L§9): evaluate the object, then read the field.
        // (Assignment `object.name = v` is the place-chain path, M4.3.)
        Node::Field { object, .. } => {
            let object = *object;
            let frame = machine.frames.last_mut().expect("eval with no frame");
            frame.conts.push(Cont::FieldRead { field: node });
            frame.conts.push(Cont::Eval { node: object });
            return Ok(());
        }
        // An index read `object[key]` (L§4.8): evaluate the object, then the key, then
        // look it up. (Assignment `object[key] = v` is the place-chain path, M4.3.)
        Node::Index { object, index } => {
            let (object, index) = (*object, *index);
            let span = resolved.ast.span(node);
            let frame = machine.frames.last_mut().expect("eval with no frame");
            frame.conts.push(Cont::IndexGotObject { index, span });
            frame.conts.push(Cont::Eval { node: object });
            return Ok(());
        }
        // A call schedules callee/argument evaluation, then `Apply` (call.rs); a
        // block-parameter invocation takes the block path (block.rs).
        Node::Call { .. } => return call::eval_call(resolved, heap, machine, node),
        // An anonymous `fn` expression interns its own closure value (L§6.10),
        // reading its captured cells from the creating environment (M2a.8).
        Node::Callable { .. } => call::make_callable(resolved, heap, machine, node),
        other => unimplemented!("expression not yet in the machine (M2a.5+): {other:?}"),
    };
    machine.reg = Some(value);
    Ok(())
}

/// A binary operator's left operand is now in the register: stash it (into a
/// `BinApply`) and evaluate the right operand (machine-design §8, expression plumbing).
pub(super) fn bin_rhs(
    machine: &mut Machine,
    op: BinaryOp,
    rhs: NodeId,
    span: crate::span::Span,
) -> Result<(), Raise> {
    let lhs = take_value(machine, span)?;
    let frame = machine.frames.last_mut().expect("bin_rhs with no frame");
    frame.conts.push(Cont::BinApply { op, lhs, span });
    frame.conts.push(Cont::Eval { node: rhs });
    Ok(())
}
