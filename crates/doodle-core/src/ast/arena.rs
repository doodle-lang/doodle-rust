//! The flat AST arena ([`Ast`]): the node + span store [`NodeId`] indexes, plus the
//! source line index the line-based breakpoint API (E§8.6) maps through. Split from the
//! parent `ast` module (the [`Node`] enum and its supporting types) so each file stays
//! within the hygiene length limit.

use super::{Node, NodeId};
use crate::span::Span;

/// A flat AST arena: node `i` has span `spans[i]`, addressed by [`NodeId`].
#[derive(Clone, Default)]
pub struct Ast {
    nodes: Vec<Node>,
    spans: Vec<Span>,
    root: Option<NodeId>,
    /// Byte offset at which each source line begins, `line_starts[0] == 0` (line 1). The
    /// engine holds byte spans, not text (E§8.1); this compact inverse — just the newline
    /// positions — is what the line-based breakpoint API (E§8.6) needs to map a line to a
    /// span and back. Set once by the parser from the (NFC-normalized) source; empty for a
    /// source-less AST (a native module, which has no line-based breakpoints).
    line_starts: Vec<u32>,
}

/// `line_starts` is a derived source index (the line↔byte map for breakpoints, E§8.6), not
/// AST structure, so it is **omitted** from `Debug` — the golden AST snapshots capture only
/// nodes, spans, and root, and stay independent of where newlines fall.
impl std::fmt::Debug for Ast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ast")
            .field("nodes", &self.nodes)
            .field("spans", &self.spans)
            .field("root", &self.root)
            .finish()
    }
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

    /// Records the line-start byte offsets of `source` for line↔byte mapping (E§8.6): the
    /// byte offset where each line begins (`line_starts[0] == 0`). Called once by the parser
    /// with the module's (NFC-normalized) source, so the offsets are into the same bytes the
    /// spans index. Newlines are ASCII `\n`, unaffected by NFC, so line boundaries agree with
    /// the source the host loaded.
    pub fn set_line_starts(&mut self, source: &str) {
        let mut starts = vec![0u32];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                // The next line begins one byte past the newline. `i` fits `u32`: spans are
                // `u32` byte offsets (ground rule 2), so a module's source is `u32`-bounded.
                starts.push(i as u32 + 1);
            }
        }
        self.line_starts = starts;
    }

    /// The 1-based line number byte `offset` falls on (E§8.6), via the line-start table, or
    /// `0` for a source-less AST (no table recorded). The line is the number of line-starts
    /// at or before `offset` — 1-based because `line_starts[0] == 0` counts line 1.
    pub fn line_of(&self, offset: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= offset) as u32
    }
}
