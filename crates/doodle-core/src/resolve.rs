//! The resolver (M1.10): one pass over the parsed [`Ast`] producing an immutable
//! [`ResolvedModule`] — the environment model the M2 machine consumes.
//!
//! This is the M1.10**a** slice: name resolution only. It builds the scope/frame
//! model (machine-design §7), assigns local **slots**, classifies every name
//! reference as a local slot, a block **static link**, a closure **capture**, or
//! a free **module name** (`name_refs`, AD5), and records module-level
//! declarations (`globals`). The static-error battery (M1.10b), the Void /
//! fn-falls-off-end checks (M1.10c, S-5/S-6), and tail marking (M1.11) layer on
//! top later — the [`CallableInfo`] fields they populate arrive with them.
//!
//! Design: `discussions/plan/resolver-m1.10-design.md` (and machine-design
//! §2/§6/§7). Two axes are kept distinct (conflating them is the classic
//! resolver bug): **lexical scope** (name visibility — every construct body is
//! its own scope, L§5.4) and **frame** (slot storage — only callable and block
//! bodies open a frame; a construct body runs in the enclosing frame).

mod walk;

use crate::ast::{Ast, NodeId};
use crate::diag::Diagnostic;
use crate::span::{ModuleId, Span};

/// The result of resolving a module: the resolved module and any diagnostics.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The resolved module (owns the AST arena).
    pub module: ResolvedModule,
    /// Static diagnostics from resolution (empty until the M1.10b battery).
    pub diagnostics: Vec<Diagnostic>,
    /// Reference sites that resolve to a local across an `fn` boundary — closure
    /// captures, whose resolution is deferred to M1.10c (pending the capture
    /// representation ruling). Their `resolutions` entry is `None` for now; this
    /// list makes the deferral explicit rather than silent. Empty once captures
    /// land.
    pub deferred_captures: Vec<NodeId>,
}

/// A resolved module (machine-design §2): the AST arena plus the resolved
/// environment. Immutable after resolution; the M2 machine consumes it.
///
/// The per-instance free-name cell cache (`Vec<Option<CellIdx>>` parallel to
/// [`name_refs`](Self::name_refs)) lives in the runtime instance, **not** here —
/// this stays immutable and shareable (machine-design §2, AD5).
#[derive(Clone, Debug)]
pub struct ResolvedModule {
    /// The module's canonical id (from the host resolver, E§6). `ModuleId(0)` at M1.
    pub canonical_id: ModuleId,
    /// The AST arena (owns nodes + spans); [`NodeId`] indexes it.
    pub ast: Ast,
    /// The root [`crate::ast::Node::Module`] node.
    pub root: NodeId,
    /// Statement-span index for breakpoints/stepping (E§8.6): `(span, stmt)`.
    pub stmt_spans: Vec<(Span, NodeId)>,
    /// One entry per callable/block body (`to`/`fn`/anon-`fn`/`do`), plus the
    /// module top level; index into this is a `CallableId`.
    pub callables: Vec<CallableInfo>,
    /// Module-level declarations (names bound in the module namespace / cells).
    pub globals: Vec<GlobalDecl>,
    /// Free-name reference sites, in encounter order; the per-instance cell cache
    /// is parallel to this by index.
    pub name_refs: Vec<NameRef>,
    /// Per-AST-node resolution, indexed by [`NodeId`]: `Some` at each name
    /// *reference* (`Ident`) and each local *declaration* (`Let`/`Const`/`Param`,
    /// whose binding slot lives on the decl node, not an `Ident` child).
    pub resolutions: Vec<Option<Resolution>>,
}

/// How a name reference or local declaration resolves (machine-design §6/§7).
///
/// M1.10a covers references that don't cross an `fn` boundary. Closure
/// **captures** (a nested `fn` referencing an enclosing frame's local) are
/// deferred to M1.10c together with their representation — the resolver design's
/// open A/B fork (a `Capture` variant + separate array vs. cell-boxed frame
/// slots). Until then, such a reference is left [`None`] and recorded in
/// [`Resolved::deferred_captures`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// A local in the *current* frame at `slot`.
    LocalSlot(u16),
    /// An enclosing local reached from a block body via the defining chain: chase
    /// `defining` `hops` times (0 = the block's own frame), then index
    /// `locals[slot]` (static links, machine-design §7). Blocks do not capture, so
    /// this covers block references to their enclosing frame(s) up to — but not
    /// across — an `fn` boundary.
    BlockOuter {
        /// Defining-chain hops to the owning frame (≥ 1 here).
        hops: u16,
        /// The slot in that frame.
        slot: u16,
    },
    /// A free name: resolved to a module binding cell lazily on first execution
    /// via `name_refs[name_ref]` and the per-instance cache (machine-design §6).
    ModuleName(u32),
}

/// Per callable/block body (machine-design §2 `callables`). M1.10a populates
/// every field here; `exits` (M1.10b) and tail marks (M1.11) are added by their
/// chunks — an absent field can't be misread as "computed but empty".
#[derive(Clone, Debug)]
pub struct CallableInfo {
    /// Whether this is a procedure, function, block, or the module top level.
    pub kind: BodyKind,
    /// The declaring node (`Callable`/`BlockArg` owner, or the `Module` root).
    pub decl: NodeId,
    /// The body [`crate::ast::Node::Block`].
    pub body: NodeId,
    /// Parameters, in order, each with its slot.
    pub params: Vec<ParamInfo>,
    /// The frame's `locals` length to allocate.
    pub slot_count: u16,
    /// Slot → local name, for all slots (params + body locals); the named-locals
    /// table the debugger reads (machine-design §17, E§8.2).
    pub slot_names: Vec<Box<str>>,
    /// The docstring span (L§8.6), if any.
    pub doc: Option<Span>,
    // Later chunks add: `cell_boxed`/`captures` (M1.10c, with the capture
    // representation ruling), `exits` (M1.10b, machine-design §12), and tail
    // marks (M1.11, machine-design §11). An absent field can't be misread as
    // "computed but empty".
}

/// What a callable body is (machine-design §8 `FrameKind`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BodyKind {
    /// A procedure body (`to`) — yields no value.
    Proc,
    /// A function body (`fn`, named or anonymous) — yields a value.
    Func,
    /// A `do … end` block argument body (second-class; static links, no capture).
    Block,
    /// The module top level.
    ModuleTopLevel,
}

/// A parameter of a callable (L§8.2), with its assigned slot.
#[derive(Clone, Debug)]
pub struct ParamInfo {
    /// The parameter name.
    pub name: Box<str>,
    /// Its slot in the frame.
    pub slot: u16,
    /// Whether it is the trailing `do name` block parameter (§8.2).
    pub is_block: bool,
    /// Whether it has a default (`name = expr`); the default expr is in the AST.
    pub has_default: bool,
}

/// A module-level declaration (machine-design §2 `globals`): a name, its
/// declaration category, and the declaring node.
#[derive(Clone, Debug)]
pub struct GlobalDecl {
    /// The declared name.
    pub name: Box<str>,
    /// The declaration category (drives assignability — rule 2a — and diagnostics).
    pub kind: GlobalKind,
    /// The declaring node.
    pub decl: NodeId,
}

/// A module-level declaration category. Only [`GlobalKind::Let`] is assignable;
/// every other kind is a non-assignable declaration binding (S-6 rule 2a). The
/// load step (M2a) maps this to a `CellKind` (machine-design §6): `Let`→`Let`,
/// `Parameter`→`Parameter`, everything else → `Const`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlobalKind {
    /// A mutable module binding (`let`) — the only `=`-assignable kind.
    Let,
    /// A non-reassignable binding (`const`).
    Const,
    /// A dynamic parameter (`parameter`) — `with`-rebindable, not `=`-assignable.
    Parameter,
    /// A procedure declaration (`to`).
    Proc,
    /// A function declaration (`fn`).
    Fn,
    /// A record type (`record`/`ref record`).
    Record,
    /// A protocol (`protocol`).
    Protocol,
    /// A nested module (`module`).
    Module,
}

/// A free-name reference site (machine-design §2 `name_refs`): keys the
/// per-instance cell cache. The executing [`ResolvedModule`] fixes the module, so
/// only the name and the use site are stored; provenance lives in the namespace
/// binding, fetched at lookup.
#[derive(Clone, Debug)]
pub struct NameRef {
    /// The referenced name.
    pub name: Box<str>,
    /// The reference site (for diagnostics via `ast.span(site)`).
    pub site: NodeId,
}

/// Resolves a parsed module (`ast` with root `root`) into a [`Resolved`].
///
/// `ast` must be the output of [`crate::parse::parse_program`]; `module` is the
/// canonical module id (`ModuleId(0)` at M1).
#[must_use]
pub fn resolve(ast: Ast, root: NodeId, module: ModuleId) -> Resolved {
    walk::Resolver::run(ast, root, module)
}
