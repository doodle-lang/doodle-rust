//! The S-6 Void consuming-site check (M1.10c): a post-pass that walks the whole
//! module and, at every value-**consuming** position, reports a consumed
//! expression that *statically* produces Void — the unified L§6.11 diagnostic.
//!
//! "Statically determinable" is deliberately a **subset**: the ruling scopes the
//! static half to a callee that resolves to a same-module `to` (a procedure).
//! That Void propagates up through an expression-position `if`/`try` to the outer
//! consumer (and grouping parens, which the parser makes transparent — there is
//! no paren node), so a `to` call reached through such branches is caught too.
//! Everything else defers to the runtime check at M2a: a call whose proc/fn
//! nature isn't lexically known (needs dispatch, M5), and a value-less *statement*
//! (`let`/`while`/…) as a value-position branch tail. The missing-`else` case (an
//! `if` with no `else` used as a value) is the **separate `if`-expr-requires-
//! `else`** piece — [`void_cause`] returns `None` for it here, so it is neither
//! misreported nor flagged until that chunk lands.
//!
//! Two positions are NOT consuming sites and so are never checked here: a **bare
//! expression statement** (§7.2 — its value is discarded) and an **`fn` body's
//! tail** (the S-5 `tailcheck` owns that consuming site, blaming the fn body).

use super::Resolver;
use crate::ast::{Arg, DictKey, Node, NodeId, Param, StrPart};
use crate::diag::code::DiagnosticCode;
use crate::resolve::{GlobalKind, Resolution};

/// Why a consumed expression statically produces Void, and where to blame it
/// (producer-site blame — the span covers the Void-producing expression).
enum VoidCause {
    /// A call to a same-module `to`: the call node to blame, and the proc's name.
    Proc(NodeId, Box<str>),
}

impl Resolver<'_> {
    /// Reports each consuming site whose expression statically produces Void (S-6).
    pub(super) fn check_void_sites(&mut self, root: NodeId) {
        self.void_walk(root);
    }

    /// Walks `node`, checking every value-consuming child and descending into all
    /// children. A consuming child is checked with [`site`](Self::site); a body or
    /// lvalue-target child is only descended (its value, if any, is not consumed
    /// *here*).
    fn void_walk(&mut self, node: NodeId) {
        match self.ast.node(node) {
            // Leaves and forms with no value-consuming children.
            Node::IntLit(_)
            | Node::BigIntLit { .. }
            | Node::FloatLit(_)
            | Node::BoolLit(_)
            | Node::NilLit
            | Node::BytesLit(_)
            | Node::Ident(_)
            | Node::Error
            | Node::Exports(_)
            | Node::Import(_)
            | Node::Record { .. } => {}

            Node::Unary { operand, .. } => {
                let operand = *operand;
                self.site(operand, "this operation needs a value");
            }
            Node::Binary { lhs, rhs, .. } => {
                let (lhs, rhs) = (*lhs, *rhs);
                self.site(lhs, "this operation needs a value");
                self.site(rhs, "this operation needs a value");
            }
            Node::Field { object, .. } => {
                let object = *object;
                self.site(object, "a field access needs a value");
            }
            Node::Index { object, index } => {
                let (object, index) = (*object, *index);
                self.site(object, "indexing needs a value");
                self.site(index, "an index needs a value");
            }
            Node::List(elems) => {
                for e in elems.clone() {
                    self.site(e, "a list item needs a value");
                }
            }
            Node::Dict(entries) => {
                for entry in entries.clone() {
                    if let DictKey::Expr(k) = entry.key {
                        self.site(k, "a dict key needs a value");
                    }
                    self.site(entry.value, "a dict value needs a value");
                }
            }
            Node::StrLit(parts) => {
                for part in parts.clone() {
                    if let StrPart::Interp(e) = part {
                        self.site(e, "an interpolation needs a value");
                    }
                }
            }
            Node::Call {
                callee,
                args,
                block,
            } => {
                let (callee, args, block) = (*callee, args.clone(), block.clone());
                self.site(callee, "the thing being called needs a value");
                for arg in &args {
                    match arg {
                        Arg::Positional(e) => self.site(*e, "an argument needs a value"),
                        Arg::Keyword { value, .. } => {
                            self.site(*value, "an argument needs a value")
                        }
                    }
                }
                if let Some(block) = block {
                    self.void_walk(block.body);
                }
            }

            // A bare expression statement is the one position that does NOT consume
            // (§7.2): descend for nested sites, but do not check its own value.
            Node::ExprStmt(e) => {
                let e = *e;
                self.void_walk(e);
            }

            Node::Let { value, .. } => {
                let value = *value;
                self.site(value, "a `let` needs a value");
            }
            Node::Const { value, .. } => {
                let value = *value;
                self.site(value, "a `const` needs a value");
            }
            Node::Assign { target, value } => {
                // The RHS is consumed; the target is an lvalue (a `.field`/`[i]`
                // base of it IS consumed, handled when we descend into it).
                let (target, value) = (*target, *value);
                self.site(value, "an assignment needs a value");
                self.void_walk(target);
            }
            Node::Block(stmts) => {
                for s in stmts.clone() {
                    self.void_walk(s);
                }
            }
            Node::If { arms, else_body } => {
                let (arms, else_body) = (arms.clone(), *else_body);
                for arm in arms {
                    self.site(arm.cond, "an `if` condition needs a value");
                    self.void_walk(arm.body);
                }
                if let Some(body) = else_body {
                    self.void_walk(body);
                }
            }
            Node::While { cond, body } => {
                let (cond, body) = (*cond, *body);
                self.site(cond, "a `while` condition needs a value");
                self.void_walk(body);
            }
            Node::Loop { body } => {
                let body = *body;
                self.void_walk(body);
            }
            Node::With { value, body, .. } => {
                let (value, body) = (*value, *body);
                self.site(value, "a `with` needs a value");
                self.void_walk(body);
            }
            Node::Try {
                body, rescue_body, ..
            } => {
                let (body, rescue_body) = (*body, *rescue_body);
                self.void_walk(body);
                self.void_walk(rescue_body);
            }
            Node::Return(op) => {
                if let Some(e) = *op {
                    self.site(e, "`return` needs a value");
                }
            }
            Node::Break(op) => {
                if let Some(e) = *op {
                    self.site(e, "`break` needs a value");
                }
            }
            Node::Continue(op) => {
                if let Some(e) = *op {
                    self.site(e, "`continue` needs a value");
                }
            }
            Node::Raise(op) => {
                if let Some(e) = *op {
                    self.site(e, "`raise` needs a value");
                }
            }
            Node::Callable { params, body, .. } => {
                let (params, body) = (params.clone(), *body);
                self.check_param_defaults(&params);
                self.void_walk(body);
            }
            Node::Protocol { members, .. } => {
                // Only default-implementation bodies (and their param defaults) are
                // resolved (dispatch.rs), so only those are checked; a required
                // member's signature has nothing resolved to check.
                for m in members.clone() {
                    if let Some(body) = m.body {
                        self.check_param_defaults(&m.params);
                        self.void_walk(body);
                    }
                }
            }
            Node::Implement { methods, .. } => {
                for method in methods.clone() {
                    self.void_walk(method);
                }
            }
            Node::Parameter { default, .. } => {
                let default = *default;
                self.site(default, "a `parameter` default needs a value");
            }

            // A nested module's body is a separate namespace, not resolved yet
            // (M1.11+), so its nodes carry no resolutions to check.
            Node::ModuleDecl { .. } => {}

            Node::Module { stmts, .. } => {
                for s in stmts.clone() {
                    self.void_walk(s);
                }
            }
        }
    }

    /// Checks each parameter default (a consuming site, L§8.2), then leaves the
    /// caller to walk the body.
    fn check_param_defaults(&mut self, params: &[Param]) {
        for p in params {
            if let Param::Ordinary {
                default: Some(d), ..
            } = p
            {
                self.site(*d, "a default needs a value");
            }
        }
    }

    /// A consuming site: check `e`'s value, then descend into `e` for nested sites.
    fn site(&mut self, e: NodeId, subject: &str) {
        self.consume(e, subject);
        self.void_walk(e);
    }

    /// Reports `e` if it statically produces Void, framed by `subject` (the
    /// consuming context) with producer-site blame.
    fn consume(&mut self, e: NodeId, subject: &str) {
        if let Some(VoidCause::Proc(call, name)) = self.void_cause(e) {
            let msg = format!(
                "{subject}, but `{name}` is a procedure (a `to`) and produces none \
                 — call it as its own statement, or make `{name}` an `fn`"
            );
            self.error(DiagnosticCode::ProcedureInExpression, call, &msg);
        }
    }

    /// The static-subset reason `e` produces Void, or `None` if it produces a value
    /// or its Void-ness is not statically determinable (→ runtime, M2a). Void
    /// propagates through an expression-position `if`/`try` to the branch that
    /// produces it, so this recurses into branch tails.
    fn void_cause(&self, e: NodeId) -> Option<VoidCause> {
        match self.ast.node(e) {
            Node::Call { callee, .. } => self
                .proc_callee_name(*callee)
                .map(|name| VoidCause::Proc(e, name)),
            // Void flows out of a value-producing `if` from whichever branch is
            // Void. A missing `else` is the separate `if`-expr-requires-`else`
            // piece (a later chunk), so it is not a cause here.
            Node::If { arms, else_body } => {
                let else_body = (*else_body)?;
                for arm in arms {
                    if let Some(c) = self.block_void_cause(arm.body) {
                        return Some(c);
                    }
                }
                self.block_void_cause(else_body)
            }
            Node::Try {
                body, rescue_body, ..
            } => self
                .block_void_cause(*body)
                .or_else(|| self.block_void_cause(*rescue_body)),
            _ => None,
        }
    }

    /// The Void cause of a block's *value* — its last statement's tail expression
    /// (mirroring the S-5 tail notion, but only the static-subset causes).
    fn block_void_cause(&self, block: NodeId) -> Option<VoidCause> {
        let stmt = self.last_stmt(block)?;
        // A tail `if`/`try` statement carries the block's value; a bare expression
        // statement's expression is that value. Any other tail (`let`, `while`, …)
        // is value-less but not a static-subset cause → runtime.
        let tail = match self.ast.node(stmt) {
            Node::ExprStmt(inner) => *inner,
            _ => stmt,
        };
        self.void_cause(tail)
    }

    /// The name of `callee` if it resolves to a same-module `to` (a procedure);
    /// `None` otherwise — an `fn`, a local/param/import, or an unknown name, whose
    /// Void-ness (if any) is not statically determinable here.
    fn proc_callee_name(&self, callee: NodeId) -> Option<Box<str>> {
        if !matches!(self.ast.node(callee), Node::Ident(_)) {
            return None;
        }
        let Some(Resolution::ModuleName(idx)) = self.resolutions[callee.0 as usize] else {
            return None;
        };
        let name = self.name_refs[idx as usize].name.clone();
        let is_proc = self
            .globals
            .iter()
            .any(|g| g.name == name && g.kind == GlobalKind::Proc);
        is_proc.then_some(name)
    }
}
