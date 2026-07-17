//! The S-6/L§6.8 value-discipline check (M1.10c): a post-pass that walks the
//! whole module and, at every value-**consuming** position, reports a consumed
//! expression that *statically* produces Void. Three producer causes, each with
//! its own diagnostic (producer-site blame, framed by the consuming context):
//! a call to a module-level `to` (`procedure-in-expression`, L§6.11); an `if`
//! used as a value with no `else` (`if-expression-missing-else`, L§6.8); and a
//! present branch/body of a value-position `if`/`try` whose tail produces no
//! value (`non-producing-branch`, L§6.8/§6.9).
//!
//! "Statically determinable" is a normative **subset** (S-6 ratified 2026-07-17):
//! a Void-producing call is caught only when the callee resolves to a
//! **module-level** `to` — a locally-declared `to` is indeterminate → the runtime
//! check (M2a). Void propagates up through an expression-position `if`/`try` to
//! the outer consumer (and grouping parens, which the parser makes transparent —
//! there is no paren node), so a cause reached through such branches is caught
//! too. Branch-tail Void-ness follows the S-5 lattice `tailcheck` uses, with one
//! consuming-site refinement: a tail that transfers control away — `raise`, a
//! **non-local `return`/`break`/`continue`**, or an infinite `loop` — diverges
//! past the consumer, so it is not Void. (`tailcheck` classifies a bare `return`
//! as value-less because at an *fn tail* it means "the fn yields no value"; at a
//! consuming site inside the fn, the same `return` instead leaves before the
//! consumer runs. The S-5/S-6 spec text should carry this one-line note.)
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
    /// A call to a module-level `to`: the call node to blame, and the proc's name.
    Proc(NodeId, Box<str>),
    /// An `if` used as a value with no `else` (L§6.8): the `if` node to blame.
    MissingElse(NodeId),
    /// A branch/body of a value-position `if`/`try` whose tail produces no value
    /// (L§6.8/§6.9): the value-less tail statement to blame.
    NonProducing(NodeId),
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
        match self.void_cause(e) {
            Some(VoidCause::Proc(call, name)) => {
                let msg = format!(
                    "{subject}, but `{name}` is a procedure (a `to`) and produces none \
                     — call it as its own statement, or make `{name}` an `fn`"
                );
                self.error(DiagnosticCode::ProcedureInExpression, call, &msg);
            }
            Some(VoidCause::MissingElse(if_node)) => {
                let msg = format!(
                    "{subject}, but an `if` used as a value needs an `else` — every \
                     branch must produce a value; add an `else` branch"
                );
                self.error(DiagnosticCode::IfExpressionMissingElse, if_node, &msg);
            }
            Some(VoidCause::NonProducing(stmt)) => {
                let msg = format!(
                    "{subject}, but this branch produces no value — a branch used as a \
                     value must end in an expression that produces one"
                );
                self.error(DiagnosticCode::NonProducingBranch, stmt, &msg);
            }
            None => {}
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
            // An `if` as a value with no `else` is always an error (L§6.8); with an
            // `else`, the Void flows out of whichever branch produces none.
            Node::If { arms, else_body } => {
                let Some(else_body) = *else_body else {
                    return Some(VoidCause::MissingElse(e));
                };
                for arm in arms {
                    if let Some(c) = self.block_void_cause(arm.body) {
                        return Some(c);
                    }
                }
                self.block_void_cause(else_body)
            }
            // A `try` as a value: either body producing none makes it Void (L§6.9).
            Node::Try {
                body, rescue_body, ..
            } => self
                .block_void_cause(*body)
                .or_else(|| self.block_void_cause(*rescue_body)),
            _ => None,
        }
    }

    /// The Void cause of a block's *value* — its last statement classified by the
    /// same S-5 lattice `tailcheck` uses: a diverging tail (`raise`/`return`/an
    /// infinite `loop`) never yields Void; a value-less tail does. A tail `if`/`try`
    /// or bare expression carries the block's value, so recurse into it.
    fn block_void_cause(&self, block: NodeId) -> Option<VoidCause> {
        let Some(stmt) = self.last_stmt(block) else {
            // An empty block produces no value (matching `tailcheck`'s
            // `tail_of_block`); blame the block itself.
            return Some(VoidCause::NonProducing(block));
        };
        match self.ast.node(stmt) {
            Node::ExprStmt(inner) => self.void_cause(*inner),
            // Defensive: the parser wraps a statement-position `if`/`try` in an
            // `ExprStmt` (the arm above), so this is not normally reached.
            Node::If { .. } | Node::Try { .. } => self.void_cause(stmt),
            // A non-local exit transfers control away, so the consumer never
            // receives this block's value — it diverges, no Void is consumed. (At an
            // *fn tail* a bare `return` instead means "the fn yields no value", which
            // is `tailcheck`'s concern; here the consuming site is never reached.)
            Node::Raise(_) | Node::Return(_) | Node::Break(_) | Node::Continue(_) => None,
            // A `loop` with no `break` bound to it is infinite (diverges); one that
            // can `break` completes with no value.
            Node::Loop { .. } => self
                .loops_with_break
                .contains(&stmt)
                .then_some(VoidCause::NonProducing(stmt)),
            // A value-less statement tail (`let`/`const`/assignment/`while`/`with`)
            // or any declaration in tail position: the branch produces no value.
            _ => Some(VoidCause::NonProducing(stmt)),
        }
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
