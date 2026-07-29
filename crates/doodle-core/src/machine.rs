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
mod compare;
mod cont;
mod error;
mod frame;
mod step;

pub use error::{Exception, ExceptionKind, Trace};

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
    state: InstanceState,
}

impl Instance {
    /// Loads a resolved module into a fresh `Ready` instance (machine-design §18).
    /// The module top level becomes an ordinary frame whose pending work is
    /// sequencing its statements (an observable, drivable `ModuleTopLevel` frame).
    pub fn load(module: ResolvedModule) -> Self {
        debug_assert!(
            matches!(
                module.ast.node(module.root),
                crate::ast::Node::Module { .. }
            ),
            "load: a resolved module's root must be the `Module` node"
        );
        let root = module.root;
        let resolved = Arc::new(module);
        let frame = Frame::module_top_level(Cont::Seq {
            block: root,
            next: 0,
        });
        Instance {
            resolved,
            heap: Heap::new(),
            machine: Machine {
                frames: vec![frame],
                reg: None,
            },
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

    /// Performs one machine transition (machine-design §8). Precondition:
    /// `!self.is_halted()`. Returns `Err` if the transition raised a runtime
    /// error (the drive loop turns it into `Raised`).
    pub(crate) fn step(&mut self) -> Result<(), Raise> {
        step::step(&self.resolved, &mut self.heap, &mut self.machine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an instance from Doodle source through the real front end, asserting
    /// the program loads clean (no lex/parse/resolve diagnostics).
    fn load_source(src: &str) -> Instance {
        use crate::diag::Severity;
        let nfc = crate::source::normalize(src);
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
        Instance::load(resolved.module)
    }

    /// Advances until a value lands in the register or the machine halts.
    fn step_to_first_value(inst: &mut Instance) {
        let mut steps = 0;
        while inst.result().is_none() && !inst.is_halted() {
            inst.step().expect("unexpected raise");
            steps += 1;
            assert!(steps < 1000, "machine failed to produce a value");
        }
    }

    /// Advances until the register holds `Int(want)`, failing if the machine
    /// halts first (or runs away).
    fn step_until_int(inst: &mut Instance, want: i64) {
        let mut steps = 0;
        while inst.result().and_then(Value::as_int) != Some(want) {
            assert!(
                !inst.is_halted(),
                "halted before the register reached {want}"
            );
            inst.step().expect("unexpected raise");
            steps += 1;
            assert!(steps < 1000, "register never reached {want}");
        }
    }

    /// Drives to halt, returning the last value the register held before the
    /// module returned Void — i.e. the final expression's value.
    fn drive_capturing_last_value(inst: &mut Instance) -> Option<Value> {
        let mut last = None;
        let mut steps = 0;
        while !inst.is_halted() {
            if let Some(v) = inst.result() {
                last = Some(v);
            }
            inst.step().expect("unexpected raise");
            steps += 1;
            assert!(steps < 10000, "machine failed to halt");
        }
        last
    }

    #[test]
    fn value_readers_match_only_their_own_variant() {
        assert_eq!(Value::Int(7).as_int(), Some(7));
        assert_eq!(Value::Float(1.5).as_int(), None);
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert_eq!(Value::Int(0).as_bool(), None);
        // Avoid a float `==` (clippy::float_cmp); presence is enough to catch a
        // reader matching the wrong variant.
        assert!(Value::Float(2.5).as_float().is_some());
        assert!(Value::Nil.as_float().is_none());
        assert!(Value::Nil.is_nil());
        assert!(!Value::Int(0).is_nil());
    }

    #[test]
    fn a_fresh_instance_is_ready_and_not_halted() {
        let inst = load_source("1\n");
        assert_eq!(inst.state(), InstanceState::Ready);
        assert!(!inst.is_halted());
    }

    #[test]
    fn evaluates_an_int_literal_into_the_register() {
        let mut inst = load_source("42\n");
        step_to_first_value(&mut inst);
        assert_eq!(inst.result().and_then(Value::as_int), Some(42));
    }

    #[test]
    fn a_bytes_literal_allocates_on_the_heap() {
        let mut inst = load_source("b\"hi\"\n");
        step_to_first_value(&mut inst);
        assert!(matches!(inst.result(), Some(Value::Bytes(_))));
    }

    #[test]
    fn sequencing_runs_statements_in_order() {
        // Each statement's value lands in the register in turn: `1` then `2`.
        // A skip/miscount bug (e.g. advancing the sequence index by two) would
        // halt before the register ever reaches `2`, failing the second wait —
        // catching what the Void-completion tests alone cannot.
        let mut inst = load_source("1\n2\n");
        step_until_int(&mut inst, 1);
        step_until_int(&mut inst, 2);
    }

    #[test]
    fn a_module_halts_and_completes_void() {
        // Several literal statements: sequencing runs each, and the top-level
        // return discards the final value (a module yields Void, L§6.11).
        let mut inst = load_source("1\ntrue\nnil\n");
        let mut steps = 0;
        while !inst.is_halted() {
            inst.step().expect("unexpected raise");
            steps += 1;
            assert!(steps < 1000, "machine failed to halt");
        }
        assert!(inst.result().is_none());
    }

    #[test]
    fn arithmetic_evaluates_through_the_machine() {
        // Precedence + associativity flow through the continuation stack.
        let mut inst = load_source("2 * 3 + 4\n");
        assert_eq!(
            drive_capturing_last_value(&mut inst).and_then(Value::as_int),
            Some(10)
        );
    }

    #[test]
    fn integer_overflow_promotes_to_bigint_through_the_machine() {
        let mut inst = load_source("9223372036854775807 + 1\n");
        assert!(matches!(
            drive_capturing_last_value(&mut inst),
            Some(Value::BigInt(_))
        ));
    }

    #[test]
    fn comparison_and_boolean_ops_evaluate_through_the_machine() {
        for (src, expected) in [
            ("3 < 5\n", true),
            ("5 <= 5\n", true),
            ("2 != 2\n", false),
            ("1 == 1.0\n", true),
            ("true and false\n", false),
            ("false or true\n", true),
            ("not false\n", true),
        ] {
            let mut inst = load_source(src);
            let got = drive_capturing_last_value(&mut inst);
            assert!(
                matches!(got, Some(Value::Bool(b)) if b == expected),
                "{src:?} should evaluate to {expected}, got {got:?}"
            );
        }
    }
}
