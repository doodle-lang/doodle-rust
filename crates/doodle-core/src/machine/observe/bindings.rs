//! The rich per-frame binding surface (engine spec E§8.2), split from the parent
//! `observe` module to keep that file within the hygiene length limit: a frame's named
//! **local bindings** (parameters + `let`/`const` in scope) and its `with`
//! **dynamic-parameter bindings**, each in both a cheap handle-free "names" form and a
//! lazy per-slot "value" form, plus the eager `Vec<`[`Binding`](super::Binding)`>`
//! conveniences. A child of `observe`, so it reuses that module's private frame helpers
//! (`frame_at`, `frame_body`, `cell_name`).

use super::super::frame::FrameKind;
use super::super::{Handle, Instance, local};
use super::Binding;

impl Instance {
    /// The **local-binding names** in scope in frame `index` (innermost = 0), in slot order —
    /// the pull model's cheap, handle-free half of [`frame_locals`](Self::frame_locals): a host
    /// lists names on every pause and mints a value only for a row it expands, via
    /// [`frame_local_value`](Self::frame_local_value). Empty for a block frame (it reads its
    /// enclosing frame's locals, §7) or an out-of-range `index`.
    pub fn frame_local_names(&self, index: usize) -> Vec<String> {
        let Some(actual) = self.frame_at(index) else {
            return Vec::new();
        };
        let Some((module, info_id)) = self.frame_body(actual) else {
            return Vec::new();
        };
        self.modules[module].resolved.callables[info_id]
            .slot_names
            .iter()
            .map(|n| n.to_string())
            .collect()
    }

    /// A fresh host-owned handle to the value of frame `index`'s `slot`-th local (the slot
    /// order of [`frame_local_names`](Self::frame_local_names)), or `None` if the slot is out
    /// of range or not yet initialized (the temporal dead zone). The lazy value half of the
    /// pull model — mints exactly the one binding a debugger row expands, not every local of
    /// every frame on every pause.
    pub fn frame_local_value(&mut self, index: usize, slot: usize) -> Option<Handle> {
        let actual = self.frame_at(index)?;
        // A block frame has no own named locals (it reads its enclosing frame's, §7), so its
        // slots are not `frame_local_names`'d; keep value addressing consistent by reporting
        // none for it — matching the empty name list.
        if matches!(self.machine.frames[actual].kind, FrameKind::Block { .. }) {
            return None;
        }
        let value = self.machine.frames[actual]
            .locals
            .get(slot)
            .and_then(|&l| local::read(&self.heap, l))?;
        Some(self.intern(value))
    }

    /// The **local bindings** in scope in frame `index` (innermost = 0, matching
    /// [`stack_walk`](Self::stack_walk)) — its parameters and `let`/`const` names, as
    /// name→value (E§8.2). A `None` value is a not-yet-initialized slot (the temporal dead
    /// zone). A block frame reports no own locals (it reads its enclosing frame's, §7); an
    /// out-of-range `index` returns empty. Mints a host-owned handle per bound value — the
    /// eager convenience over [`frame_local_names`](Self::frame_local_names) +
    /// [`frame_local_value`](Self::frame_local_value).
    pub fn frame_locals(&mut self, index: usize) -> Vec<Binding> {
        self.frame_local_names(index)
            .into_iter()
            .enumerate()
            .map(|(slot, name)| Binding {
                name,
                value: self.frame_local_value(index, slot),
            })
            .collect()
    }

    /// The **dynamic-parameter binding names** established by `with` in frame `index` (E§8.2,
    /// L§5.5), in push order — the handle-free half of
    /// [`frame_dynamic_bindings`](Self::frame_dynamic_bindings), paired with
    /// [`frame_dynamic_value`](Self::frame_dynamic_value) for the lazy value.
    pub fn frame_dynamic_names(&self, index: usize) -> Vec<String> {
        let Some(actual) = self.frame_at(index) else {
            return Vec::new();
        };
        let (start, end) = self.frame_dyn_range(actual);
        self.machine
            .dyn_stack
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .map(|(cell, _)| self.cell_name(*cell))
            .collect()
    }

    /// A fresh host-owned handle to the value of frame `index`'s `slot`-th `with` binding (the
    /// order of [`frame_dynamic_names`](Self::frame_dynamic_names)), or `None` if out of range
    /// or the cell is unbound.
    pub fn frame_dynamic_value(&mut self, index: usize, slot: usize) -> Option<Handle> {
        let actual = self.frame_at(index)?;
        let (start, end) = self.frame_dyn_range(actual);
        // Fail-soft on the slice like every sibling reader (a broken `dyn_depth` invariant returns
        // `None`, never panics — the observation surface must be safe at any stopped state).
        let cell = self
            .machine
            .dyn_stack
            .get(start..end)?
            .get(slot)
            .map(|(c, _)| *c)?;
        let value = self.heap.cell(cell).value?;
        Some(self.intern(value))
    }

    /// The **dynamic-parameter bindings** established by `with` within frame `index` (E§8.2,
    /// L§5.5), as name→current-value. The frame's `with`s are the `dyn_stack` entries it
    /// pushed (those between its `dyn_depth` and the next inner frame's); each names a module
    /// dynamic-parameter cell, whose live value is reported. Mints a host-owned handle each —
    /// the eager convenience over [`frame_dynamic_names`](Self::frame_dynamic_names) +
    /// [`frame_dynamic_value`](Self::frame_dynamic_value).
    pub fn frame_dynamic_bindings(&mut self, index: usize) -> Vec<Binding> {
        self.frame_dynamic_names(index)
            .into_iter()
            .enumerate()
            .map(|(slot, name)| Binding {
                name,
                value: self.frame_dynamic_value(index, slot),
            })
            .collect()
    }

    /// The `[start, end)` slice of `dyn_stack` holding frame `actual`'s own `with` bindings:
    /// from the frame's `dyn_depth` up to the next inner frame's (or the stack top).
    fn frame_dyn_range(&self, actual: usize) -> (usize, usize) {
        let start = self.machine.frames[actual].dyn_depth as usize;
        let end = self
            .machine
            .frames
            .get(actual + 1)
            .map_or(self.machine.dyn_stack.len(), |f| f.dyn_depth as usize);
        (start, end)
    }
}
