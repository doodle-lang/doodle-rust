//! Static conformance checks for `protocol`/`implement` (L§10, S-31/S-61): the resolver
//! post-pass that runs once the module's declarations are collected. It reports, before the
//! program runs, the structural errors the runtime would otherwise only meet late — a
//! dispatch parameter with a default, an implementation that doesn't match its member's
//! shape, a restated default, a member named that the protocol doesn't have, and an
//! implementation missing a required member (its own or an inherited one).
//!
//! **Scope.** Same-module protocols and their `extends` chain — the realistic surface. A
//! protocol reached across a module boundary (an imported `extends` parent, or an
//! `implement` of an imported protocol) is not visible to the resolver; those checks are
//! left to load, where the spec routes a cross-module structural failure to
//! `module-load-error`. Where an ancestor is cross-module the completeness checks
//! (missing-member, not-a-member) are skipped for that block, since the full member set is
//! unknown; the local checks (dispatch-parameter default, restated defaults, and any member
//! that *is* visible) still apply.

use crate::ast::{Node, NodeId, Param};
use crate::diag::DiagnosticCode;

/// A protocol member's static signature, resolved along the `extends` chain (nearest wins).
struct EffMember {
    /// The member name.
    name: Box<str>,
    /// The count of ordinary (non-block) parameters — part of the conformance shape (S-31).
    arity: usize,
    /// Whether the member takes a `do … end` block parameter — the other half of the shape.
    has_block: bool,
    /// Whether the member is **required** (its nearest declaration in the chain has no
    /// default body): an implementation must provide it.
    required: bool,
    /// The name of the protocol in the chain that declares this member (for the message
    /// "`Iterable` requires `each`").
    requiring: Box<str>,
}

/// The conformance shape of a parameter list (L§10, S-31): the count of ordinary parameters
/// and whether a block parameter is present. Names are the declaration's own, so they are
/// not part of the shape.
fn param_shape(params: &[Param]) -> (usize, bool) {
    let arity = params
        .iter()
        .filter(|p| matches!(p, Param::Ordinary { .. }))
        .count();
    let has_block = params.iter().any(|p| matches!(p, Param::Block { .. }));
    (arity, has_block)
}

impl super::Resolver<'_> {
    /// The protocol/implement conformance post-pass (L§10.1/§10.2, S-31/S-61).
    pub(super) fn check_protocols(&mut self, root: NodeId) {
        let Node::Module { stmts, .. } = self.ast.node(root) else {
            return;
        };
        let stmts = stmts.clone();
        // Same-module protocols by name — the chain and every `implement` resolve through it.
        let protos: Vec<(Box<str>, NodeId)> = stmts
            .iter()
            .filter_map(|&s| match self.ast.node(s) {
                Node::Protocol { name, .. } => Some((name.clone(), s)),
                _ => None,
            })
            .collect();
        for &s in &stmts {
            if matches!(self.ast.node(s), Node::Protocol { .. }) {
                self.check_protocol(s, &protos);
            } else if matches!(self.ast.node(s), Node::Implement { .. }) {
                self.check_implement(s, &protos);
            }
        }
    }

    /// Per-protocol checks (L§10.1, S-31/S-61): a dispatch (first) parameter may not have a
    /// default, and a member re-declared from an ancestor must keep the ancestor's shape.
    fn check_protocol(&mut self, pnode: NodeId, protos: &[(Box<str>, NodeId)]) {
        let Node::Protocol { members, .. } = self.ast.node(pnode) else {
            return;
        };
        let members = members.clone();
        // The ancestor members visible from this protocol (excluding its own declarations),
        // for the re-declaration conformance check.
        let (chain, _) = self.same_module_chain(pnode, protos);
        let ancestors: Vec<(Box<str>, usize, bool)> = chain[1..]
            .iter()
            .flat_map(|&c| self.protocol_member_shapes(c))
            .collect();
        for m in &members {
            // A member's first (dispatch) parameter may not have a default (S-31).
            if let Some(Param::Ordinary {
                default: Some(d), ..
            }) = m.params.first()
            {
                self.error(
                    DiagnosticCode::DispatchParameterDefault,
                    *d,
                    &format!(
                        "the first parameter of `{}` can't have a default — a protocol \
                         dispatches on the value passed for it, so it must always be given",
                        m.name
                    ),
                );
            }
            // A re-declaration must keep the ancestor's shape (S-61).
            let (arity, has_block) = param_shape(&m.params);
            if let Some((_, a_arity, a_block)) = ancestors
                .iter()
                .find(|(n, _, _)| n.as_ref() == m.name.as_ref())
                && (*a_arity != arity || *a_block != has_block)
            {
                let span = m.body.unwrap_or(pnode);
                self.error(
                    DiagnosticCode::ProtocolSignatureMismatch,
                    span,
                    &format!(
                        "`{}` here doesn't match the `{}` it re-declares from a parent \
                         protocol — a re-declared member must keep the same shape",
                        m.name, m.name
                    ),
                );
            }
        }
    }

    /// Per-`implement` checks (L§10.2, S-31/S-61): each method matches its member's shape and
    /// writes no defaults; every required member of the protocol's chain is provided; a
    /// method names a real member.
    fn check_implement(&mut self, inode: NodeId, protos: &[(Box<str>, NodeId)]) {
        let Node::Implement {
            protocol, methods, ..
        } = self.ast.node(inode)
        else {
            return;
        };
        let (protocol, methods) = (protocol.clone(), methods.clone());
        // A cross-module protocol is invisible to the resolver — its checks fall to load.
        let Some(&(_, pnode)) = protos.iter().find(|(n, _)| n.as_ref() == protocol.as_ref()) else {
            return;
        };
        let (chain, cross_ancestor) = self.same_module_chain(pnode, protos);
        let members = self.effective_members(&chain);
        let mut provided: Vec<Box<str>> = Vec::new();
        for &method in &methods {
            let Node::Callable {
                name: Some(mname),
                params,
                ..
            } = self.ast.node(method)
            else {
                continue;
            };
            let (mname, params) = (mname.clone(), params.clone());
            provided.push(mname.clone());
            // An implementation may not restate a member's defaults (S-31).
            for p in &params {
                if let Param::Ordinary {
                    name,
                    default: Some(d),
                } = p
                {
                    self.error(
                        DiagnosticCode::ImplementationParameterDefault,
                        *d,
                        &format!(
                            "an implementation can't give `{name}` a default — defaults \
                             belong on the protocol's member, not here"
                        ),
                    );
                }
            }
            match members.iter().find(|e| e.name.as_ref() == mname.as_ref()) {
                Some(e) => {
                    let (arity, has_block) = param_shape(&params);
                    if arity != e.arity || has_block != e.has_block {
                        self.error(
                            DiagnosticCode::ProtocolSignatureMismatch,
                            method,
                            &shape_message(&mname, &protocol, arity, has_block, e),
                        );
                    }
                }
                // A method that matches no member — unless an unseen cross-module ancestor
                // might declare it — is a typo or a stray member (L§10.2).
                None if !cross_ancestor => self.error(
                    DiagnosticCode::NotAProtocolMember,
                    method,
                    &format!("`{mname}` isn't a member of `{protocol}`"),
                ),
                None => {}
            }
        }
        // Every required member the chain declares must be provided (S-61). Skipped when an
        // ancestor is cross-module, since the required set is then unknown.
        if !cross_ancestor {
            let missing: Vec<&EffMember> = members
                .iter()
                .filter(|e| e.required && !provided.iter().any(|p| p.as_ref() == e.name.as_ref()))
                .collect();
            if !missing.is_empty() {
                self.error(
                    DiagnosticCode::IncompleteImplementation,
                    inode,
                    &missing_message(&protocol, &missing),
                );
            }
        }
    }

    /// The `extends` chain from `pnode` to its root through **same-module** protocols
    /// (`[pnode, parent, …]`); the flag is set when the chain is cut short by a cross-module
    /// (imported) parent — the caller then skips the completeness checks (S-61).
    fn same_module_chain(
        &self,
        pnode: NodeId,
        protos: &[(Box<str>, NodeId)],
    ) -> (Vec<NodeId>, bool) {
        let mut chain = vec![pnode];
        let mut cur = pnode;
        while let Node::Protocol { extends, .. } = self.ast.node(cur) {
            let Some(parent_name) = extends else { break };
            match protos
                .iter()
                .find(|(n, _)| n.as_ref() == parent_name.as_ref())
            {
                // A repeat would be a cycle, which is unwritable (parent-first load order,
                // S-61) — the guard is a defensive backstop, not a reachable state.
                Some(&(_, parent)) if !chain.contains(&parent) => {
                    chain.push(parent);
                    cur = parent;
                }
                Some(_) => break,
                None => return (chain, true),
            }
        }
        (chain, false)
    }

    /// The `(name, arity, has_block)` of every member a single protocol declares.
    fn protocol_member_shapes(&self, pnode: NodeId) -> Vec<(Box<str>, usize, bool)> {
        let Node::Protocol { members, .. } = self.ast.node(pnode) else {
            return Vec::new();
        };
        members
            .iter()
            .map(|m| {
                let (arity, has_block) = param_shape(&m.params);
                (m.name.clone(), arity, has_block)
            })
            .collect()
    }

    /// The effective member set of a chain (nearest wins, S-61): walking leaf → root, the
    /// first declaration of each name fixes its shape and whether it is required.
    fn effective_members(&self, chain: &[NodeId]) -> Vec<EffMember> {
        let mut eff: Vec<EffMember> = Vec::new();
        for &c in chain {
            let Node::Protocol {
                name: pn, members, ..
            } = self.ast.node(c)
            else {
                continue;
            };
            for m in members {
                if eff.iter().any(|e| e.name.as_ref() == m.name.as_ref()) {
                    continue; // a nearer protocol already fixed this member
                }
                let (arity, has_block) = param_shape(&m.params);
                eff.push(EffMember {
                    name: m.name.clone(),
                    arity,
                    has_block,
                    required: m.body.is_none(),
                    requiring: pn.clone(),
                });
            }
        }
        eff
    }
}

/// The message for an implementation method whose shape doesn't match its member (S-31).
fn shape_message(
    method: &str,
    protocol: &str,
    arity: usize,
    has_block: bool,
    member: &EffMember,
) -> String {
    if arity != member.arity {
        format!(
            "`{method}` takes {arity} input(s), but `{protocol}`'s `{method}` takes \
             {} — an implementation must match the protocol's shape",
            member.arity
        )
    } else if has_block && !member.has_block {
        format!("`{method}` takes a `do … end` block, but `{protocol}`'s `{method}` doesn't")
    } else {
        format!("`{method}` needs a `do … end` block to match `{protocol}`'s `{method}`")
    }
}

/// The message for an `implement` block missing required members (S-61): names each and the
/// protocol requiring it.
fn missing_message(protocol: &str, missing: &[&EffMember]) -> String {
    let list: Vec<String> = missing
        .iter()
        .map(|e| format!("`{}` (required by `{}`)", e.name, e.requiring))
        .collect();
    format!(
        "this `implement … for` block for `{protocol}` is missing {} — add {} it needs",
        list.join(", "),
        if missing.len() == 1 {
            "the member"
        } else {
            "the members"
        }
    )
}
