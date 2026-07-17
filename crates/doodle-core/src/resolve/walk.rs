//! The single-pass resolver walk (M1.10a): builds the scope/frame model, assigns
//! slots, and classifies every name reference. See `resolve.rs` for the output
//! types and `discussions/plan/resolver-m1.10-design.md` for the model.
//!
//! Two stacks track the two axes: `frames` (slot storage — a callable or block
//! body opens one; a construct body does not) and `scopes` (name visibility —
//! every body opens one). A construct-body scope shares the enclosing frame.

use super::{
    BodyKind, CallableInfo, ExitTarget, GlobalDecl, GlobalKind, NameRef, ParamInfo, Resolution,
    Resolved, ResolvedModule,
};
use crate::ast::{Ast, Node, NodeId, Param};
use crate::diag::Diagnostic;
use crate::diag::code::DiagnosticCode;
use crate::span::{ModuleId, Span};

mod dispatch;

/// A lexical control context, for `return`/`break`/`continue` targets +
/// placement (machine-design §12). A callable is a `return` target and a
/// `break`/`continue` barrier; a loop/block is a `break`/`continue` target.
/// (`if`/`with`/`try` are transparent to exits, so they push nothing.)
enum Ctrl {
    Callable,
    Loop(NodeId),
    Block,
}

/// An open frame: a callable or block body's slot storage.
struct Frame {
    kind: FrameKind,
    /// Next slot to assign (kept `u32` while building; narrowed to `u16` per slot).
    next_slot: u32,
    /// Slot → local name, accumulated as slots are assigned.
    slot_names: Vec<Box<str>>,
}

#[derive(PartialEq, Eq)]
enum FrameKind {
    Fn,
    Block,
    Module,
}

/// An open lexical scope: `name → slot`, with the frame the slots live in.
struct Scope {
    frame: usize,
    bindings: Vec<(Box<str>, u16)>,
}

pub(super) struct Resolver<'a> {
    ast: &'a Ast,
    module: ModuleId,
    resolutions: Vec<Option<Resolution>>,
    exit_targets: Vec<Option<ExitTarget>>,
    callables: Vec<CallableInfo>,
    globals: Vec<GlobalDecl>,
    name_refs: Vec<NameRef>,
    stmt_spans: Vec<(Span, NodeId)>,
    diagnostics: Vec<Diagnostic>,
    deferred_captures: Vec<NodeId>,
    frames: Vec<Frame>,
    scopes: Vec<Scope>,
    ctrl: Vec<Ctrl>,
    /// Whether the cursor is directly at module top level (a binding here is a
    /// module `global`, not a frame slot). False inside any nested body.
    module_direct: bool,
}

impl<'a> Resolver<'a> {
    pub(super) fn run(ast: Ast, root: NodeId, module: ModuleId) -> Resolved {
        let node_count = ast.len();
        let mut r = Resolver {
            ast: &ast,
            module,
            resolutions: vec![None; node_count],
            exit_targets: vec![None; node_count],
            callables: Vec::new(),
            globals: Vec::new(),
            name_refs: Vec::new(),
            stmt_spans: Vec::new(),
            diagnostics: Vec::new(),
            deferred_captures: Vec::new(),
            frames: Vec::new(),
            scopes: Vec::new(),
            ctrl: Vec::new(),
            module_direct: true,
        };
        r.resolve_module(root);
        let Resolver {
            resolutions,
            exit_targets,
            callables,
            globals,
            name_refs,
            stmt_spans,
            diagnostics,
            deferred_captures,
            ..
        } = r;
        Resolved {
            module: ResolvedModule {
                canonical_id: module,
                ast,
                root,
                stmt_spans,
                callables,
                globals,
                name_refs,
                resolutions,
                exit_targets,
            },
            diagnostics,
            deferred_captures,
        }
    }

    /// Resolves the module root (a [`Node::Module`]): the top-level frame.
    fn resolve_module(&mut self, root: NodeId) {
        let Node::Module { stmts, doc } = self.ast.node(root) else {
            return; // a non-module root can't occur from parse_program
        };
        let doc = *doc;
        let stmts = stmts.clone();
        self.frames.push(Frame {
            kind: FrameKind::Module,
            next_slot: 0,
            slot_names: Vec::new(),
        });
        self.scopes.push(Scope {
            frame: 0,
            bindings: Vec::new(),
        });
        self.resolve_body(&stmts);
        let frame = self.frames.pop().expect("module frame");
        self.scopes.pop();
        self.callables.push(CallableInfo {
            kind: BodyKind::ModuleTopLevel,
            decl: root,
            body: root,
            params: Vec::new(),
            slot_count: slot_count(&frame),
            slot_names: frame.slot_names,
            doc,
        });
    }

    /// Resolves a statement sequence, recording each statement's span boundary.
    fn resolve_body(&mut self, stmts: &[NodeId]) {
        for &stmt in stmts {
            self.stmt_spans.push((self.ast.span(stmt), stmt));
            self.resolve(stmt);
        }
    }

    /// Resolves a callable body as a new frame: binds params to slots, then the
    /// body, then records a [`CallableInfo`].
    fn resolve_callable(
        &mut self,
        decl: NodeId,
        kind: BodyKind,
        params: &[Param],
        body: NodeId,
        doc: Option<Span>,
    ) {
        // Param defaults evaluate "in the declaration's lexical scope" (L§8.2) —
        // the *enclosing* scope — so resolve them BEFORE opening the callee frame,
        // where the params would shadow. (So a default cannot see a sibling param.)
        for p in params {
            if let Param::Ordinary {
                default: Some(d), ..
            } = p
            {
                self.resolve(*d);
            }
        }
        let saved = self.module_direct;
        self.module_direct = false;
        self.frames.push(Frame {
            kind: FrameKind::Fn,
            next_slot: 0,
            slot_names: Vec::new(),
        });
        self.scopes.push(Scope {
            frame: self.frames.len() - 1,
            bindings: Vec::new(),
        });
        self.ctrl.push(Ctrl::Callable); // a `return` target; a break/continue barrier
        let param_infos = self.bind_params(params);
        self.resolve_block_stmts(body);
        self.ctrl.pop();
        let frame = self.frames.pop().expect("callable frame");
        self.scopes.pop();
        self.module_direct = saved;
        self.callables.push(CallableInfo {
            kind,
            decl,
            body,
            params: param_infos,
            slot_count: slot_count(&frame),
            slot_names: frame.slot_names,
            doc,
        });
    }

    /// Resolves a trailing `do … end` block argument as a new (block) frame.
    fn resolve_block_arg(&mut self, block: &crate::ast::BlockArg) {
        let saved = self.module_direct;
        self.module_direct = false;
        self.frames.push(Frame {
            kind: FrameKind::Block,
            next_slot: 0,
            slot_names: Vec::new(),
        });
        self.scopes.push(Scope {
            frame: self.frames.len() - 1,
            bindings: Vec::new(),
        });
        let mut param_infos = Vec::new();
        for name in &block.params {
            let slot = self.declare_local(name);
            param_infos.push(ParamInfo {
                name: name.clone(),
                slot,
                is_block: false,
                has_default: false,
            });
        }
        self.ctrl.push(Ctrl::Block); // a break (ConsumerCall) / continue (ThisBlock) target
        self.resolve_block_stmts(block.body);
        self.ctrl.pop();
        let frame = self.frames.pop().expect("block frame");
        self.scopes.pop();
        self.module_direct = saved;
        self.callables.push(CallableInfo {
            kind: BodyKind::Block,
            decl: block.body,
            body: block.body,
            params: param_infos,
            slot_count: slot_count(&frame),
            slot_names: frame.slot_names,
            doc: None,
        });
    }

    /// Binds a callable's parameters to slots (in order). Defaults are resolved by
    /// the caller [`resolve_callable`](Self::resolve_callable) in the enclosing
    /// scope *before* the frame opens (L§8.2), so this only assigns slots.
    fn bind_params(&mut self, params: &[Param]) -> Vec<ParamInfo> {
        let mut infos = Vec::new();
        for p in params {
            match p {
                Param::Ordinary { name, default } => {
                    let slot = self.declare_local(name);
                    infos.push(ParamInfo {
                        name: name.clone(),
                        slot,
                        is_block: false,
                        has_default: default.is_some(),
                    });
                }
                Param::Block { name } => {
                    let slot = self.declare_local(name);
                    infos.push(ParamInfo {
                        name: name.clone(),
                        slot,
                        is_block: true,
                        has_default: false,
                    });
                }
            }
        }
        infos
    }

    /// Resolves a construct-body [`Node::Block`] in a fresh scope (same frame).
    fn resolve_construct_body(&mut self, body: NodeId) {
        let saved = self.push_scope();
        self.resolve_block_stmts(body);
        self.pop_scope(saved);
    }

    /// Resolves a `while`/`loop` body: a construct scope plus a [`Ctrl::Loop`]
    /// context so `break`/`continue` inside it target this `loop_node`.
    pub(super) fn resolve_loop_body(&mut self, loop_node: NodeId, body: NodeId) {
        self.ctrl.push(Ctrl::Loop(loop_node));
        self.resolve_construct_body(body);
        self.ctrl.pop();
    }

    /// Annotates a `return` with its lexical target — the nearest enclosing
    /// callable (machine-design §12) — or reports a misplaced `return`.
    pub(super) fn resolve_return(&mut self, node: NodeId, operand: Option<NodeId>) {
        if let Some(e) = operand {
            self.resolve(e);
        }
        if self.ctrl.iter().any(|c| matches!(c, Ctrl::Callable)) {
            self.set_exit(node, ExitTarget::HomeCallable);
        } else {
            self.exit_error(
                node,
                "`return` can only appear inside a procedure or function",
            );
        }
    }

    /// Annotates a `break`/`continue` with its target — the nearest enclosing loop
    /// or block (machine-design §12), not crossing a callable boundary — or reports
    /// a misplaced exit.
    pub(super) fn resolve_break_continue(
        &mut self,
        node: NodeId,
        operand: Option<NodeId>,
        is_break: bool,
    ) {
        if let Some(e) = operand {
            self.resolve(e);
        }
        // `if`/`with`/`try` don't push `ctrl`, so the innermost `ctrl` entry IS
        // the nearest enclosing control context. A callable there is a barrier
        // (break/continue can't escape it to an outer loop) → misplaced.
        let target = match self.ctrl.last() {
            Some(Ctrl::Loop(loop_node)) => Some(ExitTarget::ThisLoop(*loop_node)),
            Some(Ctrl::Block) if is_break => Some(ExitTarget::ConsumerCall),
            Some(Ctrl::Block) => Some(ExitTarget::ThisBlock),
            Some(Ctrl::Callable) | None => None,
        };
        match target {
            Some(t) => self.set_exit(node, t),
            None => {
                let kw = if is_break { "break" } else { "continue" };
                self.exit_error(
                    node,
                    &format!("`{kw}` can only appear inside a loop or a block"),
                );
            }
        }
    }

    fn set_exit(&mut self, node: NodeId, target: ExitTarget) {
        self.exit_targets[node.0 as usize] = Some(target);
    }

    fn exit_error(&mut self, node: NodeId, message: &str) {
        let span = self.ast.span(node);
        self.diagnostics.push(Diagnostic::error(
            DiagnosticCode::MisplacedExit,
            self.module,
            span,
            message,
        ));
    }

    /// Resolves the statements of a [`Node::Block`] in the current scope.
    fn resolve_block_stmts(&mut self, block: NodeId) {
        if let Node::Block(stmts) = self.ast.node(block) {
            let stmts = stmts.clone();
            self.resolve_body(&stmts);
        }
    }

    /// Opens a lexical scope in the current frame; a binding here is never a
    /// module `global`. Returns the prior `module_direct` for [`pop_scope`].
    fn push_scope(&mut self) -> bool {
        let saved = self.module_direct;
        self.module_direct = false;
        self.scopes.push(Scope {
            frame: self.frames.len() - 1,
            bindings: Vec::new(),
        });
        saved
    }

    fn pop_scope(&mut self, saved: bool) {
        self.scopes.pop();
        self.module_direct = saved;
    }

    /// Declares a binding: a module `global` when directly at module level, else a
    /// local slot in the current frame (recording a decl-site resolution).
    fn declare_binding(&mut self, decl: NodeId, name: &str, kind: GlobalKind) {
        if self.module_direct {
            self.globals.push(GlobalDecl {
                name: name.into(),
                kind,
                decl,
            });
        } else {
            let slot = self.declare_local(name);
            self.set_res(decl, Resolution::LocalSlot(slot));
        }
    }

    /// Assigns the next slot in the current frame to `name`.
    fn declare_local(&mut self, name: &str) -> u16 {
        let frame = self.frames.last_mut().expect("a frame is open");
        let slot = u16::try_from(frame.next_slot).expect("frame exceeds the u16 slot space");
        frame.next_slot += 1;
        frame.slot_names.push(name.into());
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .bindings
            .push((name.into(), slot));
        slot
    }

    /// Resolves a name *reference* at `node`: a local slot, a block static link, a
    /// deferred capture (cross-`fn`), or a free module name.
    fn resolve_ref(&mut self, node: NodeId, name: &str) {
        match self.lookup(name) {
            Some((frame, slot)) => {
                let cur = self.frames.len() - 1;
                if frame == cur {
                    self.set_res(node, Resolution::LocalSlot(slot));
                } else if frame >= self.home_fn(cur) {
                    let hops = u16::try_from(cur - frame).expect("block nesting exceeds u16");
                    self.set_res(node, Resolution::BlockOuter { hops, slot });
                } else {
                    // Crosses an `fn` boundary → a closure capture; deferred to
                    // M1.10c pending the capture-representation ruling.
                    self.deferred_captures.push(node);
                }
            }
            None => self.record_name_ref(node, name),
        }
    }

    /// Records a free-name reference (resolves to a module cell lazily at load).
    fn record_name_ref(&mut self, node: NodeId, name: &str) {
        let idx = u32::try_from(self.name_refs.len()).expect("name_refs exceeds u32");
        self.name_refs.push(NameRef {
            name: name.into(),
            site: node,
        });
        self.set_res(node, Resolution::ModuleName(idx));
    }

    /// The innermost enclosing `fn`/module frame at or below `i` (blocks belong to
    /// it). The module frame at index 0 is a backstop.
    fn home_fn(&self, mut i: usize) -> usize {
        while self.frames[i].kind == FrameKind::Block {
            i -= 1;
        }
        i
    }

    /// Looks up `name` in the scope stack (innermost first).
    fn lookup(&self, name: &str) -> Option<(usize, u16)> {
        for scope in self.scopes.iter().rev() {
            for (n, slot) in scope.bindings.iter().rev() {
                if &**n == name {
                    return Some((scope.frame, *slot));
                }
            }
        }
        None
    }

    fn set_res(&mut self, node: NodeId, res: Resolution) {
        self.resolutions[node.0 as usize] = Some(res);
    }
}

/// The frame's final slot count (checked into `u16`).
fn slot_count(frame: &Frame) -> u16 {
    u16::try_from(frame.next_slot).expect("frame exceeds the u16 slot space")
}

fn kind_to_body(kind: crate::ast::CallableKind) -> BodyKind {
    match kind {
        crate::ast::CallableKind::Proc => BodyKind::Proc,
        crate::ast::CallableKind::Func => BodyKind::Func,
    }
}
