//! Declaration and binding for the resolver walk: how names enter scopes and
//! frames — parameters, `let`/`const`/declaration bindings, the frame-slot vs
//! scope-binding split (L§8.2), the duplicate-declaration check, and the L§5.1
//! **shadowing** warning. Split from `walk/mod.rs` for length; part of the same
//! [`Resolver`](super::Resolver) walk.

use super::Binding;
use crate::ast::{Node, NodeId, Param};
use crate::diag::code::DiagnosticCode;
use crate::resolve::{GlobalDecl, GlobalKind, ParamInfo, Resolution};

impl super::Resolver<'_> {
    /// Allocates a slot (frame side only) per parameter, in order, returning the
    /// param table. Names are bound in scope later by [`scope_params`], after
    /// defaults resolve (L§8.2), so the param slots are `0..n` while a default can
    /// neither see a sibling param nor resolve an enclosing local against this
    /// frame directly (it captures instead).
    pub(super) fn alloc_param_slots(&mut self, params: &[Param]) -> Vec<ParamInfo> {
        let mut infos = Vec::new();
        for p in params {
            let (name, is_block, has_default) = match p {
                Param::Ordinary { name, default } => (name, false, default.is_some()),
                Param::Block { name } => (name, true, false),
            };
            let slot = self.alloc_frame_slot(name);
            infos.push(ParamInfo {
                name: name.clone(),
                slot,
                is_block,
                has_default,
            });
        }
        infos
    }

    /// Binds each parameter's name → slot in the current scope (after defaults).
    /// A param hiding an outer binding warns (L§5.1); its caret is the callable
    /// `decl` node — a `Param` carries no span of its own. (Sibling params are the
    /// same scope, so `check_shadowing`, which inspects only *enclosing* scopes,
    /// never treats one param as shadowing another.)
    pub(super) fn scope_params(&mut self, decl: NodeId, param_infos: &[ParamInfo]) {
        for pi in param_infos {
            self.check_shadowing(decl, &pi.name);
            self.bind_in_scope(&pi.name, pi.slot, GlobalKind::Let);
        }
    }

    /// Warns (L§5.1) when this nested declaration of `name` **hides** an outer
    /// binding — one in an *enclosing* scope, or a module global (whole-scope, so
    /// including one declared later). A collision in the *same* scope is a
    /// duplicate, reported separately.
    pub(super) fn check_shadowing(&mut self, decl: NodeId, name: &str) {
        let enclosing = self.scopes.len().saturating_sub(1);
        let hides = self.scopes[..enclosing]
            .iter()
            .any(|s| s.bindings.iter().any(|b| &*b.name == name))
            || self.module_global_names.iter().any(|n| &**n == name);
        if hides {
            self.warn(
                DiagnosticCode::Shadowing,
                decl,
                &format!(
                    "this `{name}` hides an outer `{name}` declared earlier — that's \
                     allowed, but check you meant to"
                ),
            );
        }
    }

    /// The names of every *direct* module-level declaration (the module globals),
    /// for the whole-scope shadowing check. Mirrors the module-direct arms of
    /// [`declare_binding`](Self::declare_binding) (imports/exports bind no global
    /// here — imported-name shadowing is M5); a `debug_assert` in `resolve_module`
    /// keeps the two in sync.
    pub(super) fn collect_global_names(&self, stmts: &[NodeId]) -> Vec<Box<str>> {
        stmts
            .iter()
            .filter_map(|&s| match self.ast.node(s) {
                Node::Let { name, .. }
                | Node::Const { name, .. }
                | Node::Record { name, .. }
                | Node::Protocol { name, .. }
                | Node::Parameter { name, .. }
                | Node::ModuleDecl { name, .. } => Some(name.clone()),
                Node::Callable { name: Some(n), .. } => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    /// Declares a binding: a module `global` when directly at module level, else a
    /// local slot in the current frame (recording a decl-site resolution). A same-
    /// scope/same-namespace duplicate is reported (L§5.2) but still bound, for
    /// recovery; else a hidden outer binding warns (L§5.1). (Duplicate *parameters*
    /// are not checked here — params have no node span; deferred.)
    pub(super) fn declare_binding(&mut self, decl: NodeId, name: &str, kind: GlobalKind) {
        if self.module_direct {
            if self.globals.iter().any(|g| &*g.name == name) {
                self.duplicate_error(decl, name);
            }
            self.globals.push(GlobalDecl {
                name: name.into(),
                kind,
                decl,
            });
        } else {
            if self.scope_has(name) {
                self.duplicate_error(decl, name);
            } else {
                self.check_shadowing(decl, name);
            }
            let slot = self.declare_local(name, kind);
            self.set_res(decl, Resolution::LocalSlot(slot));
        }
    }

    /// Whether the current (innermost) scope already binds `name`.
    fn scope_has(&self, name: &str) -> bool {
        self.scopes
            .last()
            .expect("a scope is open")
            .bindings
            .iter()
            .any(|b| &*b.name == name)
    }

    /// Assigns the next slot in the current frame to `name` and binds it in the
    /// current scope, with declaration `kind` (which decides assignability — S-6
    /// rule 2a). For params, whose slot and scope binding are staged separately
    /// (L§8.2), see [`alloc_frame_slot`] + [`bind_in_scope`].
    pub(super) fn declare_local(&mut self, name: &str, kind: GlobalKind) -> u16 {
        let slot = self.alloc_frame_slot(name);
        self.bind_in_scope(name, slot, kind);
        slot
    }

    /// Allocates the next slot in the current frame for `name` — frame side only
    /// (name + a `cell_boxed = false` entry), no scope binding.
    fn alloc_frame_slot(&mut self, name: &str) -> u16 {
        let frame = self.frames.last_mut().expect("a frame is open");
        let slot = u16::try_from(frame.next_slot).expect("frame exceeds the u16 slot space");
        frame.next_slot += 1;
        frame.slot_names.push(name.into());
        frame.cell_boxed.push(false); // a plain local; flipped true if captured
        slot
    }

    /// Binds `name` → `slot` (declaration `kind`) in the current scope.
    fn bind_in_scope(&mut self, name: &str, slot: u16, kind: GlobalKind) {
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .bindings
            .push(Binding {
                name: name.into(),
                slot,
                kind,
            });
    }
}
