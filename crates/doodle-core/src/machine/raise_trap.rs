//! Raise-trapping (engine spec E§8.7, S-18): a host debug toggle that pauses the drive
//! `Paused(RaiseTrap)` at the point an exception is raised — **before the stack unwinds** —
//! so the debugger inspects the raising frame with the stack intact; resuming continues the
//! unwind exactly as if the trap had not fired. Independent of whether the exception is later
//! caught by a `try` (the trap fires at the raise, before the handler search).
//!
//! **Unified across all three raise sources** (S-18): a program `raise`, a foreign/host
//! `resolve(Raise)`, and an engine-generated raise all arm one in-flight `Unwind::Raise`, so
//! trapping keys off that single chokepoint. The one-shot `trapped` flag on the armed raise
//! makes the pause fire exactly once per raise — the drive loop [`take_raise_trap`] sets it,
//! the resumed drive sees it set and steps into the unwind instead of re-trapping.
//!
//! Raise-trapping is a **host directive**, like breakpoints and stepping: outside replay
//! identity (E§7.7). The drive honors it under a `Continue`/`Step*` directive; a
//! `RunToCompletion` run ignores it (the M6 directive-semantics matrix).

use super::{Handle, Instance, Position, unwind};

impl Instance {
    /// Enables or disables **raise-trapping** (E§8.7). Off by default; set between drives
    /// (like the breakpoint set, not a thread-safe control). When on, the drive pauses
    /// `Paused(RaiseTrap)` at each raise before the stack unwinds, under a `Continue`/`Step*`
    /// directive.
    pub fn set_raise_trapping(&mut self, enabled: bool) {
        self.machine.raise_trap_enabled = enabled;
    }

    /// Whether raise-trapping is currently enabled (E§8.7).
    pub fn raise_trapping(&self) -> bool {
        self.machine.raise_trap_enabled
    }

    /// The exception value the drive is **paused mid-raise** on (E§8.7), as a fresh host-owned
    /// handle (release it), or `None` if no raise is in flight. Read at a `Paused(RaiseTrap)`
    /// stop, alongside the intact stack ([`stack_walk`](Self::stack_walk)) and the raise site
    /// ([`trapped_raise_position`](Self::trapped_raise_position)).
    pub fn trapped_raise(&mut self) -> Option<Handle> {
        let value = match &self.machine.unwind {
            Some(unwind::Unwind::Raise { value, .. }) => *value,
            _ => return None,
        };
        Some(self.intern(value))
    }

    /// The source position of the raise the drive is **paused mid-raise** on (E§8.7): the
    /// raise site captured in the in-flight trace, in the raising (innermost) frame's module.
    /// `None` if no raise is in flight, or the raise carried no position.
    pub fn trapped_raise_position(&self) -> Option<Position> {
        let Some(unwind::Unwind::Raise { trace, .. }) = &self.machine.unwind else {
            return None;
        };
        let span = trace.raised_at?;
        let module = self.machine.frames.last()?.module;
        Some(Position { module, span })
    }

    /// Consumes a raise-trap opportunity (E§8.7): if raise-trapping is enabled and a
    /// freshly-armed (not-yet-trapped) `Raise` unwind is in flight, marks it trapped and
    /// returns `true` — the drive loop then stops `Paused(RaiseTrap)` with the stack intact,
    /// **before** entering the unwind. Marking it makes the pause one-shot: the resumed drive
    /// sees `trapped` set and steps into the unwind rather than re-trapping the same raise.
    /// Returns `false` when trapping is off, no raise is in flight, or this raise was already
    /// trapped.
    pub(crate) fn take_raise_trap(&mut self) -> bool {
        if !self.machine.raise_trap_enabled {
            return false;
        }
        if let Some(unwind::Unwind::Raise { trapped, .. }) = &mut self.machine.unwind
            && !*trapped
        {
            *trapped = true;
            return true;
        }
        false
    }
}
