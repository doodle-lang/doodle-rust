//! Native modules (engine spec E§5.5, S-44): a host-exposed capability library — a module
//! value whose members are foreign functions, constants, foreign values, or records, found
//! by module resolution (§6) ahead of source lookup. A native module declares **no** dynamic
//! `parameter` cells, protocols, or implementations (D-M5-3, ratified): those are language
//! constructs, provided by a Doodle wrapper module over the native primitives.
//!
//! **Members (M5.4).** All four kinds E§5.5 allows — foreign functions, constants, foreign
//! values, and records — materialized when the instance is built, so a member value is an
//! ordinary heap value like any other.
//!
//! **Representation.** A native module is **pre-loaded** into the instance's module table at
//! creation (before the first drive, S-32): it gets a synthetic [`ResolvedModule`] whose
//! `globals` are the member names (so member access `m.x` and wildcard imports resolve
//! through the same M5.2 machinery) and a namespace of cells holding the materialized member
//! values. Its top level never runs — the members are already bound — so its AST is a bare
//! empty `Module` node. `import`ing it finds it via the load `by_path` cache and binds it
//! exactly like a Doodle module.

use super::intrinsic::Intrinsic;
use crate::ast::{Ast, Node};
use crate::heap::{CalObj, CallableTarget, CellKind, Finalizer, Heap, TypeObj};
use crate::machine::value::CellIdx;
use crate::machine::{RecordType, TypeKind, Value};
use crate::resolve::{GlobalDecl, GlobalKind, ResolvedModule};
use crate::span::{ModuleId, Span};

/// A constant a native module exports (E§5.5): an inline scalar, or a string/bytes value the
/// engine materializes onto the heap when the instance is built. (BigInt / list / dict
/// constants are uncommon as native exports and are not yet provided.)
#[derive(Clone, Debug)]
pub enum ConstValue {
    /// An integer constant (`Int`).
    Int(i64),
    /// A float constant (`Float`).
    Float(f64),
    /// A boolean constant (`Bool`).
    Bool(bool),
    /// `nil`.
    Nil,
    /// A string constant (materialized to a heap `String`; must be NFC — L§4.4).
    Str(Box<str>),
    /// A byte-string constant (materialized to a heap `Bytes`).
    Bytes(Box<[u8]>),
}

/// A member of a native module (E§5.5): the kinds a native module may export. Per D-M5-3 a
/// native module exports these and nothing else — no `parameter` cells, protocols, or
/// implementations.
pub enum NativeMember {
    /// A foreign function (E§5.1): a synchronous callback or a suspending capability, built
    /// with an engine-provided [`Intrinsic`] constructor.
    Function(Intrinsic),
    /// A constant value.
    Const(ConstValue),
    /// A foreign value (E§4.5): an opaque, reference-typed host object — a host type `tag`, an
    /// opaque `ptr` the engine never dereferences, and a `finalizer` run exactly once when the
    /// value dies (GC sweep or instance destroy). Foreign values do not dispatch protocols.
    Foreign {
        /// The host type tag.
        tag: u64,
        /// The opaque host pointer/token.
        ptr: u64,
        /// The exactly-once finalizer, or `None`.
        finalizer: Option<Finalizer>,
    },
    /// A record **type** (L§9): its field names in declaration order and whether it is a
    /// `ref record`. The member name is the record's type name — `m.Point(x: 1)` constructs
    /// and `x is m.Point` tests against it.
    Record {
        /// The field names, in declaration order.
        fields: Vec<Box<str>>,
        /// Whether instances are shared (`ref record`, L§4.14) rather than copied on binding.
        is_ref: bool,
    },
}

/// A native module (E§5.5): a name (its import path segment) and its public members in
/// registration order. Built by the host and handed to
/// [`Registry::register_module`](super::intrinsic::Registry::register_module).
pub struct NativeModule {
    pub(crate) name: Box<str>,
    pub(crate) members: Vec<(Box<str>, NativeMember)>,
}

impl NativeModule {
    /// A native module named `name` (the single-segment import path it is found under).
    pub fn new(name: impl Into<Box<str>>) -> Self {
        NativeModule {
            name: name.into(),
            members: Vec::new(),
        }
    }

    /// Adds a foreign-function member `name` (a builder step).
    #[must_use]
    pub fn function(mut self, name: impl Into<Box<str>>, function: Intrinsic) -> Self {
        self.members
            .push((name.into(), NativeMember::Function(function)));
        self
    }

    /// Adds a constant member `name` (a builder step).
    #[must_use]
    pub fn constant(mut self, name: impl Into<Box<str>>, value: ConstValue) -> Self {
        self.members.push((name.into(), NativeMember::Const(value)));
        self
    }

    /// Adds a foreign-value member `name` (E§4.5): a host `tag`/`ptr` with an exactly-once
    /// `finalizer` (a builder step).
    #[must_use]
    pub fn foreign(
        mut self,
        name: impl Into<Box<str>>,
        tag: u64,
        ptr: u64,
        finalizer: Option<Finalizer>,
    ) -> Self {
        self.members.push((
            name.into(),
            NativeMember::Foreign {
                tag,
                ptr,
                finalizer,
            },
        ));
        self
    }

    /// Adds a record-type member `name` (L§9): its `fields` in declaration order and whether
    /// it is a `ref record` (a builder step). The member name is the record's type name.
    #[must_use]
    pub fn record(
        mut self,
        name: impl Into<Box<str>>,
        fields: Vec<Box<str>>,
        is_ref: bool,
    ) -> Self {
        self.members
            .push((name.into(), NativeMember::Record { fields, is_ref }));
        self
    }
}

/// Materializes a native constant onto the heap (E§5.5): a scalar is inline; a string/bytes
/// value is allocated. String constants must already be NFC (L§4.4) — the host builds them
/// from Rust `&str`, which the engine trusts here as it does a source literal.
fn materialize_const(value: ConstValue, heap: &mut Heap) -> Value {
    match value {
        ConstValue::Int(n) => Value::Int(n),
        ConstValue::Float(x) => Value::Float(x),
        ConstValue::Bool(b) => Value::Bool(b),
        ConstValue::Nil => Value::Nil,
        ConstValue::Str(s) => Value::Str(heap.alloc_string(s)),
        ConstValue::Bytes(b) => Value::Bytes(heap.alloc_bytes(b)),
    }
}

/// The materialized form of a native module: its synthetic [`ResolvedModule`], its namespace
/// (member name → cell), and its cells (permanent GC roots, like any module's globals).
pub(crate) struct BuiltNativeModule {
    pub(crate) resolved: ResolvedModule,
    pub(crate) namespace: Vec<(Box<str>, CellIdx)>,
    pub(crate) cells: Vec<CellIdx>,
}

/// Materializes a native module into the instance at load (E§5.5, S-32): each member is
/// bound in a namespace cell — a foreign function to a `CalObj` whose intrinsic id is its
/// index in `intrinsics` (into which it is appended), a constant to its heap value. Returns
/// the synthetic module; the caller assigns it `id` in the table and records its path.
pub(crate) fn build_native_module(
    module: NativeModule,
    id: ModuleId,
    heap: &mut Heap,
    intrinsics: &mut Vec<Intrinsic>,
) -> BuiltNativeModule {
    let mut ast = Ast::new();
    let root = ast.push(
        Node::Module {
            stmts: Vec::new(),
            doc: None,
        },
        Span::DUMMY,
    );
    let mut namespace: Vec<(Box<str>, CellIdx)> = Vec::with_capacity(module.members.len());
    let mut globals: Vec<GlobalDecl> = Vec::with_capacity(module.members.len());
    for (name, member) in module.members {
        let value = match member {
            NativeMember::Function(function) => {
                intrinsics.push(function);
                let iid = (intrinsics.len() - 1) as u32;
                let cal = heap.alloc_callable(CalObj {
                    module: id,
                    target: CallableTarget::Intrinsic(iid),
                    captures: Vec::new(),
                });
                Value::Callable(cal)
            }
            NativeMember::Const(value) => materialize_const(value, heap),
            NativeMember::Foreign {
                tag,
                ptr,
                finalizer,
            } => Value::Foreign(heap.alloc_foreign(tag, ptr, finalizer)),
            NativeMember::Record { fields, is_ref } => {
                let schema = RecordType {
                    name: name.clone(),
                    fields: fields.into_boxed_slice(),
                    is_ref,
                };
                Value::Type(heap.alloc_type(TypeObj {
                    kind: TypeKind::Record(schema),
                }))
            }
        };
        let cell = heap.alloc_cell(CellKind::Const, Some(value));
        namespace.push((name.clone(), cell));
        // The member is a module-level public binding; its declaring node is the module root
        // (a native member has no real declaration, and the node is never dereferenced as one
        // — the top level does not run and members are pre-bound).
        globals.push(GlobalDecl {
            name,
            kind: GlobalKind::Const,
            decl: root,
        });
    }
    let node_count = ast.len();
    let resolved = ResolvedModule {
        canonical_id: id,
        ast,
        root,
        stmt_spans: Vec::new(),
        callables: Vec::new(),
        globals,
        // A native module's members are all public (M5.4 has no native `exports` API yet).
        exports: None,
        name_refs: Vec::new(),
        resolutions: vec![None; node_count],
        exit_targets: vec![None; node_count],
        tail_calls: vec![false; node_count],
    };
    let cells = namespace.iter().map(|(_, c)| *c).collect();
    BuiltNativeModule {
        resolved,
        namespace,
        cells,
    }
}
