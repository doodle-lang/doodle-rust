//! The resumable machine: the value representation and the instance that holds
//! execution state.
//!
//! Shell for M0: the [`Value`] representation (machine-design §3) and an
//! [`Instance`] holding the lifecycle state and the result register. The heap
//! slabs, frames, continuation stack, and GC (machine-design §4+) are the M2a
//! machine core and are deliberately absent — the M2a gate covers those
//! *mechanisms*, not this `Value` enum, which is usable now.

use crate::ast::{Ast, Node, NodeId};
use crate::span::ModuleId;

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

/// A running program: the machine state the host drives (engine spec E§3).
///
/// Shell for M0: holds the program arena, the lifecycle [`InstanceState`], the
/// **result register** (`Option<Value>`, `None` = the L§6.11 Void), and a
/// step cursor over the top-level statements. The heap, frames, and drive
/// stack arrive with the M2a machine core.
pub struct Instance {
    program: Ast,
    state: InstanceState,
    result: Option<Value>,
    next_stmt: usize,
}

impl Instance {
    /// Creates a `Ready` instance over the given (already-built) program.
    pub fn new(program: Ast) -> Self {
        Instance {
            program,
            state: InstanceState::Ready,
            result: None,
            next_stmt: 0,
        }
    }

    /// The current lifecycle state (E§3.3).
    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// The result register: the last value produced, or `None` for Void
    /// (L§6.11). Meaningful once the instance reaches `Completed`.
    pub fn result(&self) -> Option<Value> {
        self.result
    }

    /// The program being run.
    pub(crate) fn program(&self) -> &Ast {
        &self.program
    }

    /// Advances the step cursor, returning the next top-level statement to run,
    /// or `None` when the module body is exhausted. This is a placeholder walk
    /// over the top-level statements; genuine resumability (a heap-resident
    /// continuation stack, machine-design §8/§14) arrives with the machine
    /// core. Today nothing interrupts the walk.
    pub(crate) fn next_statement(&mut self) -> Option<NodeId> {
        let root = self.program.root()?;
        let Node::Module(stmts) = self.program.node(root) else {
            return None;
        };
        let stmt = stmts.get(self.next_stmt).copied();
        if stmt.is_some() {
            self.next_stmt += 1;
        }
        stmt
    }

    /// Sets the lifecycle state.
    pub(crate) fn set_state(&mut self, state: InstanceState) {
        self.state = state;
    }

    /// Sets the result register.
    pub(crate) fn set_result(&mut self, result: Option<Value>) {
        self.result = result;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
