//! Records (L§9, L§4.14): the type value a `record` declaration binds, the
//! constructor a `Type(record)` callee runs, and field reads.
//!
//! A record type's schema (name, field order, `ref`-ness) lives on the shared type
//! value ([`TypeKind::Record`]); an instance ([`RecObj`](crate::heap::RecObj)) stores
//! only its field values, positionally, plus a reference to its type (nominal identity
//! for `is`, L§6.5). Value-vs-reference copy behavior (L§4.14) becomes observable with
//! mutation (M4.3); until then a record is effectively immutable.

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
    let Node::Record { name, fields, .. } = resolved.ast.node(decl) else {
        unreachable!("record::define over a non-Record node");
    };
    let schema = RecordType {
        name: name.clone(),
        fields: fields.clone().into_boxed_slice(),
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
    let rec = heap.alloc_record(type_idx, fields.into_boxed_slice());
    machine.reg = Some(Value::Record(rec));
    Ok(())
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

fn arg_err(span: Span, message: String) -> Raise {
    Raise::new(ExceptionKind::ArgumentError, message, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::TypeObj;

    fn record_type(heap: &mut Heap, name: &str, fields: &[&str]) -> TypeIdx {
        let schema = RecordType {
            name: name.into(),
            fields: fields
                .iter()
                .map(|f| (*f).into())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        };
        heap.alloc_type(TypeObj {
            kind: TypeKind::Record(schema),
        })
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
