//! The resumable machine: the value representation and the instance that holds
//! and drives execution state.
//!
//! [`Value`] is the `Copy` value representation (machine-design §3). [`Instance`]
//! is a running program (engine spec E§3): it owns the resolved module, the
//! [`Heap`], and the [`Machine`] state (the frame stack + result register), and
//! the drive loop ([`crate::drive`]) advances it one [`step`](Instance::step) at a
//! time.
//!
//! **Scope (M2a.2).** The CESK skeleton: frames, a continuation stack, `step`,
//! and module-top-level execution over the demo subset's literals. Operators,
//! calls, binding, control flow, PTC, safe points/limits, and GC join in later
//! M2a chunks (`plan/plan-m2a.md`); the additional `Machine` state they need
//! (ring buffer, fuel, unwind record, dynamic stack, drive stack) is added then.

mod arith;
mod block;
mod call;
mod compare;
mod cont;
mod control;
mod error;
mod frame;
mod local;
mod ring;
mod step;
mod types;
mod unwind;

pub use error::{Exception, ExceptionKind, Trace};
pub(crate) use types::BuiltinType;

use crate::heap::Heap;
use crate::resolve::ResolvedModule;
use crate::span::ModuleId;
use cont::Cont;
use error::Raise;
use frame::Frame;
use std::sync::Arc;

macro_rules! heap_index {
    ($($name:ident: $doc:literal,)+) => {
        $(
            #[doc = $doc]
            #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
            pub struct $name(pub u32);
        )+
    };
}

heap_index! {
    BigIntIdx: "Index of a heap bignum in the bigint slab (machine-design §4).",
    StrIdx: "Index of a string in the string slab (machine-design §4).",
    BytesIdx: "Index of a byte string in the bytes slab (machine-design §4).",
    ListIdx: "Index of a list in the list slab (machine-design §4).",
    DictIdx: "Index of a dict in the dict slab (machine-design §4).",
    RecIdx: "Index of a record in the record slab (machine-design §4).",
    CalIdx: "Index of a callable in the callable slab (machine-design §4).",
    TypeIdx: "Index of a type value in the type slab (machine-design §4).",
    FrnIdx: "Index of a foreign value in the foreign slab (machine-design §4).",
}

/// Index of a binding **cell** in the shared cells slab (machine-design §6/§7).
/// A cell is a machine-internal box — a module binding or (later) a closure
/// upvalue — **not** a `Value` variant, so it has no place in the `Value`-oriented
/// index macro above.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CellIdx(pub u32);

/// A Doodle value (language spec L§4) in the machine's `Copy` representation
/// (machine-design §3).
///
/// Heap-backed variants hold a `u32` slab index (machine-design §4), never a
/// Rust reference. **No `PartialEq`**: value equality is the semantic function
/// of L§4.13 (structural, cycle-safe, cross-numeric-kind), implemented
/// explicitly when the machine core lands; a derived bitwise `==` would be a
/// footgun. `Void` (the L§6.11 procedure-result sentinel) is deliberately not a
/// variant — the result register is `Option<Value>` with `None` = Void, so a
/// Void can never be stored into a data structure by construction.
#[derive(Clone, Copy, Debug)]
pub enum Value {
    /// `nil` (L§4.9).
    Nil,
    /// A boolean (L§4.1).
    Bool(bool),
    /// A machine-word integer — the small-int fast path (L§4.2).
    Int(i64),
    /// A heap bignum, for integers outside `i64` range (L§4.2).
    BigInt(BigIntIdx),
    /// A double-precision float (L§4.3).
    Float(f64),
    /// A string (L§4.4).
    Str(StrIdx),
    /// A byte string (L§4.5).
    Bytes(BytesIdx),
    /// A list (L§4.6).
    List(ListIdx),
    /// A dict (L§4.7).
    Dict(DictIdx),
    /// A record — value or reference; the heap header says which (L§4.14).
    Record(RecIdx),
    /// A callable: `to`, `fn`, or lambda (L§6).
    Callable(CalIdx),
    /// A module value (L§9).
    Module(ModuleId),
    /// A type value: built-in types, record types, and protocols (L§10, L§11).
    Type(TypeIdx),
    /// A foreign (host) value (engine spec E§4.5).
    Foreign(FrnIdx),
}

impl Value {
    /// Returns the integer if this is an `Int`, else `None`.
    pub fn as_int(self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(n),
            _ => None,
        }
    }

    /// Returns the boolean if this is a `Bool`, else `None`.
    pub fn as_bool(self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the float if this is a `Float`, else `None`.
    pub fn as_float(self) -> Option<f64> {
        match self {
            Value::Float(x) => Some(x),
            _ => None,
        }
    }

    /// Whether this value is `Nil`.
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }
}

/// The lifecycle state of an [`Instance`] (engine spec E§3.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstanceState {
    /// Loaded, not yet started, or between top-level statements.
    Ready,
    /// Inside a drive call.
    Running,
    /// Awaiting a capability resolution (E§7.5).
    Suspended,
    /// Stopped at a safe point for observation (E§7.4).
    Paused,
    /// Finished (E§7.2 `Completed`).
    Completed,
    /// Stopped by a limit, cancellation, or internal fault (E§9, §10).
    Faulted,
}

/// The core execution state (machine-design §8): the walkable frame stack and the
/// result register. The additional state pinned in §8 — ring buffer, fuel,
/// in-flight unwind, dynamic-parameter stack, drive stack — is added in the
/// chunks that first need it.
pub(crate) struct Machine {
    /// The frame stack (E§8.2); top = innermost active body. Empty once halted.
    frames: Vec<Frame>,
    /// The result register (L§6.11): `None` = Void.
    reg: Option<Value>,
    /// Monotonic frame-identity counter (machine-design §8): stamped into each
    /// pushed frame's `serial`, so a frame activation is distinguishable from a
    /// later reuse of the same stack slot (integrity for static links / consumers).
    frame_serial: u64,
    /// An in-flight non-local transfer (machine-design §12): while `Some`, `step`
    /// unwinds toward the exit's resolver-annotated target instead of running
    /// continuations normally. `None` in normal execution. A GC root once it can
    /// carry an exception value (M4).
    unwind: Option<unwind::Unwind>,
    /// Bounded history of frames elided by tail-call reuse (E§8.3, §11).
    ring: ring::RingBuffer,
}

impl Machine {
    /// The next frame serial (post-increment): a fresh, monotonic frame identity.
    pub(crate) fn next_frame_serial(&mut self) -> u64 {
        let serial = self.frame_serial;
        self.frame_serial += 1;
        serial
    }

    /// Records a tail-elided frame in the ring (machine-design §11).
    pub(crate) fn record_elided(&mut self, callable: CalIdx, consuming_serial: u64) {
        self.ring.record(ring::ElidedFrame {
            callable,
            consuming_serial,
        });
    }
}

/// A running program: the machine state the host drives (engine spec E§3).
///
/// Owns the immutable resolved module (shareable with tooling, machine-design §2),
/// the [`Heap`], the [`Machine`] state, and the lifecycle [`InstanceState`]. The
/// drive loop advances it via [`step`](Self::step); the module table for multiple
/// modules is M5, so an instance holds a single module at M1/M2a.
pub struct Instance {
    resolved: Arc<ResolvedModule>,
    heap: Heap,
    machine: Machine,
    /// The module namespace (machine-design §6/§18): each module-level name bound
    /// to its binding cell. A small ordered list (single module at M1/M2a);
    /// scanned linearly, so lookup is deterministic and hashing-free.
    namespace: Vec<(Box<str>, CellIdx)>,
    state: InstanceState,
}

impl Instance {
    /// Loads a resolved module into a fresh `Ready` instance (machine-design §18).
    /// Each module-level name gets an **uninitialized** binding cell (its
    /// `let`/`const` fills it when it executes; a read before then is a
    /// use-before-defined error). The module top level becomes an ordinary,
    /// drivable `ModuleTopLevel` frame whose pending work sequences its statements.
    pub fn load(module: ResolvedModule) -> Self {
        debug_assert!(
            matches!(
                module.ast.node(module.root),
                crate::ast::Node::Module { .. }
            ),
            "load: a resolved module's root must be the `Module` node"
        );
        let mut heap = Heap::new();
        // Module globals first (their `let`/`const`/`to`/`fn` fill their cells in
        // execution order — the temporal dead zone, M2a.4a), then the built-in
        // type-value prelude appended after, so a user global of the same name
        // wins the linear `find_cell` scan (control.rs). Each built-in cell is
        // seeded (not TDZ) with its type value — these names are always defined.
        let mut namespace: Vec<(Box<str>, CellIdx)> = module
            .globals
            .iter()
            .map(|g| (g.name.clone(), heap.alloc_cell(None)))
            .collect();
        for &(name, builtin) in types::BUILTINS {
            let ty = Value::Type(heap.alloc_type(crate::heap::TypeObj { builtin }));
            namespace.push((name.into(), heap.alloc_cell(Some(ty))));
        }
        // The module top level's construct-body locals may be cell-boxed (a `fn`
        // captured one, §7), so build its slots like any frame — no params, no
        // captures. `raw` is all-`None`; `let`s fill the slots as they execute.
        let module_id = module
            .callables
            .iter()
            .position(|c| matches!(c.kind, crate::resolve::BodyKind::ModuleTopLevel))
            .expect("a resolved module has a top-level callable");
        let raw = vec![None; module.callables[module_id].slot_count as usize];
        let locals = local::build(&module, &mut heap, module_id, &raw, &[]);
        let root = module.root;
        let resolved = Arc::new(module);
        let frame = Frame::module_top_level(
            locals,
            Cont::Seq {
                block: root,
                next: 0,
            },
            0, // the module frame is frame serial 0; further frames count up
        );
        Instance {
            resolved,
            heap,
            machine: Machine {
                frames: vec![frame],
                reg: None,
                frame_serial: 1,
                unwind: None,
                ring: ring::RingBuffer::new(),
            },
            namespace,
            state: InstanceState::Ready,
        }
    }

    /// The current lifecycle state (E§3.3).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// The result register: the last value produced, or `None` for Void
    /// (L§6.11). After a top-level drive completes this is `None` — a module runs
    /// for effect and yields Void.
    pub fn result(&self) -> Option<Value> {
        self.machine.reg
    }

    /// Sets the lifecycle state (the drive loop drives the transitions).
    pub(crate) fn set_state(&mut self, state: InstanceState) {
        self.state = state;
    }

    /// Whether the machine has halted — no frames remain to run.
    pub(crate) fn is_halted(&self) -> bool {
        self.machine.frames.is_empty()
    }

    /// The current frame-stack depth (for tail-call tests: a tail loop keeps this
    /// bounded — constant memory).
    #[cfg(test)]
    pub(crate) fn frame_depth(&self) -> usize {
        self.machine.frames.len()
    }

    /// The top frame's tail-iteration counter (E§8.3), or `None` when halted.
    #[cfg(test)]
    pub(crate) fn top_frame_tail_count(&self) -> Option<u64> {
        self.machine.frames.last().map(|f| f.tail_count)
    }

    /// Performs one machine transition (machine-design §8). Precondition:
    /// `!self.is_halted()`. Returns `Err` if the transition raised a runtime
    /// error (the drive loop turns it into `Raised`).
    pub(crate) fn step(&mut self) -> Result<(), Raise> {
        step::step(
            &self.resolved,
            &mut self.heap,
            &mut self.machine,
            &self.namespace,
        )
    }
}

#[cfg(test)]
mod tests;
