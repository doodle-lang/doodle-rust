//! Binding an intrinsic call's arguments to its parameters (L§8.3): the shared
//! argument-shaping helpers [`apply`](super::apply) uses. Split from `mod.rs` for length;
//! they reuse the source-callable [`bind_arguments`] path so an intrinsic call binds
//! positional/keyword/default/duplicate arguments exactly like a Doodle call.

use super::ForeignParam;
use crate::ast::NodeId;
use crate::heap::Heap;
use crate::machine::Value;
use crate::machine::call::bind_arguments;
use crate::machine::error::{ExceptionKind, Raise};
use crate::resolve::{ParamInfo, ResolvedModule};
use crate::span::Span;

/// The [`ParamInfo`] view of an intrinsic's parameters (slot = index), for the shared
/// argument- and block-binding helpers.
pub(super) fn param_infos(params: &[ForeignParam]) -> Vec<ParamInfo> {
    params
        .iter()
        .enumerate()
        .map(|(i, p)| ParamInfo {
            name: p.name.clone(),
            slot: i as u16,
            is_block: p.is_block,
            has_default: p.default.is_some(),
        })
        .collect()
}

/// Binds call-site arguments to an intrinsic's ordinary parameters (L§8.3), returning the
/// values in parameter order. Reuses [`bind_arguments`] for the positional/keyword/too-
/// many/unknown-keyword/duplicate logic (parity with Doodle calls), then fills each
/// unbound parameter from its inline default or raises a missing-argument error. A block
/// parameter is not a value here (invoked reentrantly, M2b.5).
pub(super) fn bind_foreign_arguments(
    resolved: &ResolvedModule,
    heap: &mut Heap,
    call: NodeId,
    params: &[ForeignParam],
    arg_values: &[Value],
    span: Span,
) -> Result<Vec<Value>, Raise> {
    // ParamInfo drives `bind_arguments`; slot = parameter index, so `slots` comes back
    // in parameter order.
    let param_infos = param_infos(params);
    let (slots, filled) = bind_arguments(
        resolved,
        heap,
        call,
        &param_infos,
        params.len() as u16,
        arg_values,
        None, // the foreign function's name isn't threaded to the binding site
        span,
    )?;
    let mut args = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        // The trailing block parameter is bound separately (invoked reentrantly, MD §14),
        // never as an ordinary value here.
        if p.is_block {
            continue;
        }
        let value = match slots[i] {
            Some(v) => v,
            None => match (filled[i], p.default) {
                (false, Some(d)) => d,
                _ => {
                    return Err(Raise::new(
                        ExceptionKind::MissingArgument,
                        format!("missing argument `{}` for this call", p.name),
                        span,
                    )
                    .with_details(
                        crate::machine::exception::parameter_details(None, p.name.as_ref()),
                    ));
                }
            },
        };
        args.push(value);
    }
    Ok(args)
}
