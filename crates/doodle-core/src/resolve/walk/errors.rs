//! The resolver's static-error battery (M1.10b): duplicate declarations (L§5.2)
//! and the assignment check (L§5.3, S-6 rule 2a). Split from `walk/mod.rs` for
//! length; part of the same [`Resolver`](super::Resolver) walk.
//!
//! One uniform rule governs a bare-name assignment target: it must resolve
//! lexically to a **`let`** binding (a local `let`, or a module-level `let` of the
//! current module). Everything else is an error — a `const`/declaration target
//! (rule 2a), or a name not visible as a binding here (undeclared, or one that
//! could only come from a read-only import, S-39). Because imports are read-only
//! (S-39), *no import resolution is needed*: a wildcard-imported name is as
//! unassignable as an undeclared one, so current-module lexical knowledge is
//! sound and complete. Wildcards change only the wording of the not-visible
//! message (naming the source module is deferred to M5).

use super::Resolver;
use crate::ast::{Node, NodeId};
use crate::diag::code::DiagnosticCode;
use crate::resolve::GlobalKind;

impl Resolver<'_> {
    /// Checks a resolved assignment target (L§5.3). A bare-name target must be a
    /// `let`; a `const`/declaration target is rule-2a. A module-name target is
    /// deferred to [`check_pending_assigns`](Self::check_pending_assigns), since
    /// its global may be declared later in the module. Field/index targets mutate
    /// a pointee and are always allowed.
    pub(super) fn check_assign_target(&mut self, target: NodeId) {
        let Node::Ident(name) = self.ast.node(target) else {
            return;
        };
        let name = name.clone();
        match self.lookup(&name) {
            Some((_, _, GlobalKind::Let)) => {} // a mutable local — assignable
            Some((_, _, kind)) => self.non_assignable_error(target, &name, kind),
            // Not a local: a module name; its assignability needs the complete
            // `globals`, so defer.
            None => self.pending_assigns.push((target, name)),
        }
    }

    /// Post-pass: resolve each deferred module-name assignment target against the
    /// now-complete `globals` (L§5.3).
    pub(super) fn check_pending_assigns(&mut self) {
        let pending = std::mem::take(&mut self.pending_assigns);
        for (node, name) in pending {
            match self
                .globals
                .iter()
                .find(|g| *g.name == *name)
                .map(|g| g.kind)
            {
                Some(GlobalKind::Let) => {} // a mutable module binding — assignable
                Some(kind) => self.non_assignable_error(node, &name, kind),
                None => self.undeclared_assign_error(node, &name),
            }
        }
    }

    /// A `const`/declaration assignment target — the const-reassignment family
    /// (S-6 rule 2a: declaration bindings are non-assignable).
    fn non_assignable_error(&mut self, node: NodeId, name: &str, kind: GlobalKind) {
        let what = match kind {
            GlobalKind::Const => "a constant (`const`)",
            GlobalKind::Proc => "a procedure (`to`)",
            GlobalKind::Fn => "a function (`fn`)",
            GlobalKind::Record => "a record type",
            GlobalKind::Protocol => "a protocol",
            GlobalKind::Module => "a module",
            // A dynamic parameter is `with`-rebindable, not `=`-assignable.
            GlobalKind::Parameter => "a dynamic parameter (rebind it with `with`)",
            GlobalKind::Let => return, // `let` is assignable — not reached
        };
        self.error(
            DiagnosticCode::ConstReassignment,
            node,
            &format!("can't assign to `{name}` — it is {what}, not a mutable `let`"),
        );
    }

    /// An assignment target that is not a visible binding: undeclared, or a name
    /// that could only come from a read-only import (L§5.3). One honest message
    /// covers both (naming a wildcard source is M5 provenance polish).
    fn undeclared_assign_error(&mut self, node: NodeId, name: &str) {
        self.error(
            DiagnosticCode::UndeclaredAssignment,
            node,
            &format!(
                "no `let` named `{name}` is visible here — \
                 write `let {name} = …` to create it; an imported name can't be \
                 assigned (imports are read-only), and a dynamic parameter is set \
                 with `with`"
            ),
        );
    }

    /// A duplicate binding in one scope (L§5.2).
    pub(super) fn duplicate_error(&mut self, node: NodeId, name: &str) {
        self.error(
            DiagnosticCode::DuplicateDeclaration,
            node,
            &format!("`{name}` is already declared in this scope"),
        );
    }
}
