//! The abstract syntax tree: a flat arena of [`Node`]s indexed by [`NodeId`].
//!
//! The parser (M1.6/M1.7) grows this: expression nodes carry fully lowered
//! literal values, statement nodes carry the L§7 forms, and a [`Node::Block`]
//! is a body (a statement sequence, §7.1). The full per-module `ResolvedModule`
//! the resolver produces (slot tables, capture plans, exit annotations, …) is
//! described in machine-design §2; the resolver (M1.9+) annotates names, slots,
//! and tail positions on top of this tree.

use crate::span::Span;

/// Index of a node in an [`Ast`] arena.
///
/// A `Copy` `u32` index, never a Rust reference, so machine state stays
/// index-based and snapshot-friendly (machine-design ground rule 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct NodeId(pub u32);

/// A node in the AST arena.
///
/// The front end grows this into the full grammar (language spec L§3+). The
/// expression kinds (M1.6) carry fully lowered literal values; the resolver
/// (M1.9+) annotates names, slots, and tail positions separately
/// (machine-design §2).
#[derive(Clone, Debug)]
pub enum Node {
    /// An integer literal that fits `i64` (L§3.6.1).
    IntLit(i64),
    /// An integer literal beyond `i64`: the sign-free digits in `radix`
    /// (underscores stripped), materialized to a heap bignum at run time.
    BigIntLit {
        /// The literal's radix: 2, 8, 10, or 16.
        radix: u8,
        /// The digits, underscores removed, in that radix.
        digits: Box<str>,
    },
    /// A float literal (L§3.6.2).
    FloatLit(f64),
    /// A boolean literal `true` / `false` (L§3.6.6).
    BoolLit(bool),
    /// The `nil` literal (L§3.6.6).
    NilLit,
    /// A list literal `[ … ]` (L§4.7).
    List(Vec<NodeId>),
    /// A dict literal `{ k: v, … }` (L§4.8).
    Dict(Vec<DictEntry>),
    /// A string literal (L§3.6.3/§3.6.4): decoded text runs interleaved with
    /// interpolated expressions (§6.7).
    StrLit(Vec<StrPart>),
    /// A bytes literal `b"…"` (L§3.6.5): the decoded byte sequence.
    BytesLit(Vec<u8>),
    /// An identifier reference (L§3.4); the resolver binds it later.
    Ident(Box<str>),
    /// A prefix unary operation (L§6.5).
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand expression.
        operand: NodeId,
    },
    /// An infix binary operation (L§6.5).
    Binary {
        /// The operator.
        op: BinaryOp,
        /// The left operand.
        lhs: NodeId,
        /// The right operand.
        rhs: NodeId,
    },
    /// Member access `object.name` (L§6.5 postfix `.`).
    Field {
        /// The object expression.
        object: NodeId,
        /// The field name.
        name: Box<str>,
    },
    /// Indexing `object[index]` (L§6.3).
    Index {
        /// The indexed expression.
        object: NodeId,
        /// The index/key expression.
        index: NodeId,
    },
    /// A call `callee(args)` (L§6.4). Parens are always required.
    Call {
        /// The callee expression.
        callee: NodeId,
        /// The arguments, positional before keyword.
        args: Vec<Arg>,
    },
    /// A `let` binding `let name = value` (L§5.2): a new mutable binding.
    Let {
        /// The bound name.
        name: Box<str>,
        /// The initializer expression.
        value: NodeId,
    },
    /// A `const` binding `const name = value` (L§5.2): a non-reassignable binding.
    Const {
        /// The bound name.
        name: Box<str>,
        /// The initializer expression.
        value: NodeId,
    },
    /// An assignment `target = value` (L§5.3). `target` is an lvalue: an
    /// [`Node::Ident`], [`Node::Field`], or [`Node::Index`].
    Assign {
        /// The assignment target (an lvalue).
        target: NodeId,
        /// The assigned value.
        value: NodeId,
    },
    /// A body: a sequence of statements (L§7.1), its own scope (L§5.4).
    Block(Vec<NodeId>),
    /// An `if` (L§6.8/§7.5): condition/body arms (`else if` flattened into the
    /// list) and an optional final `else` body. The statement-vs-expression
    /// distinction (whether a final `else` is required) is semantic (M1.10).
    If {
        /// The `if` / `else if` arms, in order.
        arms: Vec<IfArm>,
        /// The final `else` body, if present.
        else_body: Option<NodeId>,
    },
    /// A `while` loop `while cond do body end` (L§7.6).
    While {
        /// The loop condition (a `Bool`).
        cond: NodeId,
        /// The loop body.
        body: NodeId,
    },
    /// A `loop` `loop do body end` (L§7.7): repeats until a `break`/`return`/raise.
    Loop {
        /// The loop body.
        body: NodeId,
    },
    /// A `with` `with name = value do body end` (L§5.5): dynamic-parameter binding.
    With {
        /// The dynamic-parameter name.
        name: Box<str>,
        /// The value bound for the body's dynamic extent.
        value: NodeId,
        /// The body run under the binding.
        body: NodeId,
    },
    /// A `try` `try body rescue e handler end` (L§6.9/§12.2).
    Try {
        /// The protected body.
        body: NodeId,
        /// The name the caught value is bound to in the handler.
        rescue_name: Box<str>,
        /// The rescue handler body.
        rescue_body: NodeId,
    },
    /// A `return` (L§7.10): exits the enclosing procedure/function, optionally
    /// with a value.
    Return(Option<NodeId>),
    /// A `break` (L§7.10): exits the nearest block-consuming call, optionally
    /// with a value.
    Break(Option<NodeId>),
    /// A `continue` (L§7.10): ends the current block invocation, optionally
    /// yielding a value.
    Continue(Option<NodeId>),
    /// A `raise` (L§12.1): raises an exception; a bare `raise` re-raises.
    Raise(Option<NodeId>),
    /// An expression statement (L§7): evaluate the child expression.
    ExprStmt(NodeId),
    /// A module body: its top-level statements in source order (L§7).
    Module(Vec<NodeId>),
    /// A placeholder for a syntax error, so parsing can recover and continue.
    Error,
}

/// One arm of an `if` (L§6.8/§7.5): a condition and the body run when it holds.
#[derive(Clone, Debug)]
pub struct IfArm {
    /// The arm's condition (a `Bool`).
    pub cond: NodeId,
    /// The body run when the condition is `true`.
    pub body: NodeId,
}

/// A dict-literal entry `key: value` (L§4.8).
#[derive(Clone, Debug)]
pub struct DictEntry {
    /// The key.
    pub key: DictKey,
    /// The value expression.
    pub value: NodeId,
}

/// A dict-literal key (L§4.8): a bare word (a string key) or a computed
/// expression.
#[derive(Clone, Debug)]
pub enum DictKey {
    /// A bare-word key `name:` — the string key `"name"`.
    Bare(Box<str>),
    /// A computed key `expr:`.
    Expr(NodeId),
}

/// A call argument (L§6.4): positional, or a keyword `name: value`.
#[derive(Clone, Debug)]
pub enum Arg {
    /// A positional argument.
    Positional(NodeId),
    /// A keyword argument `name: value`.
    Keyword {
        /// The parameter name.
        name: Box<str>,
        /// The argument value.
        value: NodeId,
    },
}

/// One piece of a string literal (L§3.6.3): a decoded text run or an
/// interpolated expression.
#[derive(Clone, Debug)]
pub enum StrPart {
    /// A run of decoded literal text (escapes applied, `{{`/`}}` collapsed).
    Text(Box<str>),
    /// An interpolated expression `{ … }` (§6.7).
    Interp(NodeId),
}

/// A prefix unary operator (L§6.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    /// Unary `-` (negation).
    Neg,
    /// Unary `+` (identity).
    Pos,
    /// `not` (boolean negation).
    Not,
}

/// An infix binary operator (L§6.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `//`
    FloorDiv,
    /// `%`
    Rem,
    /// `**`
    Pow,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `is`
    Is,
    /// `and`
    And,
    /// `or`
    Or,
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
