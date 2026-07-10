//! The abstract syntax tree: a flat arena of [`Node`]s indexed by [`NodeId`].
//!
//! Shell for M0: the arena plus the handful of node kinds needed to hand-build
//! a one-statement program. The full per-module `ResolvedModule` the front end
//! will produce (slot tables, capture plans, exit annotations, …) is described
//! in machine-design §2; there is no parser yet.

use crate::span::Span;

/// Index of a node in an [`Ast`] arena.
///
/// A `Copy` `u32` index, never a Rust reference, so machine state stays
/// index-based and snapshot-friendly (machine-design ground rule 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct NodeId(pub u32);

/// A node in the AST arena.
///
/// Shell: only the kinds exercised by the M0 hand-built program are present.
/// The front end grows this into the full grammar (language spec L§3+).
#[derive(Clone, Debug)]
pub enum Node {
    /// An integer-literal expression (L§4.2), e.g. `42`.
    IntLit(i64),
    /// An expression statement (L§7): evaluate the child expression.
    ExprStmt(NodeId),
    /// A module body: its top-level statements in source order (L§7).
    Module(Vec<NodeId>),
}

/// A flat AST arena: node `i` has span `spans[i]`, addressed by [`NodeId`].
#[derive(Clone, Debug, Default)]
pub struct Ast {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    root: Option<NodeId>,
}

impl Ast {
    /// Creates an empty arena.
    pub fn new() -> Self {
        Ast::default()
    }

    /// Interns `node` with `span`, returning its fresh [`NodeId`].
    ///
    /// Panics if the arena would exceed the `u32` [`NodeId`] index space
    /// (machine-design ground rule 2) — overflow must fail loudly, never wrap
    /// into a `NodeId` that aliases an already-interned node.
    pub fn push(&mut self, node: Node, span: Span) -> NodeId {
        let index =
            u32::try_from(self.nodes.len()).expect("AST arena exceeds the u32 NodeId index space");
        let id = NodeId(index);
        self.nodes.push(node);
        self.spans.push(span);
        id
    }

    /// Returns the node addressed by `id`.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// Returns the span of the node addressed by `id`.
    pub fn span(&self, id: NodeId) -> Span {
        self.spans[id.0 as usize]
    }

    /// The number of nodes in the arena.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the arena has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Sets the module's root node (a [`Node::Module`]).
    pub fn set_root(&mut self, root: NodeId) {
        self.root = Some(root);
    }

    /// The module's root node, if one has been set.
    pub fn root(&self) -> Option<NodeId> {
        self.root
    }
}
