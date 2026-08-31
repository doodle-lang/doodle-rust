//! Breakpoints (engine spec E§8.6, S-21): the host sets them at a source position — a
//! module **canonical id** plus a line — and the engine stops `Paused(Breakpoint(id))` at
//! the first statement safe point at or after that position, under a `Continue`/`Step*`
//! directive (`RunToCompletion` ignores them; the drive wiring is in [`crate::drive`]).
//!
//! **Addressing is by the host-owned canonical id** (§6), not an engine module index:
//! module indices are internal load-order values, while the canonical id is the stable name
//! the host, the load-diagnostics record, and replay artifacts already share. A breakpoint
//! whose canonical id names a module **not yet loaded** — or a file never imported — is
//! **pending**, not an error, so the set-then-run gutter flow works for modules that load
//! mid-drive. Breakpoints **re-resolve at every load of their canonical**
//! ([`Instance::reresolve_breakpoints`]): resolution snaps forward to the first safe point at
//! or after the line (first on the line wins); a line with no safe point at or after it stays
//! pending and unhittable. That re-resolution is also the canonical-id-reuse/reload rule.
//!
//! Matching at run time is by the **resolved statement node**: the drive records the
//! statement about to run at each safe point (`machine.safe_point_stmt`) and
//! [`Instance::breakpoint_hit`] tests it, so a breakpoint inside a loop re-fires every
//! iteration for free. Breakpoints are **host directives**, like stepping — outside replay
//! identity (E§7.7).

use super::Instance;
use crate::ast::NodeId;
use crate::drive::BreakpointId;
use crate::resolve::ResolvedModule;
use crate::span::ModuleId;

/// One installed breakpoint (E§8.6): the host-owned canonical id + line it was set at, and
/// its current mapping to a concrete statement safe point. `resolved` is `None` when the
/// breakpoint is **pending** — the canonical names no loaded module, or no safe point lies at
/// or after `line` (unhittable). Recomputed at every load of the canonical (S-21).
struct BreakpointEntry {
    id: BreakpointId,
    canonical: Box<str>,
    line: u32,
    resolved: Option<(ModuleId, NodeId)>,
}

/// The instance's breakpoint set (E§8.6), held on the [`Machine`](super::Machine). Ids are
/// allocated monotonically so a cleared id is never reused (a stale host reference cannot
/// alias a new breakpoint). The entry list is scanned linearly — small and deterministic, no
/// hashing on an observable path (E§11).
pub(crate) struct Breakpoints {
    next_id: u32,
    entries: Vec<BreakpointEntry>,
}

impl Breakpoints {
    /// An empty breakpoint set.
    pub(crate) fn new() -> Self {
        Breakpoints {
            next_id: 0,
            entries: Vec::new(),
        }
    }
}

/// A breakpoint as the host reads it back (E§8.6 `breakpoints()`): the id, the canonical id +
/// line it was set at, and whether it is currently **resolved** to a hittable safe point
/// (`false` = pending — the module is not loaded, or the line has no safe point at or after
/// it), so a host can gray an unhittable gutter mark.
#[derive(Clone, Debug)]
pub struct BreakpointInfo {
    /// The breakpoint's id (from [`Instance::set_breakpoint`]).
    pub id: BreakpointId,
    /// The canonical id it was set on.
    pub canonical_id: String,
    /// The 1-based source line it was set on.
    pub line: u32,
    /// Whether it currently maps to a hittable safe point (`false` = pending).
    pub resolved: bool,
}

/// Resolves a breakpoint `line` to the first statement safe point at or after it in
/// `resolved` (S-21): the statement whose start line is the least `>= line`, and among the
/// statements sharing that line the earliest in source (least byte offset). `None` if no
/// statement lies at or after the line (a breakpoint past the last code — pending and
/// unhittable). Reads the module's `stmt_spans` (every statement node, E§8.6) and the AST
/// line index (§8.1); code-less lines snap forward because only statement starts are keys.
fn resolve_line(resolved: &ResolvedModule, line: u32) -> Option<NodeId> {
    resolved
        .stmt_spans
        .iter()
        .filter_map(|(span, node)| {
            let stmt_line = resolved.ast.line_of(span.start);
            (stmt_line >= line).then_some((stmt_line, span.start, *node))
        })
        .min_by_key(|(stmt_line, start, _)| (*stmt_line, *start))
        .map(|(_, _, node)| node)
}

impl Instance {
    /// Sets a breakpoint at (`canonical_id`, `line`) and returns its id (E§8.6). Resolves it
    /// immediately if that canonical is loaded; otherwise it is **pending** (installed, and
    /// resolved when the module loads — the set-then-run flow) — never an error, even for a
    /// canonical that names no module the program ever imports.
    pub fn set_breakpoint(&mut self, canonical_id: &str, line: u32) -> BreakpointId {
        let id = BreakpointId(self.machine.breakpoints.next_id);
        self.machine.breakpoints.next_id += 1;
        let resolved = self.resolve_breakpoint(canonical_id, line);
        self.machine.breakpoints.entries.push(BreakpointEntry {
            id,
            canonical: canonical_id.into(),
            line,
            resolved,
        });
        id
    }

    /// Clears the breakpoint `id` (E§8.6). A no-op if it was already cleared or never set —
    /// clearing is idempotent, and an id is never reused, so this cannot hit a different one.
    pub fn clear_breakpoint(&mut self, id: BreakpointId) {
        self.machine.breakpoints.entries.retain(|e| e.id != id);
    }

    /// The installed breakpoints (E§8.6), in the order they were set, each marked resolved or
    /// pending so a host can render the gutter (gray a pending/unhittable mark). Read at a
    /// stopped instance, like the rest of the observation surface.
    pub fn breakpoints(&self) -> Vec<BreakpointInfo> {
        self.machine
            .breakpoints
            .entries
            .iter()
            .map(|e| BreakpointInfo {
                id: e.id,
                canonical_id: e.canonical.to_string(),
                line: e.line,
                resolved: e.resolved.is_some(),
            })
            .collect()
    }

    /// Resolves (`canonical_id`, `line`) against the module loaded under that canonical, if
    /// any (E§8.6): its `(ModuleId, NodeId)` safe point, or `None` when the canonical is not
    /// loaded or the line has no safe point at or after it (pending).
    fn resolve_breakpoint(&self, canonical_id: &str, line: u32) -> Option<(ModuleId, NodeId)> {
        let module = self.machine.load.by_canonical(canonical_id)?;
        let resolved = &self.modules[module.0 as usize].resolved;
        resolve_line(resolved, line).map(|node| (module, node))
    }

    /// Re-resolves every breakpoint on `canonical_id` against the module now loaded under it
    /// (S-21): called at each load of that canonical, so a set-then-run breakpoint on a
    /// mid-drive import — or one on a reloaded module (canonical-id reuse) — snaps to the
    /// freshly loaded source. A line that now has no safe point at or after it goes back to
    /// pending. No-op if the canonical is (still) not loaded.
    pub(crate) fn reresolve_breakpoints(&mut self, canonical_id: &str) {
        let Some(module) = self.machine.load.by_canonical(canonical_id) else {
            return;
        };
        // `self.modules` and `self.machine` are disjoint fields, so the immutable module
        // borrow and the mutable entry borrow do not conflict.
        let resolved = &self.modules[module.0 as usize].resolved;
        for entry in &mut self.machine.breakpoints.entries {
            if entry.canonical.as_ref() == canonical_id {
                entry.resolved = resolve_line(resolved, entry.line).map(|node| (module, node));
            }
        }
    }

    /// The breakpoint (if any) at the current statement safe point (E§8.6): the statement the
    /// machine just reached (recorded as `machine.safe_point_stmt`) in the active frame's
    /// module, tested against the resolved breakpoints. `None` when this safe point is not a
    /// statement (a call-entry or return point) or no breakpoint sits on the statement. The
    /// drive loop calls it under a `Continue`/`Step*` directive only (`RunToCompletion`
    /// ignores breakpoints). Matching by node re-fires a loop-body breakpoint each iteration.
    pub(crate) fn breakpoint_hit(&self) -> Option<BreakpointId> {
        let node = self.machine.safe_point_stmt?;
        let module = self.machine.frames.last()?.module;
        self.machine
            .breakpoints
            .entries
            .iter()
            .find(|e| e.resolved == Some((module, node)))
            .map(|e| e.id)
    }
}
