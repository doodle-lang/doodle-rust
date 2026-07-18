//! Name-*reference* classification for the resolver walk: resolving each
//! `Node::Ident` use to a local slot, a block static link, a closure **capture**
//! (representation B, resolver-design §8), or a free module name. Split from
//! `walk/mod.rs` (which holds the scope/frame engine and *declaration* side) for
//! length; part of the same [`Resolver`](super::Resolver) walk.

use super::{FrameCapture, FrameKind};
use crate::ast::NodeId;
use crate::resolve::{CaptureFrom, CaptureSource, NameRef, Resolution};

impl super::Resolver<'_> {
    /// Resolves a name *reference* at `node`: a local slot, a block static link, a
    /// closure capture (cross-`fn`), or a free module name.
    pub(super) fn resolve_ref(&mut self, node: NodeId, name: &str) {
        match self.lookup(name) {
            Some((frame, slot, _)) => {
                let cur = self.frames.len() - 1;
                if frame == cur {
                    self.set_res(node, Resolution::LocalSlot(slot));
                } else if frame >= self.home_fn(cur) {
                    let hops = u16::try_from(cur - frame).expect("block nesting exceeds u16");
                    self.set_res(node, Resolution::BlockOuter { hops, slot });
                } else {
                    // Crosses an `fn` boundary → a closure capture (representation B).
                    self.resolve_capture(node, name, frame, slot);
                }
            }
            None => self.record_name_ref(node, name),
        }
    }

    /// Resolves a cross-`fn` reference (a closure **capture**, representation B,
    /// resolver-design §8): cell-box the owner's slot, thread the cell through
    /// every intervening closure as a trailing capture slot, and point the
    /// reference at its home `fn`'s capture slot (deref driven by `cell_boxed`).
    fn resolve_capture(&mut self, node: NodeId, name: &str, owner_frame: usize, owner_slot: u16) {
        let cur = self.frames.len() - 1;
        // The owner's slot becomes cell-boxed (a nested `fn` captures it). A late
        // promotion: refs already emitted as LocalSlot/BlockOuter stay valid — the
        // flag drives the runtime deref.
        self.frames[owner_frame].cell_boxed[owner_slot as usize] = true;
        // Thread the cell up through each closure (`Fn` frame) in `(owner, cur]`,
        // tracking where it now lives ("one level down") as we go.
        let origin = (owner_frame, owner_slot);
        let (mut cell_frame, mut cell_slot) = (owner_frame, owner_slot);
        for f in (owner_frame + 1)..=cur {
            if self.frames[f].kind != FrameKind::Fn {
                continue; // blocks don't capture — they reach via static links
            }
            let cap_slot = match self.frame_capture_slot(f, origin) {
                Some(slot) => slot, // this closure already captures the cell
                None => {
                    // The cell is `hops` static links up from `f`'s creating frame
                    // (`frames[f-1]`). Totality invariant: that chase runs through
                    // Block frames only, never crossing a callable boundary — it
                    // holds by construction because every intervening `fn` frame is
                    // open, so the source is always the nearest enclosing closure.
                    // (Revisit if M1.11+ resolves nested `module` bodies: a `Module`
                    // frame could then sit above index 0, between owner and closure.)
                    let hops = u16::try_from((f - 1) - cell_frame)
                        .expect("capture static-link depth exceeds u16");
                    debug_assert!(
                        (cell_frame + 1..f).all(|j| self.frames[j].kind == FrameKind::Block),
                        "capture hops crossed a callable frame boundary"
                    );
                    let from = CaptureFrom {
                        hops,
                        slot: cell_slot,
                    };
                    self.alloc_capture_slot(f, name, origin, from)
                }
            };
            (cell_frame, cell_slot) = (f, cap_slot);
        }
        // The reference reaches the cell via its home `fn`'s capture slot.
        let home = self.home_fn(cur);
        let slot = self
            .frame_capture_slot(home, origin)
            .expect("the home fn captured the cell");
        if cur == home {
            self.set_res(node, Resolution::LocalSlot(slot));
        } else {
            let hops = u16::try_from(cur - home).expect("block nesting exceeds u16");
            self.set_res(node, Resolution::BlockOuter { hops, slot });
        }
    }

    /// The capture slot in frame `f` for `origin`, if `f` already captures it.
    fn frame_capture_slot(&self, f: usize, origin: (usize, u16)) -> Option<u16> {
        self.frames[f]
            .captures
            .iter()
            .find(|c| c.origin == origin)
            .map(|c| c.source.slot)
    }

    /// Allocates a fresh trailing capture slot in frame `f` (a closure) for
    /// `origin`, named `name`, filled from `from` at closure creation.
    fn alloc_capture_slot(
        &mut self,
        f: usize,
        name: &str,
        origin: (usize, u16),
        from: CaptureFrom,
    ) -> u16 {
        let frame = &mut self.frames[f];
        let slot = u16::try_from(frame.next_slot).expect("frame exceeds the u16 slot space");
        frame.next_slot += 1;
        frame.slot_names.push(name.into());
        frame.cell_boxed.push(true); // a capture slot holds a cell reference
        frame.captures.push(FrameCapture {
            origin,
            source: CaptureSource { slot, from },
        });
        slot
    }

    /// Records a free-name reference (resolves to a module cell lazily at load).
    pub(super) fn record_name_ref(&mut self, node: NodeId, name: &str) {
        let idx = u32::try_from(self.name_refs.len()).expect("name_refs exceeds u32");
        self.name_refs.push(NameRef {
            name: name.into(),
            site: node,
        });
        self.set_res(node, Resolution::ModuleName(idx));
    }
}
