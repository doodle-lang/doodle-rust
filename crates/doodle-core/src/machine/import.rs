//! The instance side of module loading (engine spec E§6, S-60): building the host-facing
//! import request and consuming a host import resolution. On `Source` the engine parses
//! the fetched module, seeds it, and pushes its top-level frame so the resumed drive runs
//! it (the importer stays parked beneath); `NotFound`/`Raise` arm a raise at the `import`
//! site. Split from `machine.rs` so it stays within the hygiene length limit; it carries
//! its own `impl Instance`, as `load.rs`/`lifecycle.rs` do.

use super::error::ExceptionKind;
use super::frame::Frame;
use super::modload::{LoadState, PendingImport, Suspension};
use super::{CellIdx, Cont, Handle, HandleError, Instance, LoadedModule};
use crate::drive::ImportRequest;
use crate::resolve::BodyKind;
use crate::span::{ModuleId, Span};
use std::sync::Arc;

impl Instance {
    /// The host-facing import request for the parked suspension (E§6): the requested
    /// dotted path (as segments) and the importing module's id (for host diagnostics).
    pub(crate) fn import_request(&self) -> ImportRequest {
        match &self.machine.pending {
            Some(Suspension::Import(p)) => ImportRequest {
                path: p.path.iter().map(|s| s.to_string()).collect(),
                importer: p.importer.0,
            },
            _ => unreachable!("import_request requires a parked import suspension"),
        }
    }

    /// Takes the parked import request, asserting the suspension is an import (an import
    /// resolution only follows an import suspend; a capability suspend is consumed by
    /// `resolve`).
    fn take_import(&mut self) -> PendingImport {
        match self.machine.pending.take() {
            Some(Suspension::Import(p)) => p,
            _ => unreachable!("an import resolution requires a parked import suspension"),
        }
    }

    /// Resolves a parked import with the module's `text` and host `canonical_id` (E§6): if
    /// the canonical id names a module already loaded, alias the requested path to it
    /// (singleton, L§11.3); otherwise parse + resolve + seed the module, register it
    /// `Loading`, and push its top-level frame so the resumed drive runs it. A fetched
    /// source with **static errors** instead enters `failed` retaining a `module-load-error`
    /// and raises it at the `import` site (S-8, E§3.2) — a re-import re-raises that value.
    pub(crate) fn load_import_source(&mut self, text: &str, canonical_id: &str) {
        let pending = self.take_import();
        // Singleton by canonical identity (L§11.3): a distinct path the host maps to a
        // module already in the table — alias, do not reload. The re-pushed
        // `ImportTargets` cont then finds it loaded (or failed) via `by_path` and acts.
        if let Some(existing) = self.machine.load.by_canonical(canonical_id) {
            self.machine.load.alias_path(&pending.path, existing);
            return;
        }
        let id = ModuleId(self.modules.len() as u32);
        let nfc = crate::source::normalize(text);
        let parsed = crate::parse::parse_program(nfc.as_ref(), id);
        // Resolve unconditionally, so even a parse-errored module yields a (partial)
        // resolved module to register in `failed`. A parse error takes priority over any
        // resolve cascade it induces for the reported message.
        let parse_error = first_error(&parsed.diagnostics);
        let out = crate::resolve::resolve(parsed.ast, parsed.root, id);
        let module = out.module;
        let static_error = parse_error.or_else(|| first_error(&out.diagnostics));
        let namespace = super::load::seed_namespace(
            &module,
            &mut self.heap,
            self.machine.error_type,
            &self.machine.intrinsics,
        );
        // This module's namespace cells join the instance's permanent GC roots (AD5): its
        // globals live for the instance, so a later collection during any module's step
        // keeps them alive. (A `failed` module never runs, but rooting its cells is
        // harmless and keeps the table uniform.)
        self.machine
            .module_root_cells
            .extend(namespace.iter().map(|(_, cell)| *cell));
        if let Some(diagnostic) = static_error {
            self.enter_failed_module(&pending, module, namespace, id, canonical_id, diagnostic);
            return;
        }
        // Clean load: build the top-level frame and push the module `Loading`.
        let top = module
            .callables
            .iter()
            .position(|c| matches!(c.kind, BodyKind::ModuleTopLevel))
            .expect("a resolved module has a top-level callable");
        let raw = vec![None; module.callables[top].slot_count as usize];
        let locals = super::local::build(&module, &mut self.heap, top, &raw, &[]);
        let root = module.root;
        let serial = self.machine.next_frame_serial();
        let frame = Frame::module_top_level(
            id,
            locals,
            Cont::Seq {
                block: root,
                next: 0,
            },
            serial,
        );
        self.machine.load.begin(&pending.path, canonical_id, id);
        self.modules.push(LoadedModule {
            resolved: Arc::new(module),
            namespace,
        });
        self.machine.frames.push(frame);
    }

    /// Registers a statically-broken fetched module (E§3.2 `LoadError`) as `failed` and
    /// raises its `module-load-error` at the `import` site. The `Error` value is both
    /// **retained** in the `failed` state (so a re-import re-raises it unchanged, S-8) and
    /// armed as the raise now. The module is pushed but never executed — the table stays
    /// parallel to the load-state registry.
    fn enter_failed_module(
        &mut self,
        pending: &PendingImport,
        module: crate::resolve::ResolvedModule,
        namespace: Vec<(Box<str>, CellIdx)>,
        id: ModuleId,
        canonical_id: &str,
        diagnostic: String,
    ) {
        let message = format!(
            "the module `{}` could not be loaded: {diagnostic}",
            super::modload::join_path(&pending.path)
        );
        let value = super::exception::make_error(
            &mut self.heap,
            self.machine.error_type,
            ExceptionKind::ModuleLoadError.slug(),
            &message,
        );
        self.machine.load.begin(&pending.path, canonical_id, id);
        self.machine.load.set_state(id, LoadState::Failed(value));
        self.modules.push(LoadedModule {
            resolved: Arc::new(module),
            namespace,
        });
        // No sub-module frame was pushed, so the importer is still the top frame: capture
        // the trace against it, at the `import` site.
        let trace = super::observe::capture_trace(
            self.current_resolved(),
            &self.heap,
            &self.machine,
            Some(pending.span),
        );
        self.machine.arm_raise_value(value, trace);
    }

    /// Resolves a parked import with `NotFound` (E§6): raises `module-not-found` at the
    /// `import` site in the importer.
    pub(crate) fn raise_import_not_found(&mut self) {
        let pending = self.take_import();
        let message = format!(
            "the module `{}` was not found",
            super::modload::join_path(&pending.path)
        );
        self.arm_import_raise(ExceptionKind::ModuleNotFound, &message, pending.span);
    }

    /// Resolves a parked import with a host `Raise` (E§6): raises the host-supplied value
    /// at the `import` site (e.g. a failed network fetch). Errors on a stale handle.
    pub(crate) fn raise_import_value(&mut self, handle: Handle) -> Result<(), HandleError> {
        let pending = self.take_import();
        let value = self.machine.handles.resolve(handle)?;
        let trace = super::observe::capture_trace(
            self.current_resolved(),
            &self.heap,
            &self.machine,
            Some(pending.span),
        );
        self.machine.arm_raise_value(value, trace);
        Ok(())
    }

    /// Materializes an engine `Error` of `kind`/`message` and arms it as a raise at `span`
    /// in the importer (E§9): the trace is captured against the importer's resolved module
    /// (the top frame — no sub-module frame was pushed on this path).
    fn arm_import_raise(&mut self, kind: ExceptionKind, message: &str, span: Span) {
        let trace = super::observe::capture_trace(
            self.current_resolved(),
            &self.heap,
            &self.machine,
            Some(span),
        );
        let value = super::exception::make_error(
            &mut self.heap,
            self.machine.error_type,
            kind.slug(),
            message,
        );
        self.machine.arm_raise_value(value, trace);
    }
}

/// The first `Error`-severity diagnostic's message, if any (a fetched module that does
/// not compile). Warnings do not block loading.
fn first_error(diagnostics: &[crate::diag::Diagnostic]) -> Option<String> {
    diagnostics
        .iter()
        .find(|d| d.severity == crate::diag::Severity::Error)
        .map(|d| d.message.clone())
}
