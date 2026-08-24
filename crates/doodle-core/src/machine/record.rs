//! Records (L§9, L§4.14): the type value a `record` declaration binds, the
//! constructor a `Type(record)` callee runs, and field reads.
//!
//! A record type's schema (name, field order, `ref`-ness) lives on the shared type
//! value ([`TypeKind::Record`]); an instance ([`RecObj`](crate::heap::RecObj)) stores
//! only its field values, positionally, plus a reference to its type (nominal identity
//! for `is`, L§6.5). Field place assignment (`r.name = v`) mutates a record in place;
//! [`copy_on_bind`] implements the value-vs-`ref` copy behavior (L§4.14) that makes
//! the distinction observable — a value record is copied at each bind, a `ref` shared.

use super::compare::kind_name;
use super::error::{ExceptionKind, Raise};
use super::step::take_value;
use super::{Machine, RecordType, TypeKind, Value};
use crate::ast::{Arg, Node, NodeId};
use crate::heap::{Heap, TypeObj};
use crate::machine::{TypeIdx, control};
use crate::resolve::ResolvedModule;
use crate::span::Span;

/// Runs a `record …` declaration (L§9): builds the record's **type value** (its
/// schema) and binds it to the declaration's name, so `Point(…)` constructs and
/// `x is Point` tests against it.
pub(crate) fn define(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    namespace: &control::Namespace,
    decl: NodeId,
) {
    let Node::Record {
        name,
        fields,
        is_ref,
        ..
    } = resolved.ast.node(decl)
    else {
        unreachable!("record::define over a non-Record node");
    };
    let schema = RecordType {
        name: name.clone(),
        fields: fields.clone().into_boxed_slice(),
        is_ref: *is_ref,
    };
    let kind = TypeKind::Record(schema);
    let ty = Value::Type(heap.alloc_type(TypeObj { kind }));
    control::bind_decl(resolved, heap, machine, namespace, decl, ty);
}

/// Constructs a record instance for a `Type(record)` callee (L§9): matches the call's
/// arguments to the type's fields, then allocates the instance into the register.
pub(crate) fn construct(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    call: NodeId,
    type_idx: TypeIdx,
    arg_values: Vec<Value>,
) -> Result<(), Raise> {
    let span = resolved.ast.span(call);
    // Copy the field names out so the type borrow releases before the alloc.
    let (name, field_names) = match &heap.type_value(type_idx).kind {
        TypeKind::Record(rt) => (rt.name.clone(), rt.fields.to_vec()),
        TypeKind::Builtin(_) => unreachable!("construct on a built-in type"),
    };
    let fields = match_fields(resolved, call, &name, &field_names, &arg_values, span)?;
    // A field is a place: storing a value record into it copies (L§4.14), exactly as
    // a `let`/assignment/argument bind does. `ref` fields and non-records share.
    let fields: Vec<Value> = fields.into_iter().map(|f| copy_on_bind(f, heap)).collect();
    let rec = heap.alloc_record(type_idx, fields.into_boxed_slice());
    machine.reg = Some(Value::Record(rec));
    Ok(())
}

/// Copies `value` for a **binding, assignment, argument bind, or container store**
/// (L§4.14): a **value record** is copied — recursively through its value-record
/// fields, so the new binding is fully independent — and everything else (scalars,
/// strings, lists, dicts, callables, `ref` records) is *shared*, its bits returned
/// unchanged. Reads and place navigation do **not** copy; the copy fires only where
/// a value lands in a place, giving C-style struct-copy value semantics.
///
/// Recursion terminates: a value record can never contain itself (assigning it into
/// a field copies), so a value-record graph is a finite tree. No GC runs during the
/// copy — collection happens only at statement-level safe points, between machine
/// transitions — so the intermediate copies are safe before they are rooted.
pub(crate) fn copy_on_bind(value: Value, heap: &mut Heap) -> Value {
    let Value::Record(r) = value else {
        return value;
    };
    let type_idx = heap.record(r).type_idx;
    let is_ref = match &heap.type_value(type_idx).kind {
        TypeKind::Record(rt) => rt.is_ref,
        TypeKind::Builtin(_) => unreachable!("a record's type is a record type"),
    };
    if is_ref {
        return value; // a `ref` record is shared, not copied.
    }
    // Copy the fields out so the record's immutable borrow releases before the
    // recursive copies allocate (`Value` is `Copy`).
    let fields = heap.record(r).fields.to_vec();
    let copied: Vec<Value> = fields.into_iter().map(|f| copy_on_bind(f, heap)).collect();
    Value::Record(heap.alloc_record(type_idx, copied.into_boxed_slice()))
}

/// Matches a construction call's arguments to `field_names` (L§9), returning the field
/// values in declaration order. Positional args fill fields left to right; keyword
/// args fill by name. A missing field, an unknown field name, a duplicate, or too many
/// positionals raises `ArgumentError`.
fn match_fields(
    resolved: &ResolvedModule,
    call: NodeId,
    rec_name: &str,
    field_names: &[Box<str>],
    arg_values: &[Value],
    span: Span,
) -> Result<Vec<Value>, Raise> {
    let Node::Call { args, .. } = resolved.ast.node(call) else {
        unreachable!("record::match_fields over a non-Call node");
    };
    let mut slots: Vec<Option<Value>> = vec![None; field_names.len()];
    let mut pos = 0usize;
    for (arg, &val) in args.iter().zip(arg_values.iter()) {
        let f = match arg {
            Arg::Positional(_) => {
                if pos >= field_names.len() {
                    return Err(arg_err(
                        span,
                        format!("too many values for record `{rec_name}`"),
                    ));
                }
                let f = pos;
                pos += 1;
                f
            }
            Arg::Keyword { name, .. } => {
                match field_names.iter().position(|f| f.as_ref() == name.as_ref()) {
                    Some(f) => f,
                    None => {
                        return Err(arg_err(
                            span,
                            format!("record `{rec_name}` has no field `{name}`"),
                        ));
                    }
                }
            }
        };
        if slots[f].is_some() {
            return Err(arg_err(
                span,
                format!("field `{}` was given more than once", field_names[f]),
            ));
        }
        slots[f] = Some(val);
    }
    // Every field must be given (records have no field defaults in M4.2).
    let mut fields = Vec::with_capacity(field_names.len());
    for (i, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(v) => fields.push(v),
            None => {
                return Err(arg_err(
                    span,
                    format!("missing field `{}` for record `{rec_name}`", field_names[i]),
                ));
            }
        }
    }
    Ok(fields)
}

/// The record's object is in the register (from a `Field` node's evaluation): read
/// the named field (L§9), raising if the field is absent or the value is not a record.
pub(crate) fn field_read(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    field_node: NodeId,
) -> Result<(), Raise> {
    let span = resolved.ast.span(field_node);
    let object = take_value(machine, span)?;
    let Node::Field { name, .. } = resolved.ast.node(field_node) else {
        unreachable!("record::field_read over a non-Field node");
    };
    let Value::Record(r) = object else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't read a field of {}", kind_name(object)),
            span,
        ));
    };
    let type_idx = heap.record(r).type_idx;
    let pos = match &heap.type_value(type_idx).kind {
        TypeKind::Record(rt) => rt.fields.iter().position(|f| f.as_ref() == name.as_ref()),
        TypeKind::Builtin(_) => unreachable!("a record's type is a record type"),
    };
    match pos {
        Some(p) => {
            machine.reg = Some(heap.record(r).fields[p]);
            Ok(())
        }
        None => Err(Raise::new(
            ExceptionKind::NoSuchField,
            format!("this record has no field `{name}`"),
            span,
        )),
    }
}

/// Completes a field place assignment `object.name = rhs` (L§5.3): `object` (the
/// place, navigated with no copy) is passed in; the RHS is in the register. Copies
/// the RHS for binding (L§4.14) and writes it into the field, mutating the record in
/// place. Raises `TypeMismatch` if `object` is not a record, `NoSuchField` if it has
/// no such field. The statement yields Void.
pub(crate) fn field_set(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    machine: &mut Machine,
    assign: NodeId,
    object: Value,
) -> Result<(), Raise> {
    let Node::Assign { target, value } = resolved.ast.node(assign) else {
        unreachable!("record::field_set over a non-Assign node");
    };
    let (field_node, value_node) = (*target, *value);
    let rhs = copy_on_bind(take_value(machine, resolved.ast.span(value_node))?, heap);
    let Node::Field { name, .. } = resolved.ast.node(field_node) else {
        unreachable!("record::field_set over a non-Field target");
    };
    let span = resolved.ast.span(field_node);
    let Value::Record(r) = object else {
        return Err(Raise::new(
            ExceptionKind::TypeMismatch,
            format!("you can't set a field of {}", kind_name(object)),
            span,
        ));
    };
    let type_idx = heap.record(r).type_idx;
    let pos = match &heap.type_value(type_idx).kind {
        TypeKind::Record(rt) => rt.fields.iter().position(|f| f.as_ref() == name.as_ref()),
        TypeKind::Builtin(_) => unreachable!("a record's type is a record type"),
    };
    match pos {
        Some(p) => {
            heap.record_set_field(r, p, rhs);
            Ok(())
        }
        None => Err(Raise::new(
            ExceptionKind::NoSuchField,
            format!("this record has no field `{name}`"),
            span,
        )),
    }
}

fn arg_err(span: Span, message: String) -> Raise {
    Raise::new(ExceptionKind::ArgumentError, message, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::TypeObj;

    fn record_type(heap: &mut Heap, name: &str, fields: &[&str]) -> TypeIdx {
        typed_record(heap, name, fields, false)
    }

    fn typed_record(heap: &mut Heap, name: &str, fields: &[&str], is_ref: bool) -> TypeIdx {
        let schema = RecordType {
            name: name.into(),
            fields: fields
                .iter()
                .map(|f| (*f).into())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            is_ref,
        };
        heap.alloc_type(TypeObj {
            kind: TypeKind::Record(schema),
        })
    }

    /// A record field's value, for asserting on copies.
    fn field(heap: &Heap, v: Value, pos: usize) -> Value {
        match v {
            Value::Record(r) => heap.record(r).fields[pos],
            _ => panic!("not a record"),
        }
    }

    #[test]
    fn copy_on_bind_copies_a_value_record_but_shares_a_ref_record() {
        let mut heap = Heap::new();
        let vty = typed_record(&mut heap, "V", &["x"], false);
        let rty = typed_record(&mut heap, "R", &["x"], true);
        let v = Value::Record(heap.alloc_record(vty, Box::new([Value::Int(1)])));
        let r = Value::Record(heap.alloc_record(rty, Box::new([Value::Int(1)])));
        // A value record is copied: a distinct instance whose mutation is independent.
        let v2 = copy_on_bind(v, &mut heap);
        assert!(!matches!((v, v2), (Value::Record(a), Value::Record(b)) if a == b));
        let Value::Record(v2r) = v2 else {
            unreachable!()
        };
        heap.record_set_field(v2r, 0, Value::Int(99));
        assert!(
            matches!(field(&heap, v, 0), Value::Int(1)),
            "original untouched"
        );
        // A `ref` record is shared: the same instance.
        let r2 = copy_on_bind(r, &mut heap);
        assert!(matches!((r, r2), (Value::Record(a), Value::Record(b)) if a == b));
    }

    #[test]
    fn copy_on_bind_copies_value_record_fields_recursively_and_shares_others() {
        let mut heap = Heap::new();
        let inner_ty = typed_record(&mut heap, "Inner", &["x"], false);
        let outer_ty = typed_record(&mut heap, "Outer", &["inner", "xs"], false);
        let inner = Value::Record(heap.alloc_record(inner_ty, Box::new([Value::Int(1)])));
        let shared_list = Value::List(heap.alloc_list(vec![Value::Int(0)]));
        let outer = Value::Record(heap.alloc_record(outer_ty, Box::new([inner, shared_list])));
        let copy = copy_on_bind(outer, &mut heap);
        // The value-record `inner` field is a *distinct* object (deep copy)…
        assert!(
            !matches!((field(&heap, outer, 0), field(&heap, copy, 0)),
                (Value::Record(a), Value::Record(b)) if a == b),
            "nested value record is copied"
        );
        // …while the reference-typed `xs` field is *shared* (same list).
        assert!(
            matches!((field(&heap, outer, 1), field(&heap, copy, 1)),
                (Value::List(a), Value::List(b)) if a == b),
            "reference field is shared"
        );
    }

    #[test]
    fn copy_on_bind_returns_non_records_unchanged() {
        let mut heap = Heap::new();
        let list = Value::List(heap.alloc_list(vec![]));
        assert!(matches!(
            (list, copy_on_bind(list, &mut heap)),
            (Value::List(a), Value::List(b)) if a == b
        ));
        assert!(matches!(
            copy_on_bind(Value::Int(7), &mut heap),
            Value::Int(7)
        ));
    }

    #[test]
    fn gc_keeps_a_reachable_records_type_and_fields() {
        let mut heap = Heap::new();
        let ty = record_type(&mut heap, "Point", &["x", "y"]);
        let field_str = Value::Str(heap.alloc_string("hi".into()));
        let rec = heap.alloc_record(ty, Box::new([Value::Int(1), field_str]));
        let before = heap.live_objects();
        // Rooting the record keeps its type value AND its heap-allocated field alive.
        heap.collect(|tracer| tracer.value(Value::Record(rec)));
        assert_eq!(
            heap.live_objects(),
            before,
            "record, type, and field all survive"
        );
        // Without a root, everything is reclaimed.
        heap.collect(|_| {});
        assert_eq!(heap.live_objects(), 0);
    }
}
