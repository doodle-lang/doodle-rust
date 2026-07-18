//! The resolver (M1.10): one pass over the parsed [`Ast`] producing an immutable
//! [`ResolvedModule`] — the environment model the M2 machine consumes.
//!
//! It builds the scope/frame model (machine-design §7), assigns local **slots**,
//! classifies every name reference as a local slot, a block **static link**, a
//! closure **capture** (a cell-boxed slot, representation B), or a free **module
//! name** (`name_refs`, AD5); records module-level declarations (`globals`); runs
//! the static-error battery (M1.10b) and the Void / fn-falls-off-end checks
//! (M1.10c, S-5/S-6). Tail marking (M1.11) layers on later — the [`CallableInfo`]
//! fields it populates arrive with it.
//!
//! Design: `discussions/plan/resolver-m1.10-design.md` (and machine-design
//! §2/§6/§7). Two axes are kept distinct (conflating them is the classic
//! resolver bug): **lexical scope** (name visibility — every construct body is
//! its own scope, L§5.4) and **frame** (slot storage — only callable and block
//! bodies open a frame; a construct body runs in the enclosing frame).

mod walk;

use crate::ast::{Ast, NodeId};
use crate::diag::{Diagnostic, Severity};
use crate::span::{ModuleId, Span};

/// The result of resolving a module: the resolved module and any diagnostics.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The resolved module (owns the AST arena).
    pub module: ResolvedModule,
    /// Static diagnostics from resolution.
    pub diagnostics: Vec<Diagnostic>,
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
    /// Per-AST-node exit target, indexed by [`NodeId`]: `Some` at each
    /// `return`/`break`/`continue` node with a valid lexical target (machine-design
    /// §12). `raise` is never annotated (its handler search is dynamic); a
    /// misplaced exit is `None` and has a diagnostic instead.
    pub exit_targets: Vec<Option<ExitTarget>>,
}

/// The lexical target of a non-local exit (machine-design §12): the resolver
/// annotates `return`/`break`/`continue` so the machine performs no dynamic
/// "nearest-X" scan. `raise` is not annotated (its handler search is genuinely
/// dynamic).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitTarget {
    /// `break`/`continue` to the nearest enclosing loop, named by the
    /// `while`/`loop` node (the machine pops to the matching reloop cont).
    ThisLoop(NodeId),
    /// `continue` in a block body: end this block invocation (the block-return
    /// path to the block's consumer).
    ThisBlock,
    /// `break` in a block body: exit the block-consuming call.
    ConsumerCall,
    /// `return`: exit the enclosing callable, chasing the defining chain through
    /// any intervening blocks/constructs to the home `to`/`fn`.
    HomeCallable,
}

/// How a name reference or local declaration resolves (machine-design §6/§7).
///
/// A reference that crosses an `fn` boundary (a closure **capture**) resolves —
/// under capture representation **B** (resolver-design §8) — to a `LocalSlot`/
/// `BlockOuter` naming a **capture slot** of the referencing closure's frame; the
/// owning frame's `cell_boxed[slot]` (see [`CallableInfo::cell_boxed`]) tells the
/// machine to dereference the [`CellObj`](machine-design §7) that slot holds.
/// There is no distinct `Capture` variant: a captured cell is just a cell-boxed
/// frame slot, filled at closure creation from the closure's [`captures`]
/// ([`CallableInfo::captures`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// A local in the *current* frame at `slot`. If that frame's
    /// `cell_boxed[slot]` is set (a nested `fn` captured it, or it is a capture
    /// slot), the machine dereferences the cell; otherwise the slot holds the
    /// value directly.
    LocalSlot(u16),
    /// An enclosing local reached from a block body via the defining chain: chase
    /// `defining` `hops` times (0 = the block's own frame), then index
    /// `locals[slot]` (static links, machine-design §7). Blocks do not capture, so
    /// this covers block references to their enclosing frame(s) up to — but not
    /// across — an `fn` boundary; the target frame's `cell_boxed[slot]` still
    /// decides deref-vs-direct.
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

/// One cell a closure splices into its frame at creation (capture representation
/// **B**, resolver-design §8; machine-design §7/§10). The closure's `captures`
/// list drives creation: for each entry, read the cell named by [`from`](Self::from)
/// out of the *creating* (enclosing) environment and place it in this closure
/// frame's capture [`slot`](Self::slot). At invocation those cells are spliced
/// into the new frame's capture slots (machine-design §10).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CaptureSource {
    /// The (trailing) capture slot in *this* closure's frame the cell fills.
    pub slot: u16,
    /// Where the cell comes from in the enclosing environment at creation.
    pub from: CaptureFrom,
}

/// Where a captured cell comes from at closure creation: a **static link from the
/// creating frame** (machine-design §7). `hops = 0` is the creating frame's own
/// slot (a cell-boxed local, or a pass-through capture slot); `hops > 0` chases
/// the creating frame's `defining` chain (the closure was created inside a block).
///
/// **Totality invariant (resolver-design §8):** the chase runs through `Block`
/// frames only and never crosses a `Callable` boundary — a capture from beyond
/// the home callable is threaded through that callable's own capture slots
/// instead. The resolver `debug_assert`s this when it emits each source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CaptureFrom {
    /// Defining-chain hops from the creating frame to the frame holding the cell.
    pub hops: u16,
    /// The slot in that frame.
    pub slot: u16,
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
    /// Slot → local name, for all slots (params + body locals + capture slots);
    /// the named-locals table the debugger reads (machine-design §17, E§8.2).
    pub slot_names: Vec<Box<str>>,
    /// Per-slot: is this slot **cell-boxed** (machine-design §7)? Set when a nested
    /// `fn` captures the slot (a late promotion — already-emitted `LocalSlot`/
    /// `BlockOuter` refs stay valid, the flag drives runtime deref) and always set
    /// for a capture slot. Parallel to `slot_names` (length `slot_count`).
    pub cell_boxed: Vec<bool>,
    /// The cells this closure captures (capture representation **B**, resolver-
    /// design §8). Each is a discovery-order capture slot appended at the frame's
    /// growing end (never renumbering an already-emitted slot, but not necessarily
    /// a contiguous suffix — a body local declared after a capture ref sits above
    /// it); the machine splices each cell into its **explicit** [`CaptureSource::slot`].
    /// Empty for a `to`/module/block body (only an `fn` captures); a plain `fn`
    /// with no free enclosing-local references also has none.
    pub captures: Vec<CaptureSource>,
    /// The docstring span (L§8.6), if any.
    pub doc: Option<Span>,
    // Later chunks add: `exits` (M1.10b, machine-design §12) and tail marks
    // (M1.11, machine-design §11). An absent field can't be misread as "computed
    // but empty".
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

/// Lexes, parses, and resolves `source` (load-normalized, see
/// [`crate::source::normalize`]), returning all static diagnostics — the
/// conformance runner's `stage: full` entry, mirroring
/// [`crate::parse_to_diagnostics`].
///
/// A syntax **error** leaves the AST partial, so resolution (which would cascade
/// spurious errors over a broken tree) is skipped and the syntactic diagnostics
/// are returned; an error-free parse is resolved and the resolver's diagnostics
/// returned. Either way the result is source-ordered. (The gate keys on
/// `Severity::Error`, not on any diagnostic: a lex/parse *warning* — none exist
/// today — would not suppress resolution; when one lands, merge it here.)
#[must_use]
pub fn full_to_diagnostics(source: &str) -> Vec<Diagnostic> {
    let parsed = crate::parse::parse_program(source, ModuleId(0));
    if parsed
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error)
    {
        return parsed.diagnostics;
    }
    resolve(parsed.ast, parsed.root, ModuleId(0)).diagnostics
}
