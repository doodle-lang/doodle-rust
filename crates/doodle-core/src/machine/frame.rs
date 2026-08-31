//! Execution frames (machine-design §8): one frame per active callable/block body
//! plus the module top level, each carrying a LIFO continuation stack.
//!
//! **Scope (M2a.7).** The module-top-level, callable (`to`/`fn`/lambda), and
//! block (`do … end`) frames. A block frame carries its **defining** link (the
//! static-link parent whose locals it reads, §7) and its **consumer** (who
//! invoked it, for `break`, §12); a callable frame carries the invoked callable
//! value. Every frame carries a `serial` (frame identity, **preserved** across a
//! tail-reuse), a `tail_count` (tail iterations absorbed, E§8.3), and, for a
//! callee with a `do name` block parameter, the bound [`BlockDescriptor`]. The
//! dynamic-parameter depth joins at M4.

use super::Value;
use super::cont::Cont;
use crate::ast::NodeId;
use crate::machine::{CalIdx, CellIdx};
use crate::span::ModuleId;

/// One frame-local slot (machine-design §7). A **direct** slot stores its value
/// inline; a **cell-boxed** slot holds a reference to a heap [`CellObj`](crate::heap::CellObj)
/// — its storage that a nested `fn` captured (representation B, §7/§10), so the
/// closure and the frame share one mutable binding. The resolver's `cell_boxed`
/// flag decides which a slot is at frame entry; thereafter the variant is
/// self-describing (reads/writes dereference a `Boxed` slot).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Local {
    /// A plain local; `None` = declared-but-unbound (the temporal dead zone).
    Direct(Option<Value>),
    /// A cell-boxed local: the value lives in the heap cell at this index.
    Boxed(CellIdx),
}

/// What a frame is running (machine-design §8 `FrameKind`).
pub(crate) enum FrameKind {
    /// The module top level. A module runs for effect, so it completes Void
    /// (L§6.11); at M1/M2a there is a single module, so the id is implicit.
    ModuleTopLevel,
    /// A `to`/`fn`/lambda body. Holds the **invoked callable value** (`CalIdx`),
    /// not just its `CallableId`: E§8.2 hands the frame's callable back as a
    /// handle and callable equality is identity (L§4.9), so the frame must carry
    /// the same `CalObj` the program called (machine-design §8). Its `CallableId`
    /// (via the `CalObj`) supplies the body kind for the return contract.
    Callable {
        /// The invoked callable value.
        cal: CalIdx,
    },
    /// A `do … end` block body (second-class, §8.5). Reads its enclosing locals
    /// through the **defining** static link (§7), not a capture; `break` exits its
    /// **consumer** (§12). A block yields its last expression's value to its
    /// invoker on each invocation.
    Block {
        /// The static-link parent frame (index into the frame stack) whose locals
        /// the block reads — the frame the block was *defined* in (§7).
        defining: usize,
        /// The defining frame's `serial`, checked on each static-link chase to
        /// catch a stale link (a reused frame slot; always valid until M2a.7).
        defining_serial: u64,
        /// Who invoked this block — the target `break` exits (§12).
        consumer: Consumer,
    },
}

/// Who received/invoked a block — the `break` target (machine-design §8/§12).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Consumer {
    /// A non-tail Doodle consumer: the frame that invoked the block (`break`
    /// unwinds through it inclusive, delivering the value as its call's result).
    DoodleCall {
        /// The invoking frame's index in the frame stack.
        frame: usize,
        /// The invoking frame's `serial` (integrity check).
        serial: u64,
    },
    /// A **native** consumer (E§5.4/§7.6, MD §14): the block was invoked by a native
    /// block-consuming function through a reentrant drive, so its consumer is on the
    /// far side of the host boundary — there is no consumer *frame*. A `break`
    /// targeting it unwinds the stack down to `boundary` (the frame depth the native
    /// call sits at) and parks there; the reentrant drive relays it as the S-46
    /// `NonLocalExit(Break)` and the native call's apply site completes the call.
    Native {
        /// The frame depth of the native block-consuming call — the block frame sits
        /// just above it, and a `break` targeting this consumer pops down to here.
        boundary: usize,
    },
    // `TailReused` (M2a.7) joins later.
}

/// A bound `do name` block parameter (machine-design §8/§10): a machine-internal
/// handle to a block argument — **never a Doodle value** (§8.5), so it lives here
/// on the frame rather than in `locals`. Invoking it pushes a [`FrameKind::Block`]
/// frame whose `defining`/`defining_serial` come from this descriptor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockDescriptor {
    /// The frame the block was defined in (its static-link parent, §7).
    pub(crate) defining: usize,
    /// That frame's `serial` at capture (integrity check).
    pub(crate) defining_serial: u64,
    /// The block body's `CallableId` (a [`crate::resolve::BodyKind::Block`]).
    pub(crate) callable: u32,
}

/// An execution frame: its kind, its local slots, and its continuation stack —
/// the frame's pending work, top = next to run (machine-design §8).
pub(crate) struct Frame {
    /// What this frame is running.
    pub(crate) kind: FrameKind,
    /// The module this frame executes in (machine-design §6, AD5): the module whose
    /// resolved AST and module-scope namespace its name reads and node lookups use. A
    /// callable frame runs in the module its callable was **defined** in (the `CalObj`'s
    /// module), a block in its defining frame's module, the top level in the loading
    /// module. A cross-module call therefore switches the active module by frame.
    pub(crate) module: ModuleId,
    /// Frame-local slots (machine-design §7), sized by the body's resolver
    /// `slot_count`. Each is direct or cell-boxed (see [`Local`]).
    pub(crate) locals: Vec<Local>,
    /// Pending work for this frame (LIFO).
    pub(crate) conts: Vec<Cont>,
    /// Frame identity (machine-design §8): distinguishes this activation from a
    /// reused frame slot; preserved across tail reuse (M2a.7).
    pub(crate) serial: u64,
    /// The bound `do name` block parameter, if this callee received one.
    pub(crate) block_param: Option<BlockDescriptor>,
    /// Per-frame tail-iteration counter (E§8.3): the number of tail calls this
    /// frame slot has absorbed by reuse. `0` for a fresh frame; `+= 1` on each
    /// reuse (machine-design §11). A `10⁷`-iteration tail loop runs in this one
    /// frame with `tail_count == 10⁷`.
    pub(crate) tail_count: u64,
    /// Where this frame was entered (E§8.2, MD §17): the `Call` node whose
    /// application pushed it, for `stack_walk`'s call-site position. `None` for the
    /// module top level (no call site) and for a block invoked by a **native**
    /// consumer (its invocation is host code, not a Doodle call). Updated on tail
    /// reuse to the new call.
    pub(crate) call_site: Option<NodeId>,
    /// The `dyn_stack` depth when this frame was entered (machine-design §13): the
    /// dynamic bindings this frame established are the entries above it, so E§8.2 can
    /// report per frame which `with`s it opened. Preserved across tail reuse — a tail
    /// call is never inside a `with` body (a barrier, L§8.7), so the stack is back at
    /// this depth when the reuse happens. Read by `frame_dynamic_bindings` (E§8.2).
    pub(crate) dyn_depth: u32,
}

impl Frame {
    /// A fresh module-top-level frame with pre-built `locals`, whose only pending
    /// work is `body_seq` (the sequencing continuation over the module's statements).
    pub(crate) fn module_top_level(
        module: ModuleId,
        locals: Vec<Local>,
        body_seq: Cont,
        serial: u64,
    ) -> Self {
        Frame {
            kind: FrameKind::ModuleTopLevel,
            module,
            locals,
            conts: vec![body_seq],
            serial,
            block_param: None,
            tail_count: 0,
            call_site: None,
            // The module frame is the first frame — nothing has opened a `with` yet.
            dyn_depth: 0,
        }
    }

    /// A fresh callable frame for invoking `cal`, with `locals` already holding
    /// the bound arguments (direct or cell-boxed) and `block_param` the bound
    /// `do name` block argument, if any. Its pending work is the body `Seq` over a
    /// [`ReturnBarrier`](Cont::ReturnBarrier) (machine-design §8/§10); parameter
    /// defaults are pushed on top by the caller so they run before the body.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn callable(
        module: ModuleId,
        cal: CalIdx,
        locals: Vec<Local>,
        body: NodeId,
        serial: u64,
        block_param: Option<BlockDescriptor>,
        call_site: NodeId,
        dyn_depth: u32,
    ) -> Self {
        Frame {
            kind: FrameKind::Callable { cal },
            module,
            locals,
            conts: callable_conts(body),
            serial,
            block_param,
            tail_count: 0,
            call_site: Some(call_site),
            dyn_depth,
        }
    }

    /// **Reuses** this frame in place for a tail call to `cal` (machine-design §11):
    /// overwrites its kind, locals, block parameter, and continuations with the
    /// callee's fresh activation, **preserving** the frame's `serial` and stack slot
    /// (logical depth) and bumping `tail_count`. This is what makes a tail loop run
    /// in constant memory. Only a kind-matched marked-tail call reaches here (S-55);
    /// parameter defaults are pushed on top by the caller.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reuse_as_callable(
        &mut self,
        module: ModuleId,
        cal: CalIdx,
        locals: Vec<Local>,
        body: NodeId,
        block_param: Option<BlockDescriptor>,
        call_site: NodeId,
    ) {
        self.kind = FrameKind::Callable { cal };
        // A tail call may cross into another module (the callee's), so update the active
        // module even though the frame slot and serial are preserved.
        self.module = module;
        self.locals = locals;
        self.conts = callable_conts(body);
        self.block_param = block_param;
        self.tail_count += 1;
        self.call_site = Some(call_site);
        // `serial`, the stack slot, and `dyn_depth` are deliberately preserved (E§8.5):
        // a tail call is never inside a `with` body (a barrier, L§8.7), so `dyn_stack`
        // is back at this frame's entry depth when the reuse happens.
    }

    /// A fresh block frame for one invocation of a `do … end` block, with `locals`
    /// holding the block's bound parameters. Its `defining`/`defining_serial` come
    /// from the [`BlockDescriptor`]; its `consumer` is who invoked it (§12). Like a
    /// callable, its work is the body `Seq` over a [`ReturnBarrier`](Cont::ReturnBarrier),
    /// which delivers the block's value to the invoker (§8.5).
    // A block frame carries every field its two call sites must supply; there is no
    // cohesive sub-group to extract, so the argument count is inherent.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn block(
        module: ModuleId,
        defining: usize,
        defining_serial: u64,
        consumer: Consumer,
        locals: Vec<Local>,
        body: NodeId,
        serial: u64,
        call_site: Option<NodeId>,
        dyn_depth: u32,
    ) -> Self {
        Frame {
            kind: FrameKind::Block {
                defining,
                defining_serial,
                consumer,
            },
            module,
            locals,
            conts: callable_conts(body),
            serial,
            block_param: None,
            tail_count: 0,
            call_site,
            dyn_depth,
        }
    }
}

/// A body's pending work: run its statements (`Seq`) over a bottom
/// [`ReturnBarrier`](Cont::ReturnBarrier) that delivers the result.
fn callable_conts(body: NodeId) -> Vec<Cont> {
    vec![
        Cont::ReturnBarrier,
        Cont::Seq {
            block: body,
            next: 0,
        },
    ]
}
