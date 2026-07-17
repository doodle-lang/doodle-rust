//! The per-node dispatch of the resolver walk: one arm per [`Node`] kind. Split
//! from `walk/mod.rs` (which holds the scope/frame engine) purely for length;
//! this `impl` block is part of the same [`Resolver`](super::Resolver) walk.

use crate::ast::{Arg, CallableKind, DictKey, Node, NodeId, StrPart};
use crate::resolve::{GlobalKind, Resolution};

impl super::Resolver<'_> {
    /// Resolves a single node (statement or expression — resolution is identical;
    /// the value/Void distinction is M1.10c).
    pub(super) fn resolve(&mut self, node: NodeId) {
        match self.ast.node(node) {
            // Leaves: nothing to resolve.
            Node::IntLit(_)
            | Node::BigIntLit { .. }
            | Node::FloatLit(_)
            | Node::BoolLit(_)
            | Node::NilLit
            | Node::BytesLit(_)
            | Node::Error
            | Node::Exports(_)
            | Node::Import(_) => {}

            Node::Ident(name) => {
                let name = name.clone();
                self.resolve_ref(node, &name);
            }
            Node::Unary { operand, .. } => self.resolve(*operand),
            Node::Binary { lhs, rhs, .. } => {
                let (lhs, rhs) = (*lhs, *rhs);
                self.resolve(lhs);
                self.resolve(rhs);
            }
            Node::Field { object, .. } => self.resolve(*object),
            Node::Index { object, index } => {
                let (object, index) = (*object, *index);
                self.resolve(object);
                self.resolve(index);
            }
            Node::List(elems) => {
                for e in elems.clone() {
                    self.resolve(e);
                }
            }
            Node::Dict(entries) => {
                for entry in entries.clone() {
                    if let DictKey::Expr(k) = entry.key {
                        self.resolve(k);
                    }
                    self.resolve(entry.value);
                }
            }
            Node::StrLit(parts) => {
                for part in parts.clone() {
                    if let StrPart::Interp(e) = part {
                        self.resolve(e);
                    }
                }
            }
            Node::Call {
                callee,
                args,
                block,
            } => {
                let callee = *callee;
                let args = args.clone();
                let block = block.clone();
                self.resolve(callee);
                for arg in &args {
                    match arg {
                        Arg::Positional(e) => self.resolve(*e),
                        Arg::Keyword { value, .. } => self.resolve(*value),
                    }
                }
                if let Some(block) = block {
                    self.resolve_block_arg(&block);
                }
            }
            Node::ExprStmt(e) => self.resolve(*e),

            // Bindings. `const` must keep its kind (a module `const` is a
            // non-assignable global; the M1.10b const-reassignment check and the
            // M2a cell kind both read it), so the arms don't merge.
            Node::Let { name, value } => {
                let (name, value) = (name.clone(), *value);
                self.resolve(value); // RHS is in the *outer* scope (before binding)
                self.declare_binding(node, &name, GlobalKind::Let);
            }
            Node::Const { name, value } => {
                let (name, value) = (name.clone(), *value);
                self.resolve(value);
                self.declare_binding(node, &name, GlobalKind::Const);
            }
            Node::Assign { target, value } => {
                let (target, value) = (*target, *value);
                self.resolve(value);
                self.resolve(target);
            }

            // Control constructs — their bodies are construct scopes (same frame).
            Node::If { arms, else_body } => {
                let arms = arms.clone();
                let else_body = *else_body;
                for arm in arms {
                    self.resolve(arm.cond);
                    self.resolve_construct_body(arm.body);
                }
                if let Some(body) = else_body {
                    self.resolve_construct_body(body);
                }
            }
            Node::While { cond, body } => {
                let (cond, body) = (*cond, *body);
                self.resolve(cond);
                self.resolve_construct_body(body);
            }
            Node::Loop { body } => {
                let body = *body;
                self.resolve_construct_body(body);
            }
            Node::With { name, value, body } => {
                let (name, value, body) = (name.clone(), *value, *body);
                // The dynamic-parameter name references a `parameter` cell (a free
                // module name); record the reference site on the `with` node.
                self.record_name_ref(node, &name);
                self.resolve(value);
                self.resolve_construct_body(body);
            }
            Node::Try {
                body,
                rescue_name,
                rescue_body,
            } => {
                let (body, rescue_name, rescue_body) = (*body, rescue_name.clone(), *rescue_body);
                self.resolve_construct_body(body);
                // The caught value binds `rescue_name` for the handler's scope: a
                // slot in the enclosing frame, recorded on the `try` node.
                let saved = self.push_scope();
                let slot = self.declare_local(&rescue_name);
                self.set_res(node, Resolution::LocalSlot(slot));
                self.resolve_block_stmts(rescue_body);
                self.pop_scope(saved);
            }

            // Exits: resolve the optional operand.
            Node::Return(op) | Node::Break(op) | Node::Continue(op) | Node::Raise(op) => {
                if let Some(e) = *op {
                    self.resolve(e);
                }
            }

            // Callables: a named decl binds its name (global or slot); the body is
            // a new frame. Anonymous `fn` (name: None) binds nothing.
            Node::Callable {
                kind,
                name,
                params,
                body,
                doc,
            } => {
                let (kind, params, body, doc) = (*kind, params.clone(), *body, *doc);
                if let Some(name) = name {
                    let gk = match kind {
                        CallableKind::Proc => GlobalKind::Proc,
                        CallableKind::Func => GlobalKind::Fn,
                    };
                    let name = name.clone();
                    self.declare_binding(node, &name, gk);
                }
                self.resolve_callable(node, super::kind_to_body(kind), &params, body, doc);
            }

            // Module-level type/namespace declarations.
            Node::Record { name, doc, .. } => {
                let (name, doc) = (name.clone(), *doc);
                let _ = doc; // record bodies are docstring-only; no code to resolve
                self.declare_binding(node, &name, GlobalKind::Record);
            }
            Node::Protocol { name, members, .. } => {
                let (name, members) = (name.clone(), members.clone());
                self.declare_binding(node, &name, GlobalKind::Protocol);
                // Default-implementation bodies are callable frames; signatures
                // (body: None) have nothing to resolve. Member names dispatch (M5),
                // so they are not module globals.
                for m in members {
                    if let Some(body) = m.body {
                        self.resolve_callable(
                            node,
                            super::kind_to_body(m.kind),
                            &m.params,
                            body,
                            m.doc,
                        );
                    }
                }
            }
            Node::Implement { methods, .. } => {
                // Each method is a Callable; resolve its body as a frame, but it is
                // a dispatch method (M5), not a module global — so don't bind a name.
                for method in methods.clone() {
                    if let Node::Callable {
                        kind,
                        params,
                        body,
                        doc,
                        ..
                    } = self.ast.node(method)
                    {
                        let (kind, params, body, doc) = (*kind, params.clone(), *body, *doc);
                        self.resolve_callable(
                            method,
                            super::kind_to_body(kind),
                            &params,
                            body,
                            doc,
                        );
                    }
                }
            }
            Node::Parameter { name, default } => {
                let (name, default) = (name.clone(), *default);
                self.resolve(default);
                self.declare_binding(node, &name, GlobalKind::Parameter);
            }
            Node::ModuleDecl { name, .. } => {
                // A nested module has its own namespace; declare the name here, but
                // its body's resolution (a separate namespace) is deferred (M1.11+),
                // so nodes inside it stay unresolved for now.
                let name = name.clone();
                self.declare_binding(node, &name, GlobalKind::Module);
            }

            // A bare Block only reaches here defensively (bodies are entered by
            // their construct); treat it as a construct scope.
            Node::Block(_) => self.resolve_construct_body(node),

            // The module root is resolved by `resolve_module`, never via `resolve`.
            Node::Module { .. } => {}
        }
    }
}
