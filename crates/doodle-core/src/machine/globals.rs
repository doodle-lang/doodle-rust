//! Module-globals observation (engine spec E§8.2): the module-level bindings in scope in a
//! frame's home module — the `let`/`const`/`parameter` **variables** plus the `to`/`fn`/`record`/
//! `protocol`/`module` declarations — as a pull-based, lazy, **read-only** observation surface
//! like frame locals (mutation is §8.9 live-edit territory, not inspection). Module-level
//! `let`/`const` are *globals* (a module's namespace, machine-design §2), **not** frame slots, so
//! a top-level program's variables live here, not in [`frame_locals`](Instance::frame_locals).
//! The host shows a module's globals **once**, keyed off the selected frame's home
//! [`module`](super::FrameObservation::module) (they are module-scoped, not per-frame).

use super::control;
use super::{Handle, Instance};
use crate::resolve::GlobalKind;

/// A module-level binding the debugger lists (E§8.2): its `name`, declaration `kind` (the host
/// filters — the *variables* are `Let`/`Const`/`Parameter`; `Proc`/`Fn`/`Record`/`Protocol`/
/// `Module` are the other declarations), and `slot` — its index, the key for
/// [`module_global_value`](Instance::module_global_value). Value-free; read the value lazily.
#[derive(Clone, Debug)]
pub struct GlobalBinding {
    /// The declared name.
    pub name: String,
    /// The declaration category (`Let`/`Const`/`Parameter`/…) — the engine stays policy-free
    /// and lets the host choose which kinds the panel shows.
    pub kind: GlobalKind,
    /// The binding's index in declaration order — the `slot` for [`module_global_value`].
    pub slot: usize,
}

impl Instance {
    /// The module-level binding names of module `module` (E§8.2), in declaration order, each with
    /// its `kind` and `slot` — the handle-free, eager half (the host filters by kind and reads a
    /// value lazily with [`module_global_value`](Self::module_global_value)). Empty for an
    /// out-of-range module. Module globals are in scope module-wide, so a host shows them **once**
    /// per module — keyed off the selected frame's home
    /// [`module`](super::FrameObservation::module), not repeated per frame.
    pub fn module_global_names(&self, module: usize) -> Vec<GlobalBinding> {
        let Some(loaded) = self.modules.get(module) else {
            return Vec::new();
        };
        loaded
            .resolved
            .globals
            .iter()
            .enumerate()
            .map(|(slot, global)| GlobalBinding {
                name: global.name.to_string(),
                kind: global.kind,
                slot,
            })
            .collect()
    }

    /// A fresh **host-owned** handle to the current value of module `module`'s `slot`-th global
    /// (the slot order of [`module_global_names`](Self::module_global_names)), or `None` if the
    /// slot is out of range or **not yet defined** — its `let`/`const`/`parameter` declaration
    /// has not executed (the module-level temporal dead zone). Never a fault, so a host can
    /// render at any safe point, including mid-module-load. A `parameter`'s value is its
    /// **current dynamic value** (the `with`-override in force, L§5.5) — its cell holds the live
    /// value — so a host watching a `with` block sees it change.
    pub fn module_global_value(&mut self, module: usize, slot: usize) -> Option<Handle> {
        let cell = {
            let loaded = self.modules.get(module)?;
            let global = loaded.resolved.globals.get(slot)?;
            control::find_cell(&loaded.namespace, &global.name)?
        };
        let value = self.heap.cell(cell).value?;
        Some(self.intern(value))
    }
}
