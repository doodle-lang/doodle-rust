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
use crate::ast::{ImportTarget, Node, NodeId};
use crate::diag::code::DiagnosticCode;
use crate::resolve::GlobalKind;

impl Resolver<'_> {
    /// Records each selective (non-wildcard) import as `bound-name → source
    /// display`, for the assignment check's "imported from …" message. The bound
    /// name is the alias, else the last path segment.
    pub(super) fn record_selective_imports(&mut self, targets: &[ImportTarget]) {
        for t in targets {
            if t.wildcard {
                continue; // a wildcard's names aren't known until load (M5)
            }
            let Some(last) = t.path.last() else { continue };
            let name = t.alias.clone().unwrap_or_else(|| last.clone());
            let source: Box<str> = t
                .path
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<&str>>()
                .join(".")
                .into();
            self.selective_imports.push((name, source));
        }
    }

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
            Some((_, _, GlobalKind::Let, _)) => {} // a mutable local — assignable
            Some((_, _, kind, _)) => self.non_assignable_error(target, &name, kind),
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

    /// Post-pass: each `exports` name must be a module-level declaration (L§11.1); one that
    /// is not is `undeclared-export`. Builds the module's public surface (the union of the
    /// declared exported names) — left `None` when the module has no `exports` statement, so
    /// every definition stays public.
    pub(super) fn check_exports(&mut self) {
        let pending = std::mem::take(&mut self.pending_exports);
        if pending.is_empty() {
            return; // no `exports` statement: all module-level definitions are public
        }
        let mut public: Vec<Box<str>> = Vec::new();
        for (node, name) in pending {
            let declared = self.globals.iter().any(|g| *g.name == *name);
            if !declared {
                self.error(
                    DiagnosticCode::UndeclaredExport,
                    node,
                    &format!(
                        "`{name}` is exported but never declared in this module — \
                         declare it, or remove it from `exports`"
                    ),
                );
            } else if !public.iter().any(|n| **n == *name) {
                public.push(name);
            }
        }
        self.exports = Some(public);
    }

    /// Post-pass: each `with` binds a dynamic parameter (L§5.5), so its target name
    /// must resolve to a module-level `parameter`. A different global kind (a lexical
    /// binding, a callable, a type/protocol/module) or no such declaration is a static
    /// error — `with` never rebinds a `let`/`const`, and there is no auto-creation.
    pub(super) fn check_with_targets(&mut self) {
        let pending = std::mem::take(&mut self.pending_with_targets);
        for (node, name) in pending {
            match self
                .globals
                .iter()
                .find(|g| *g.name == *name)
                .map(|g| g.kind)
            {
                Some(GlobalKind::Parameter) => {}
                Some(kind) => {
                    let what = with_target_kind(kind);
                    self.error(
                        DiagnosticCode::WithTargetNotParameter,
                        node,
                        &format!(
                            "`with` rebinds a dynamic parameter for a block, but `{name}` \
                             is {what} — declare it with `parameter`, or change a `let` \
                             with `=`"
                        ),
                    );
                }
                None => self.error(
                    DiagnosticCode::WithTargetNotParameter,
                    node,
                    &format!(
                        "`with` needs a dynamic parameter named `{name}`, but none is \
                         declared — declare it with `parameter {name} = default`"
                    ),
                ),
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

    /// An assignment target that is not a visible binding (L§5.3). A *selective*
    /// import gets a specific "imported from …" message (its source is lexically
    /// known); an undeclared or wildcard-supplied name gets the dual-intent
    /// message (typo of a `let`, or a read-only import — naming a wildcard source
    /// is M5 provenance polish).
    fn undeclared_assign_error(&mut self, node: NodeId, name: &str) {
        if let Some((_, source)) = self.selective_imports.iter().find(|(n, _)| &**n == name) {
            let source = source.clone();
            self.error(
                DiagnosticCode::UndeclaredAssignment,
                node,
                &format!(
                    "`{name}` is imported from `{source}` — imported names can't be \
                     assigned (imports are read-only)"
                ),
            );
            return;
        }
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

    /// A duplicate binding in one scope (L§5.2), with a note pointing at the
    /// original declaration (`first`) when it is known (rubric (b): "the original
    /// is here"). A same-scope *local* duplicate has no recorded decl node yet, so
    /// `first` is `None` there.
    pub(super) fn duplicate_error(&mut self, node: NodeId, name: &str, first: Option<NodeId>) {
        let span = self.ast.span(node);
        let mut diag = crate::diag::Diagnostic::error(
            DiagnosticCode::DuplicateDeclaration,
            self.module,
            span,
            format!("`{name}` is already declared in this scope"),
        );
        if let Some(first) = first {
            diag = diag.with_note(crate::diag::Note {
                message: format!("the first `{name}` is declared here"),
                span: Some(self.ast.span(first)),
            });
        }
        self.diagnostics.push(diag);
    }
}

/// How a non-`parameter` global reads in a `with`-target error (L§5.5). `Parameter`
/// is the valid target and never reaches this.
fn with_target_kind(kind: GlobalKind) -> &'static str {
    match kind {
        GlobalKind::Let => "a mutable `let`",
        GlobalKind::Const => "a constant (`const`)",
        GlobalKind::Proc => "a procedure (`to`)",
        GlobalKind::Fn => "a function (`fn`)",
        GlobalKind::Record => "a record type",
        GlobalKind::Protocol => "a protocol",
        GlobalKind::Module => "a module",
        GlobalKind::Parameter => "a dynamic parameter",
    }
}
