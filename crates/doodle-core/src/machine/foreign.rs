//! The foreign-value slice of the host↔value boundary API (engine spec E§4.5): the
//! `make_foreign` constructor and the `foreign_tag`/`foreign_ptr` readers, split from
//! [`boundary`](super::boundary) for length. A foreign value is an **opaque host
//! object** Doodle can hold, pass, and store by identity (L§4.13) but not inspect: it
//! has no fields and no arithmetic (attempting either raises). Its optional finalizer
//! runs **exactly once** — at the collection that reclaims it, or at
//! [`destroy`](super::Instance::destroy) if it is still live — to release the
//! underlying host resource, and is never Doodle-observable (its effects cannot
//! re-enter the instance, so determinism is preserved, E§11).

use super::boundary::ValueError;
use super::values;
use super::{Handle, Instance};
use crate::heap::Finalizer;

impl Instance {
    /// Constructs a foreign (host) value (E§4.5). `tag` is the host's type
    /// discriminator; `ptr` is an opaque host pointer returned verbatim by
    /// [`foreign_ptr`](Self::foreign_ptr) and handed to `finalizer`. The optional
    /// `finalizer` runs **exactly once** — at the collection that reclaims the value,
    /// or at [`destroy`](Self::destroy) if it is still live then — to release the
    /// underlying resource.
    pub fn make_foreign(&mut self, tag: u64, ptr: u64, finalizer: Option<Finalizer>) -> Handle {
        values::make_foreign(
            &mut self.machine.handles,
            &mut self.heap,
            tag,
            ptr,
            finalizer,
        )
    }

    /// The host type `tag` of a foreign value (E§4.5). Errors if the value is not a
    /// foreign value ([`ValueError::WrongKind`]) or its handle is stale.
    pub fn foreign_tag(&self, handle: Handle) -> Result<u64, ValueError> {
        values::foreign_tag(&self.machine.handles, &self.heap, handle)
    }

    /// The opaque host `ptr` of a foreign value (E§4.5), returned verbatim. Errors if
    /// the value is not a foreign value ([`ValueError::WrongKind`]) or its handle is stale.
    pub fn foreign_ptr(&self, handle: Handle) -> Result<u64, ValueError> {
        values::foreign_ptr(&self.machine.handles, &self.heap, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::Limits;
    use crate::machine::{Kind, Registry};
    use crate::span::ModuleId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

    /// A `Ready` instance over a trivial clean-loading program — foreign values need only
    /// the heap + handle table, not a running program.
    fn instance() -> Instance {
        use crate::diag::Severity;
        let nfc = crate::source::normalize("1\n");
        let parsed = crate::parse::parse_program(nfc.as_ref(), ModuleId(0));
        assert!(
            !parsed
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error),
            "unexpected parse error(s): {:?}",
            parsed.diagnostics
        );
        let resolved = crate::resolve::resolve(parsed.ast, parsed.root, ModuleId(0));
        assert!(
            resolved.diagnostics.is_empty(),
            "unexpected resolve diagnostic(s): {:?}",
            resolved.diagnostics
        );
        Instance::load(resolved.module, Limits::default(), Registry::new(), "main")
    }

    /// Wraps `f` as a boxed [`Finalizer`] (the coercion to `dyn FnOnce` needs the target
    /// type, which the return type supplies). `+ Send` mirrors the [`Finalizer`] alias, so
    /// these tests exercise the same bound the C-ABI trampoline satisfies (state shared with
    /// the test is `Arc`/atomic, not `Rc`).
    fn finalizer(f: impl FnOnce(u64) + Send + 'static) -> Option<Finalizer> {
        Some(Box::new(f))
    }

    #[test]
    fn tag_and_ptr_round_trip_and_report_foreign_kind() {
        let mut inst = instance();
        let h = inst.make_foreign(42, 99, None);
        assert_eq!(inst.kind_of(h), Ok(Kind::Foreign));
        assert_eq!(inst.foreign_tag(h), Ok(42));
        assert_eq!(inst.foreign_ptr(h), Ok(99));
    }

    #[test]
    fn a_foreign_reader_on_a_non_foreign_reports_wrong_kind() {
        let mut inst = instance();
        let i = inst.make_int(1);
        assert_eq!(
            inst.foreign_tag(i),
            Err(ValueError::WrongKind {
                expected: Kind::Foreign,
                got: Kind::Int,
            })
        );
    }

    #[test]
    fn an_unreachable_foreign_runs_its_finalizer_once_at_gc() {
        let mut inst = instance();
        let count = Arc::new(AtomicU32::new(0));
        let seen_ptr = Arc::new(AtomicU64::new(0));
        let (c, p) = (count.clone(), seen_ptr.clone());
        let h = inst.make_foreign(
            1,
            77,
            finalizer(move |ptr| {
                c.fetch_add(1, Relaxed);
                p.store(ptr, Relaxed);
            }),
        );
        inst.release(h).unwrap(); // now unreachable
        inst.force_collect();
        assert_eq!(
            count.load(Relaxed),
            1,
            "finalizer ran once at the reclaiming collection"
        );
        assert_eq!(
            seen_ptr.load(Relaxed),
            77,
            "the finalizer received the host ptr"
        );
        // The slot is freed and its finalizer taken, so a later collection cannot run it
        // again — exactly-once.
        inst.force_collect();
        assert_eq!(
            count.load(Relaxed),
            1,
            "not re-run by a subsequent collection"
        );
    }

    #[test]
    fn a_retained_foreign_is_not_finalized_at_gc() {
        let mut inst = instance();
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        // Keep the handle (a GC root), so the value survives collection unfinalized.
        let _h = inst.make_foreign(
            1,
            0,
            finalizer(move |_| {
                c.fetch_add(1, Relaxed);
            }),
        );
        inst.force_collect();
        assert_eq!(
            count.load(Relaxed),
            0,
            "a reachable foreign is not finalized"
        );
    }

    #[test]
    fn destroy_finalizes_every_live_foreign_once() {
        let mut inst = instance();
        let count = Arc::new(AtomicU32::new(0));
        let (c1, c2) = (count.clone(), count.clone());
        // Two live foreign values (handles held); destroy must finalize both, once each.
        let _a = inst.make_foreign(
            1,
            10,
            finalizer(move |_| {
                c1.fetch_add(1, Relaxed);
            }),
        );
        let _b = inst.make_foreign(
            2,
            20,
            finalizer(move |_| {
                c2.fetch_add(1, Relaxed);
            }),
        );
        inst.destroy();
        assert_eq!(
            count.load(Relaxed),
            2,
            "both live finalizers ran once at destroy"
        );
    }

    #[test]
    fn a_gc_finalized_foreign_is_not_finalized_again_at_destroy() {
        let mut inst = instance();
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let h = inst.make_foreign(
            1,
            0,
            finalizer(move |_| {
                c.fetch_add(1, Relaxed);
            }),
        );
        inst.release(h).unwrap();
        inst.force_collect();
        assert_eq!(count.load(Relaxed), 1, "finalized at GC");
        // The value is gone; destroy must not run its finalizer a second time.
        inst.destroy();
        assert_eq!(
            count.load(Relaxed),
            1,
            "destroy does not re-finalize a collected value"
        );
    }

    #[test]
    fn a_foreign_without_a_finalizer_is_inert_at_gc_and_destroy() {
        let mut inst = instance();
        let h = inst.make_foreign(1, 0, None);
        inst.release(h).unwrap();
        inst.force_collect(); // no finalizer: nothing to run, no panic
        let _live = inst.make_foreign(2, 0, None);
        inst.destroy(); // a live no-finalizer foreign: also fine
    }

    #[test]
    fn a_panicking_finalizer_is_isolated_from_its_peers() {
        // A finalizer must not unwind (host contract); a buggy one that does is caught, so
        // it neither skips its peers' finalizers (which would leak their resources) nor
        // aborts the process at destroy. Both finalizers here run; the panic is contained.
        let mut inst = instance();
        let count = Arc::new(AtomicU32::new(0));
        let (c_panic, c_ok) = (count.clone(), count.clone());
        let _a = inst.make_foreign(
            1,
            0,
            finalizer(move |_| {
                c_panic.fetch_add(1, Relaxed);
                panic!("a finalizer that misbehaves");
            }),
        );
        let _b = inst.make_foreign(
            2,
            0,
            finalizer(move |_| {
                c_ok.fetch_add(1, Relaxed);
            }),
        );
        // Silence the expected panic's default hook so it does not clutter test output.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        inst.destroy();
        std::panic::set_hook(prev);
        assert_eq!(
            count.load(Relaxed),
            2,
            "the panicking finalizer's peer still ran"
        );
    }
}
