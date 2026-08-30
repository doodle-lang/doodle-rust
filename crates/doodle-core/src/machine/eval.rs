//! Expression evaluation (machine-design §8): turning an expression node into a
//! value in the register, or scheduling the continuations that will. [`eval`] is the
//! leaf dispatcher; the rest handle the operand-plumbing continuations `step`'s
//! `dispatch` routes here (`and`/`or` short-circuit, list-literal building, and the
//! binary right-operand step). The statement-level transitions stay in `step`.

use super::cont::Cont;
use super::control;
use super::error::{ExceptionKind, Raise};
use super::protocol::{self, Dispatch};
use super::step::take_value;
use super::{LoadedModule, Machine, Value, arith, call, compare, dict, protect, record};
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

/// Folds a string interpolation's parts from `start` into `acc` — already the seam-joined
/// NFC rendering of the earlier parts (L§6.7). Literal runs are seam-appended in place;
/// the first `{expr}` reached is scheduled for evaluation (its value is rendered and
/// folded in by [`str_interp`] on the way back), suspending this pass. When the parts run
/// out the finished `String` is allocated into the register.
///
/// Joining at the seam (AD4) rather than concatenating then normalizing keeps the result
/// equal to a single NFC pass over the whole — [`seam_concat`](crate::unicode::seam_concat)
/// renormalizes only each boundary, and NFC is closed under that piecewise join — while
/// touching only the boundaries, not the (already-NFC) interiors.
fn str_interp_advance(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    mut acc: String,
    start: usize,
) -> Result<(), Raise> {
    let Node::StrLit(parts) = resolved.ast.node(node) else {
        unreachable!("str_interp over a non-StrLit node");
    };
    for (index, part) in parts.iter().enumerate().skip(start) {
        match part {
            StrPart::Text(run) => {
                acc = crate::unicode::seam_concat(&acc, &crate::unicode::nfc(run));
            }
            StrPart::Interp(expr) => {
                let expr = *expr;
                let frame = machine.frames.last_mut().expect("eval with no frame");
                frame.conts.push(Cont::StrInterp { node, acc, index });
                frame.conts.push(Cont::Eval { node: expr });
                return Ok(());
            }
        }
    }
    machine.reg = Some(Value::Str(heap.alloc_string(acc.into_boxed_str())));
    Ok(())
}

/// A string interpolation's `{expr}` value is now in the register (L§6.7): render it through
/// real `Stringable` dispatch (L§15 hook 1, D-M5-1). The `to_string` member is invoked by id
/// — a **hidden binding**, so a user's local `to_string` cannot change interpolation (S-37).
/// A type with an explicit `implement Stringable` drives its `to_string` (a real call that can
/// raise), resumed by [`str_interp_rendered`]; every other value renders through the native
/// seam (scalars final, compound placeholder) and seam-appends here.
pub(super) fn str_interp(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    acc: String,
    index: usize,
) -> Result<(), Raise> {
    let Node::StrLit(parts) = resolved.ast.node(node) else {
        unreachable!("str_interp over a non-StrLit node");
    };
    let StrPart::Interp(expr) = &parts[index] else {
        unreachable!("str_interp resumed on a non-interpolation part");
    };
    let expr = *expr;
    // A procedure call `{p()}` produces no value; using it here is a Void-in-expression
    // raise attributed to the interpolated expression (L§6.11).
    let span = resolved.ast.span(expr);
    let value = take_value(machine, span)?;
    // Drive an explicit `implement Stringable` for the value's type; otherwise fall through
    // to the native renderer. Restricting to `Stringable` keeps an unrelated user protocol
    // that happens to declare a `to_string` member out of interpolation.
    if let (Some(member), Some(filter)) = (
        machine.protocols.to_string_member(),
        machine.protocols.stringable_id(),
    ) {
        let dt = protocol::dispatch_type_of(value, heap, modules, &machine.intrinsics);
        if let Dispatch::Call(cal) = machine.protocols.resolve(member, dt, Some(filter), heap) {
            let frame = machine.frames.last_mut().expect("eval with no frame");
            frame
                .conts
                .push(Cont::StrInterpRendered { node, acc, index });
            return protocol::enter_unary(modules, heap, machine, cal, value, node, span);
        }
    }
    let acc = crate::unicode::seam_concat(&acc, &super::stringify::render(heap, value));
    str_interp_advance(resolved, heap, machine, node, acc, index + 1)
}

/// A driven `Stringable.to_string` for an interpolated `{expr}` has returned its value into
/// the register (L§15): a `to_string` yields text, so the value must be a `String` — else a
/// clear type error. Seam-append it and continue folding the string's remaining parts.
pub(super) fn str_interp_rendered(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    node: NodeId,
    acc: String,
    index: usize,
) -> Result<(), Raise> {
    let span = resolved.ast.span(node);
    let value = take_value(machine, span)?;
    let Value::Str(idx) = value else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            "a `to_string` implementation must return a String".to_string(),
            span,
        )
        .with_details(super::exception::type_mismatch_details(
            "to_string",
            &["String"],
            value,
            heap,
        )));
    };
    let rendered = heap.string(idx).utf8.to_string();
    let acc = crate::unicode::seam_concat(&acc, &rendered);
    str_interp_advance(resolved, heap, machine, node, acc, index + 1)
}

/// Evaluates one expression, either producing a value into the register (a leaf)
/// or scheduling continuations that will (a compound operator). Returns `Err` if
/// reading a name raised.
pub(super) fn eval(
    resolved: &ResolvedModule,
    modules: &[LoadedModule],
    heap: &mut Heap,
    machine: &mut Machine,
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
        // heap string is NFC (L§4.4), so normalize before allocating.
        //
        // An interpolated literal (`"…{expr}…"`, L§6.7) folds its literal runs and its
        // rendered `{expr}` values together; a `{expr}` is an ordinary expression that
        // must be evaluated (and can raise or suspend), so the work is driven through
        // continuations rather than assembled here.
        Node::StrLit(parts) => {
            if parts.iter().any(|p| matches!(p, StrPart::Interp(_))) {
                return str_interp_advance(resolved, heap, machine, node, String::new(), 0);
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
        Node::Ident(_) => control::read_ref(resolved, modules, heap, machine, node)?,
        // `if` in expression position: same machinery as the statement form; the
        // selected branch's value stays in the register for the consumer (L§6.8).
        Node::If { .. } => {
            let frame = machine.frames.last_mut().expect("eval with no frame");
            control::schedule_if(frame, resolved, node);
            return Ok(());
        }
        // `try` in expression position (L§6.9): its value is the protected body's value,
        // or the rescue body's if it caught. Same machinery as the statement form.
        Node::Try { .. } => {
            let frame = machine.frames.last_mut().expect("eval with no frame");
            protect::schedule_try(frame, resolved, node);
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
        Node::Dict(_) => {
            return dict::dict_advance(resolved, modules, heap, machine, node, Vec::new(), 0);
        }
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
