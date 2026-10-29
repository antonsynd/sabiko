//! AST to MIR lowering.
//!
//! One [`Body`] is built per function whose type check succeeded. The structure follows
//! rustc's `mir_build`: an expression is lowered in one of three ways depending on what
//! the context needs from it.
//!
//! - [`Builder::as_operand`] produces an [`Operand`]: a constant, or a `copy`/`move` of a
//!   place, introducing a temporary for anything else.
//! - [`Builder::as_place`] produces a [`Place`] for place expressions (locals, field
//!   accesses with their autoderefs, `*e`); any other expression is evaluated into a
//!   temporary first.
//! - [`Builder::expr_into_dest`] writes the expression's value into a destination place.
//!   Control flow lives here: `if`, loops, `&&`/`||` and blocks all branch and finally
//!   write their value into the destination.
//!
//! Storage markers follow rustc too: a `StorageLive` when a user local or temporary comes
//! into existence, `StorageDead` at the end of its scope (the enclosing block for `let`
//! bindings, the enclosing statement for temporaries) in reverse declaration order, and
//! on every early exit (`break`, `continue`, `return`) for all scopes being left.
//!
//! Lowering itself never reports diagnostics: the type checker already rejected every
//! invalid program, and bodies with type errors are skipped.

use std::collections::HashMap;

use sabiko_ast::{
    BinOp, Block, Expr, ExprKind, FnDecl, Item, ItemKind, Lit, NodeId, Pat, PatKind, SourceFile,
    Stmt, StmtKind,
};
use sabiko_diagnostics::DiagnosticSink;
use sabiko_resolve::{Builtin, DefId, DefKind, LocalId, PrimTy, Res, ResolveResults};
use sabiko_span::Span;
use sabiko_types::{BodyTypes, ConstValue, Mutability, Ty, TypeckResults};

use crate::{
    AggregateKind, BasicBlock, BasicBlockData, Body, Constant, Local, LocalDecl, LocalKind,
    Operand, Place, Rvalue, Statement, StatementKind, Terminator, TerminatorKind,
};

/// Lowers every function in `file` whose body type-checked without errors, in source
/// order. Constants have no bodies. A function that refers to a constant whose evaluation
/// failed is skipped as well: its errors were reported with the constant.
///
/// `_sink` is unused for now: lowering never reports; the validator does.
#[must_use]
pub fn build_bodies(
    file: &SourceFile,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    _sink: &mut DiagnosticSink,
) -> Vec<Body> {
    let mut bodies = Vec::new();
    for item in &file.items {
        let ItemKind::Fn(f) = &item.kind else {
            continue;
        };
        let def = resolve.def_of_item(item.id);
        let types = typeck.body(def);
        if types.has_errors {
            continue;
        }
        if let Some(body) = Builder::new(resolve, typeck, types, def, item, f).build(f) {
            bodies.push(body);
        }
    }
    bodies
}

/// A loop being lowered: where `continue` and `break` go, and where `break value`
/// writes.
struct LoopScope {
    header: BasicBlock,
    /// Created on the first `break`; a loop nobody breaks out of has no exit.
    exit: Option<BasicBlock>,
    /// `None` when the loop's value is discarded.
    dest: Option<Place>,
    /// `scopes.len()` when the loop was entered; `break`/`continue` leave every scope
    /// above it.
    scope_depth: usize,
}

struct Builder<'a> {
    resolve: &'a ResolveResults,
    typeck: &'a TypeckResults,
    types: &'a BodyTypes,
    body: Body,
    /// Resolver bindings to MIR locals.
    local_map: HashMap<LocalId, Local>,
    /// Locals to `StorageDead` when each open scope ends, innermost last. Blocks and
    /// statements (for their temporaries) each open one.
    scopes: Vec<Vec<Local>>,
    loops: Vec<LoopScope>,
    /// The block holding the single `return`, created on first use.
    return_block: Option<BasicBlock>,
    /// The block statements are currently appended to.
    current: BasicBlock,
    /// Set when a constant without a value was referenced; the body is discarded.
    failed: bool,
    /// The span of the outermost parentheses around an expression, for the expressions
    /// that have them (see [`Builder::span_of`]).
    paren_spans: HashMap<NodeId, Span>,
}

impl<'a> Builder<'a> {
    fn new(
        resolve: &'a ResolveResults,
        typeck: &'a TypeckResults,
        types: &'a BodyTypes,
        def: DefId,
        item: &Item,
        f: &FnDecl,
    ) -> Self {
        let sig = typeck.items.fn_sig(def);
        let mut locals = vec![LocalDecl {
            ty: sig.output.clone(),
            mutability: Mutability::Mut,
            kind: LocalKind::Return,
            name: None,
            span: f.ret.as_ref().map_or(f.name.span, |t| t.span),
            source: None,
        }];
        let mut local_map = HashMap::new();
        for (param, ty) in f.params.iter().zip(&sig.inputs) {
            let (name, mutability, source) = match &param.pat.kind {
                PatKind::Ident { mutable, name } => (
                    Some(name.name.clone()),
                    Mutability::from_bool(*mutable),
                    resolve.local_of_pat(param.pat.id),
                ),
                // A tuple pattern destructures the argument at the start of the body.
                PatKind::Wild | PatKind::Tuple(_) => (None, Mutability::Not, None),
            };
            let local = Local(u32::try_from(locals.len()).expect("fewer than 2^32 locals"));
            if let Some(id) = source {
                local_map.insert(id, local);
            }
            locals.push(LocalDecl {
                ty: ty.clone(),
                mutability,
                kind: LocalKind::Arg,
                name,
                span: param.span,
                source,
            });
        }
        let body = Body {
            def,
            name: f.name.name.clone(),
            locals,
            arg_count: f.params.len(),
            blocks: vec![BasicBlockData::default()],
            span: item.span,
        };
        Self {
            resolve,
            typeck,
            types,
            body,
            local_map,
            scopes: Vec::new(),
            loops: Vec::new(),
            return_block: None,
            current: BasicBlock::START,
            failed: false,
            paren_spans: HashMap::new(),
        }
    }

    fn build(mut self, f: &FnDecl) -> Option<Body> {
        let span = f.body.span;
        // Bindings of tuple-pattern parameters live for the whole body.
        self.push_scope();
        self.push_scope();
        for (i, param) in f.params.iter().enumerate() {
            if let PatKind::Tuple(_) = param.pat.kind {
                let arg = Local(u32::try_from(i + 1).expect("fewer than 2^32 locals"));
                self.bind_pattern(&param.pat, Place::local(arg));
            }
        }
        self.pop_scope(param_span(f));
        self.lower_block_into(&Place::local(Local::RETURN), &f.body);
        self.pop_scope(span);
        let ret = self.return_block();
        self.terminate(TerminatorKind::Goto { target: ret }, span);
        self.current = ret;
        self.terminate(TerminatorKind::Return, span);
        debug_assert!(self.scopes.is_empty() && self.loops.is_empty());
        if self.failed {
            None
        } else {
            Some(self.body)
        }
    }

    // ----- CFG plumbing -----

    fn new_block(&mut self) -> BasicBlock {
        let bb = BasicBlock(u32::try_from(self.body.blocks.len()).expect("fewer than 2^32 blocks"));
        self.body.blocks.push(BasicBlockData::default());
        bb
    }

    fn return_block(&mut self) -> BasicBlock {
        if let Some(bb) = self.return_block {
            return bb;
        }
        let bb = self.new_block();
        self.return_block = Some(bb);
        bb
    }

    fn push_stmt(&mut self, kind: StatementKind, span: Span) {
        self.body.blocks[self.current.index()]
            .statements
            .push(Statement { kind, span });
    }

    fn assign(&mut self, place: Place, rvalue: Rvalue, span: Span) {
        self.push_stmt(StatementKind::Assign(place, rvalue), span);
    }

    /// Ends `bb` with `kind`. Every block is terminated exactly once.
    fn terminate_block(&mut self, bb: BasicBlock, kind: TerminatorKind, span: Span) {
        let data = &mut self.body.blocks[bb.index()];
        debug_assert!(data.terminator.is_none(), "bb{} terminated twice", bb.0);
        data.terminator = Some(Terminator { kind, span });
    }

    /// Ends the current block with `kind`.
    fn terminate(&mut self, kind: TerminatorKind, span: Span) {
        let bb = self.current;
        self.terminate_block(bb, kind, span);
    }

    /// Ends the current block with a `switchInt` on `discr`. A discriminant of type `!`
    /// (`if loop {} {}`, `if return {}`) never produces a value: the block is unreachable
    /// instead, and the branches lowered after it are dead code `simplify` removes.
    fn switch_int(
        &mut self,
        discr: Operand,
        targets: Vec<(u128, BasicBlock)>,
        otherwise: BasicBlock,
        span: Span,
    ) {
        if discr.ty(&self.body).is_never() {
            self.terminate(TerminatorKind::Unreachable, span);
        } else {
            self.terminate(
                TerminatorKind::SwitchInt {
                    discr,
                    targets,
                    otherwise,
                },
                span,
            );
        }
    }

    /// Ends the current block with a jump and continues in a fresh block, which is only
    /// reachable if something else jumps to it. Code after `return`, `break` and
    /// `continue` lands in such a block; `simplify` removes it.
    fn jump_away(&mut self, kind: TerminatorKind, span: Span) {
        self.terminate(kind, span);
        self.current = self.new_block();
    }

    // ----- locals and scopes -----

    fn new_local(&mut self, decl: LocalDecl) -> Local {
        let local = Local(u32::try_from(self.body.locals.len()).expect("fewer than 2^32 locals"));
        self.body.locals.push(decl);
        local
    }

    /// Declares a temporary in the enclosing block's scope (not the statement's), so it
    /// outlives the statement that creates it.
    fn new_temp_in_block_scope(&mut self, ty: Ty, span: Span) -> Local {
        let local = self.new_local(LocalDecl {
            ty,
            mutability: Mutability::Mut,
            kind: LocalKind::Temp,
            name: None,
            span,
            source: None,
        });
        let block_scope = self.scopes.len() - 2;
        self.scopes[block_scope].push(local);
        self.push_stmt(StatementKind::StorageLive(local), span);
        local
    }

    /// Declares a temporary in the innermost scope and marks it live.
    fn new_temp(&mut self, ty: Ty, span: Span) -> Local {
        let local = self.new_local(LocalDecl {
            ty,
            mutability: Mutability::Mut,
            kind: LocalKind::Temp,
            name: None,
            span,
            source: None,
        });
        self.scopes
            .last_mut()
            .expect("temporaries are created inside a scope")
            .push(local);
        self.push_stmt(StatementKind::StorageLive(local), span);
        local
    }

    /// Declares the local of a `let` binding in the enclosing block scope (the innermost
    /// scope is the statement's own) and marks it live.
    fn declare_user_local(&mut self, id: LocalId, span: Span) -> Local {
        let info = self.resolve.local(id);
        let local = self.new_local(LocalDecl {
            ty: self.types.local_ty(id).clone(),
            mutability: Mutability::from_bool(info.mutable),
            kind: LocalKind::User,
            name: Some(info.name.clone()),
            span: info.span,
            source: Some(id),
        });
        self.local_map.insert(id, local);
        let block_scope = self.scopes.len() - 2;
        self.scopes[block_scope].push(local);
        self.push_stmt(StatementKind::StorageLive(local), span);
        local
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    /// Closes the innermost scope, ending the storage of its locals in reverse order.
    fn pop_scope(&mut self, span: Span) {
        let locals = self.scopes.pop().expect("a scope is open");
        self.storage_dead(&locals, span);
    }

    fn storage_dead(&mut self, locals: &[Local], span: Span) {
        for &local in locals.iter().rev() {
            self.push_stmt(StatementKind::StorageDead(local), span);
        }
    }

    /// Ends the storage of every local in the scopes at `depth` and above, innermost
    /// first, without closing them: the code that follows an early exit is unreachable
    /// but is still lowered in the scopes it is written in.
    fn exit_scopes(&mut self, depth: usize, span: Span) {
        let locals: Vec<Local> = self.scopes[depth..]
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev().copied())
            .collect();
        for local in locals {
            self.push_stmt(StatementKind::StorageDead(local), span);
        }
    }

    // ----- helpers -----

    fn ty(&self, expr: &Expr) -> &Ty {
        self.types.expr_ty(expr.id)
    }

    fn mir_local(&self, id: LocalId) -> Local {
        self.local_map[&id]
    }

    /// `copy` for `Copy` types, `move` otherwise.
    fn place_operand(&self, place: Place) -> Operand {
        if self.typeck.items.is_copy(&place.ty(&self.body)) {
            Operand::Copy(place)
        } else {
            Operand::Move(place)
        }
    }

    /// An operand for a coercion site: a call argument, struct field initialiser, assignment
    /// right-hand side, annotated `let` initialiser or return value, whose expected type is
    /// `target` (`None` when there is none, as for `Print`'s argument). Like rustc, a
    /// `&mut T` place is reborrowed instead of moved there, so passing a mutable reference
    /// to a function does not consume it: `&mut *p`, or `&*p` when `target` is `&T` (the
    /// `&mut T` to `&T` coercion), which leaves other shared borrows of `*p` usable.
    fn coerced_operand(&mut self, expr: &Expr, target: Option<&Ty>) -> Operand {
        let is_place_expr = matches!(
            strip_parens(expr).kind,
            ExprKind::Path(_) | ExprKind::Field(..) | ExprKind::Deref(_)
        );
        if is_place_expr {
            if let Ty::Ref(Mutability::Mut, referent) = self.ty(expr) {
                let referent = (**referent).clone();
                let mutability = match target {
                    Some(Ty::Ref(Mutability::Not, _)) => Mutability::Not,
                    _ => Mutability::Mut,
                };
                let place = self.as_place(expr);
                if place.local != Local::RETURN || !place.projection.is_empty() {
                    let temp = self.new_temp(Ty::Ref(mutability, Box::new(referent)), expr.span);
                    self.assign(
                        Place::local(temp),
                        Rvalue::Ref(mutability, place.deref()),
                        expr.span,
                    );
                    return self.place_operand(Place::local(temp));
                }
            }
        }
        self.as_operand(expr)
    }

    fn unit_rvalue(span: Span) -> Rvalue {
        Rvalue::Use(Operand::Constant(Constant {
            ty: Ty::Unit,
            value: ConstValue::Unit,
            span,
        }))
    }

    fn bool_constant(value: bool, span: Span) -> Operand {
        Operand::Constant(Constant {
            ty: Ty::BOOL,
            value: ConstValue::Bool(value),
            span,
        })
    }

    fn lit_constant(&mut self, lit: &Lit, ty: &Ty, span: Span) -> Constant {
        let value = match lit {
            Lit::Int { value, .. } => {
                if ty.as_prim().is_some_and(PrimTy::is_signed_int) {
                    // The type checker only lets `2^127` through as the operand of a
                    // negation (`-170141183460469231731687303715884105728i128`); wrapping
                    // it to `i128::MIN` keeps `-MIN == MIN` exact. Saturating it to
                    // `i128::MAX` made that literal `MIN + 1`.
                    #[allow(clippy::cast_possible_wrap)]
                    ConstValue::Int(*value as i128)
                } else {
                    ConstValue::Uint(*value)
                }
            }
            Lit::Float { text, .. } => {
                if let Ok(f) = sabiko_lexer::literal::parse_float(text) {
                    ConstValue::Float(f.value)
                } else {
                    // The lexer validated the literal; this cannot happen.
                    self.failed = true;
                    ConstValue::Float(0.0)
                }
            }
            Lit::Bool(b) => ConstValue::Bool(*b),
            Lit::Char(c) => ConstValue::Char(*c),
            Lit::Str(s) => ConstValue::Str(s.clone()),
        };
        Constant {
            ty: ty.clone(),
            value,
            span,
        }
    }

    /// The value of a `const` item. One that failed to evaluate (its errors were reported
    /// with the item) poisons this body.
    fn const_constant(&mut self, def: DefId, ty: &Ty, span: Span) -> Constant {
        let value = if let Some(v) = self.typeck.const_value(def) {
            v.clone()
        } else {
            self.failed = true;
            ConstValue::Unit
        };
        Constant {
            ty: ty.clone(),
            value,
            span,
        }
    }

    /// What a path expression names, through parentheses.
    /// `&e` / `&mut e` where `e` is not a place: the borrow of a temporary.
    fn ref_of_temporary(expr: &Expr) -> Option<(bool, &Expr)> {
        if let ExprKind::Ref {
            mutable,
            expr: inner,
        } = &strip_parens(expr).kind
        {
            let is_place = matches!(
                strip_parens(inner).kind,
                ExprKind::Path(_) | ExprKind::Field(..) | ExprKind::Deref(_)
            );
            if !is_place {
                return Some((*mutable, inner));
            }
        }
        None
    }

    /// A place expression of type `&mut T`, which coercion sites reborrow.
    fn is_mut_ref_place_expr(&self, expr: &Expr) -> bool {
        matches!(
            strip_parens(expr).kind,
            ExprKind::Path(_) | ExprKind::Field(..) | ExprKind::Deref(_)
        ) && matches!(self.ty(expr), Ty::Ref(Mutability::Mut, _))
    }

    fn path_res(&self, expr: &Expr) -> Option<Res> {
        match &expr.kind {
            ExprKind::Path(_) => Some(self.resolve.res(expr.id)),
            ExprKind::Paren(inner) => self.path_res(inner),
            _ => None,
        }
    }

    /// The span statements lowered from `expr` carry: its own, or, inside parentheses, that
    /// of the outermost parentheses, as rustc's HIR has it (`(a + b)` overflows at the
    /// `(`), so panic locations agree with rustc's.
    fn span_of(&self, expr: &Expr) -> Span {
        self.paren_spans.get(&expr.id).copied().unwrap_or(expr.span)
    }

    /// Records that `inner`, the contents of the parenthesised `paren`, takes the span of
    /// the outermost parentheses, and returns `inner`.
    fn enter_paren<'e>(&mut self, paren: &Expr, inner: &'e Expr) -> &'e Expr {
        let span = self.span_of(paren);
        self.paren_spans.insert(inner.id, span);
        inner
    }

    fn def_kind(&self, def: DefId) -> DefKind {
        self.resolve.def(def).kind
    }

    // ----- expressions -----

    /// Lowers `expr` to an operand: a constant, or a fresh temporary holding its value.
    ///
    /// Place expressions are copied (or moved) into the temporary at this point rather
    /// than read lazily by the consuming statement, exactly as rustc's `as_operand` does.
    /// The order matters: in `F(p.A, Bump(&mut p))` the first argument must see `p.A`
    /// before `Bump` runs, and a reference temporary such as the result of `Weigh(&mut n)`
    /// in `*Weigh(&mut n) + n.W` must die once its referent is copied, so the sibling
    /// operand can read `n`.
    fn as_operand(&mut self, expr: &Expr) -> Operand {
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.as_operand(inner)
            }
            ExprKind::Lit(lit) => {
                let ty = self.ty(expr).clone();
                Operand::Constant(self.lit_constant(lit, &ty, expr.span))
            }
            ExprKind::Unit => Operand::Constant(Constant {
                ty: Ty::Unit,
                value: ConstValue::Unit,
                span: expr.span,
            }),
            ExprKind::Path(_) if matches!(self.resolve.res(expr.id), Res::Def(def) if self.def_kind(def) == DefKind::Const) =>
            {
                let Res::Def(def) = self.resolve.res(expr.id) else {
                    unreachable!()
                };
                let ty = self.ty(expr).clone();
                Operand::Constant(self.const_constant(def, &ty, expr.span))
            }
            _ => {
                let temp = self.as_temp(expr);
                self.place_operand(Place::local(temp))
            }
        }
    }

    /// Lowers `expr` to a place. Non-place expressions are evaluated into a temporary.
    fn as_place(&mut self, expr: &Expr) -> Place {
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.as_place(inner)
            }
            ExprKind::Path(_) => match self.resolve.res(expr.id) {
                Res::Local(id) => Place::local(self.mir_local(id)),
                _ => Place::local(self.as_temp(expr)),
            },
            ExprKind::Field(base, _) => {
                let access = self.types.field_accesses[&expr.id];
                let field_ty = self.ty(expr).clone();
                let mut place = self.as_place(base);
                for _ in 0..access.autoderefs {
                    place = place.deref();
                }
                place.field(access.index, field_ty)
            }
            ExprKind::Deref(inner) => self.as_place(inner).deref(),
            _ => Place::local(self.as_temp(expr)),
        }
    }

    /// Evaluates `expr` into a fresh temporary.
    fn as_temp(&mut self, expr: &Expr) -> Local {
        let ty = self.ty(expr).clone();
        let temp = self.new_temp(ty, expr.span);
        self.expr_into_dest(&Place::local(temp), expr);
        temp
    }

    /// Lowers `expr` and writes its value into `dest`. Diverging expressions write
    /// nothing and leave the builder in an unreachable block.
    #[allow(clippy::too_many_lines)]
    fn expr_into_dest(&mut self, dest: &Place, expr: &Expr) {
        let span = self.span_of(expr);
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.expr_into_dest(dest, inner);
            }
            ExprKind::Lit(_) | ExprKind::Unit => {
                let op = self.as_operand(expr);
                self.assign(dest.clone(), Rvalue::Use(op), span);
            }
            ExprKind::Tuple(_) => self.tuple_into(dest, expr, false),
            // A place read straight into the destination (`let x = p.A;`), as rustc does:
            // only operands of a larger expression go through a temporary.
            ExprKind::Field(..) | ExprKind::Deref(_) => {
                let place = self.as_place(expr);
                let op = self.place_operand(place);
                self.assign(dest.clone(), Rvalue::Use(op), span);
            }
            ExprKind::Path(_) => match self.resolve.res(expr.id) {
                Res::Local(id) => {
                    let op = self.place_operand(Place::local(self.mir_local(id)));
                    self.assign(dest.clone(), Rvalue::Use(op), span);
                }
                Res::Def(def) => match self.def_kind(def) {
                    DefKind::Const => {
                        let op = self.as_operand(expr);
                        self.assign(dest.clone(), Rvalue::Use(op), span);
                    }
                    DefKind::Struct => {
                        self.assign(
                            dest.clone(),
                            Rvalue::Aggregate(AggregateKind::Adt(def), Vec::new()),
                            span,
                        );
                    }
                    DefKind::Fn => unreachable!("function items as values are rejected"),
                },
                Res::Prim(_) | Res::Err => unreachable!("unresolved path in a checked body"),
            },
            ExprKind::Unary(op, operand) => {
                let operand = self.as_operand(operand);
                self.assign(dest.clone(), Rvalue::UnaryOp(*op, operand), span);
            }
            ExprKind::Binary(op @ (BinOp::And | BinOp::Or), lhs, rhs) => {
                self.lower_logical(dest, *op, lhs, rhs, span);
            }
            ExprKind::Binary(op, lhs, rhs)
                if op.is_comparison() && self.comparison_derefs(lhs, rhs) > 0 =>
            {
                self.lower_ref_comparison(dest, *op, lhs, rhs, span);
            }
            ExprKind::Binary(op, lhs, rhs) => {
                let lhs = self.as_operand(lhs);
                let rhs = self.as_operand(rhs);
                self.assign(dest.clone(), Rvalue::BinaryOp(*op, lhs, rhs), span);
            }
            ExprKind::Assign(..) | ExprKind::AssignOp(..) => {
                self.lower_assign(expr);
                self.assign(dest.clone(), Self::unit_rvalue(span), span);
            }
            ExprKind::Call { callee, args } => {
                let Some(Res::Def(func)) = self.path_res(callee) else {
                    unreachable!("callees are function paths in a checked body")
                };
                // `Print` and `PrintLine` are overloaded and have no signature.
                let inputs = match self.typeck.items.builtin(func) {
                    Some(Builtin::Print | Builtin::PrintLine) => Vec::new(),
                    _ => self.typeck.items.fn_sig(func).inputs.clone(),
                };
                let args: Vec<Operand> = args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| self.coerced_operand(a, inputs.get(i)))
                    .collect();
                let next = self.new_block();
                // A callee returning `!` (`Panic`) has no return edge; what follows lands
                // in an unreachable block, as after `return`.
                let target = if self.ty(expr).is_never() {
                    None
                } else {
                    Some(next)
                };
                self.terminate(
                    TerminatorKind::Call {
                        func,
                        args,
                        destination: dest.clone(),
                        target,
                    },
                    span,
                );
                self.current = next;
            }
            ExprKind::Ref {
                mutable,
                expr: inner,
            } => {
                // `&loop {}`, `&break 4`: the borrow of a diverging operand (typed `&!`)
                // has no value to borrow; only the operand runs, and what follows is
                // unreachable. The destination is never written.
                if self.ty(inner).is_never() {
                    self.lower_expr_discard(inner);
                } else {
                    let place = self.as_place(inner);
                    self.assign(
                        dest.clone(),
                        Rvalue::Ref(Mutability::from_bool(*mutable), place),
                        span,
                    );
                }
            }
            ExprKind::Cast(inner, _) => {
                let target = self.ty(expr).clone();
                let operand = self.as_operand(inner);
                self.assign(dest.clone(), Rvalue::Cast(operand, target), span);
            }
            ExprKind::Block(block) => self.lower_block_into(dest, block),
            ExprKind::If { cond, then, els } => {
                self.lower_if(Some(dest), cond, then, els.as_deref(), span);
            }
            ExprKind::While { cond, body } => {
                self.lower_while(cond, body, span);
                self.assign(dest.clone(), Self::unit_rvalue(span), span);
            }
            ExprKind::Loop { body } => self.lower_loop(Some(dest.clone()), body, span),
            ExprKind::Break(value) => self.lower_break(value.as_deref(), span),
            ExprKind::Continue => self.lower_continue(span),
            ExprKind::Return(value) => self.lower_return(value.as_deref(), span),
            ExprKind::Struct { fields, .. } => {
                let Res::Def(def) = self.resolve.res(expr.id) else {
                    unreachable!("unresolved struct literal in a checked body")
                };
                let adt = self.typeck.items.adt(def);
                // Initialisers run in source order; the aggregate lists declaration order.
                let mut operands: Vec<Option<Operand>> = vec![None; adt.fields.len()];
                let indices: Vec<usize> = fields
                    .iter()
                    .map(|f| {
                        adt.field_index(&f.name.name)
                            .expect("struct literal fields exist in a checked body")
                    })
                    .collect();
                for (field, index) in fields.iter().zip(indices) {
                    let field_ty = adt.fields[index].ty.clone();
                    operands[index] = Some(self.coerced_operand(&field.value, Some(&field_ty)));
                }
                let operands = operands
                    .into_iter()
                    .map(|op| op.expect("every field is initialised in a checked body"))
                    .collect();
                self.assign(
                    dest.clone(),
                    Rvalue::Aggregate(AggregateKind::Adt(def), operands),
                    span,
                );
            }
            ExprKind::MethodCall { .. } | ExprKind::Index(..) => {
                unreachable!("method calls and indexing are rejected by the type checker")
            }
        }
    }

    /// `(a, b, ...)` into `dest`. Elements run left to right; at a coercion site each is
    /// coerced to its element type (a `&mut` place is reborrowed), elsewhere a `&mut` place
    /// element is moved, as in rustc. In a `let` initialiser (`extending`), the operand of
    /// an element `&<rvalue>` lives as long as the binding, and so on into nested tuples
    /// (rustc's temporary lifetime extension: `let p = (&f(), 1);` keeps `f()`'s value).
    fn tuple_into(&mut self, dest: &Place, expr: &Expr, extending: bool) {
        let span = self.span_of(expr);
        let ExprKind::Tuple(elems) = &strip_parens(expr).kind else {
            unreachable!("not a tuple expression")
        };
        let Ty::Tuple(elem_tys) = self.ty(expr).clone() else {
            unreachable!("a tuple expression has a tuple type in a checked body")
        };
        let coerced = self.types.coerced_tuples.contains(&strip_parens(expr).id);
        let operands = elems
            .iter()
            .zip(elem_tys.iter())
            .map(|(e, ty)| {
                if extending {
                    if let Some(op) = self.extended_operand(e) {
                        return op;
                    }
                }
                if coerced {
                    self.coerced_operand(e, Some(ty))
                } else {
                    self.as_operand(e)
                }
            })
            .collect();
        self.assign(
            dest.clone(),
            Rvalue::Aggregate(AggregateKind::Tuple, operands),
            span,
        );
    }

    /// An operand of a `let` initialiser whose temporary rustc extends to the binding's
    /// block: `&<rvalue>` (the rvalue goes into a block-scoped temporary) or a tuple
    /// holding one. `None` for anything else, which is lowered as usual.
    fn extended_operand(&mut self, e: &Expr) -> Option<Operand> {
        if let Some(temp) = self.extended_ref(e) {
            return Some(self.place_operand(Place::local(temp)));
        }
        if let ExprKind::Tuple(_) = strip_parens(e).kind {
            let ty = self.ty(e).clone();
            let temp = self.new_temp(ty, e.span);
            self.tuple_into(&Place::local(temp), e, true);
            return Some(self.place_operand(Place::local(temp)));
        }
        None
    }

    /// `&<rvalue>` in a `let` initialiser: evaluates the rvalue into a temporary that lives
    /// as long as the enclosing block, and returns a statement temporary holding the
    /// reference to it. `None` when `e` is not such a borrow (or borrows a diverging
    /// expression, which has nothing to keep).
    fn extended_ref(&mut self, e: &Expr) -> Option<Local> {
        let (mutable, inner) =
            Self::ref_of_temporary(e).filter(|(_, inner)| !self.ty(inner).is_never())?;
        let ty = self.ty(inner).clone();
        let referent = self.new_temp_in_block_scope(ty, inner.span);
        self.expr_into_dest(&Place::local(referent), inner);
        let ref_ty = self.ty(e).clone();
        let temp = self.new_temp(ref_ty, e.span);
        self.assign(
            Place::local(temp),
            Rvalue::Ref(Mutability::from_bool(mutable), Place::local(referent)),
            e.span,
        );
        Some(temp)
    }

    /// Lowers an expression whose value is not needed (an expression statement). The
    /// statement-like forms need no destination; anything else goes into a temporary that
    /// dies with the statement.
    fn lower_expr_discard(&mut self, expr: &Expr) {
        let span = self.span_of(expr);
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.lower_expr_discard(inner);
            }
            ExprKind::Assign(..) | ExprKind::AssignOp(..) => self.lower_assign(expr),
            ExprKind::Block(block) => self.lower_block_discard(block),
            ExprKind::If { cond, then, els } => {
                self.lower_if(None, cond, then, els.as_deref(), span);
            }
            ExprKind::While { cond, body } => self.lower_while(cond, body, span),
            ExprKind::Loop { body } => self.lower_loop(None, body, span),
            ExprKind::Break(value) => self.lower_break(value.as_deref(), span),
            ExprKind::Continue => self.lower_continue(span),
            ExprKind::Return(value) => self.lower_return(value.as_deref(), span),
            _ => {
                self.as_temp(expr);
            }
        }
    }

    /// How many references both operands of a comparison have: `r == s` for `r, s: &&i32`
    /// compares `**r` with `**s`. Zero for primitive operands and when either side
    /// diverges.
    fn comparison_derefs(&self, lhs: &Expr, rhs: &Expr) -> usize {
        let (mut a, mut b) = (self.ty(lhs), self.ty(rhs));
        let mut derefs = 0;
        while let (Ty::Ref(_, x), Ty::Ref(_, y)) = (a, b) {
            a = x;
            b = y;
            derefs += 1;
        }
        derefs
    }

    /// `r == s` (or any comparison) on references compares the referents, as Rust's
    /// `PartialEq`/`PartialOrd` impls for `&A` do. Each operand is evaluated to a shared
    /// reference first and both are dereferenced in the comparison itself, after both
    /// were evaluated: rustc passes `&r` and `&s` to `PartialEq::eq`, so the referents
    /// stay borrowed while the right operand runs (`r == { *r = 3; s }` is a conflict),
    /// and a `&mut` operand is only borrowed, not moved.
    fn lower_ref_comparison(
        &mut self,
        dest: &Place,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) {
        let derefs = self.comparison_derefs(lhs, rhs);
        let mut lhs = self.comparison_operand(lhs);
        let mut rhs = self.comparison_operand(rhs);
        for _ in 0..derefs {
            lhs = lhs.deref();
            rhs = rhs.deref();
        }
        let lhs = self.place_operand(lhs);
        let rhs = self.place_operand(rhs);
        self.assign(dest.clone(), Rvalue::BinaryOp(op, lhs, rhs), span);
    }

    /// A reference operand of a comparison, as a temporary holding the reference. A place
    /// is reborrowed shared (`&*r`), so a `&mut` place is not moved; anything else is
    /// evaluated into the temporary.
    fn comparison_operand(&mut self, expr: &Expr) -> Place {
        let is_place = match &strip_parens(expr).kind {
            ExprKind::Path(_) => matches!(self.path_res(expr), Some(Res::Local(_))),
            ExprKind::Field(..) | ExprKind::Deref(_) => true,
            _ => false,
        };
        if !is_place {
            return Place::local(self.as_temp(expr));
        }
        let Ty::Ref(_, referent) = self.ty(expr).clone() else {
            unreachable!("comparison operands with derefs are references")
        };
        let place = self.as_place(expr);
        let temp = self.new_temp(Ty::shared_ref(*referent), expr.span);
        self.assign(
            Place::local(temp),
            Rvalue::Ref(Mutability::Not, place.deref()),
            expr.span,
        );
        Place::local(temp)
    }

    /// `p = e` and `p op= e`. The right-hand side is evaluated first, then the place, as
    /// in rustc.
    fn lower_assign(&mut self, expr: &Expr) {
        let span = self.span_of(expr);
        match &expr.kind {
            ExprKind::Assign(lhs, rhs) => {
                let target = self.ty(lhs).clone();
                let rhs = self.coerced_operand(rhs, Some(&target));
                let place = self.as_place(lhs);
                self.assign(place, Rvalue::Use(rhs), span);
            }
            ExprKind::AssignOp(op, lhs, rhs) => {
                let rhs = self.as_operand(rhs);
                let place = self.as_place(lhs);
                let current = self.place_operand(place.clone());
                self.assign(place, Rvalue::BinaryOp(*op, current, rhs), span);
            }
            _ => unreachable!("not an assignment"),
        }
    }

    /// `a && b` / `a || b`: the right operand only runs when the left one does not decide
    /// the result.
    fn lower_logical(&mut self, dest: &Place, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) {
        let lhs = self.as_operand(lhs);
        let rhs_block = self.new_block();
        let short_block = self.new_block();
        let (targets, otherwise, short_value) = match op {
            // `false && _` is false; `true || _` is true.
            BinOp::And => (vec![(0, short_block)], rhs_block, false),
            BinOp::Or => (vec![(0, rhs_block)], short_block, true),
            _ => unreachable!("not a logical operator"),
        };
        self.switch_int(lhs, targets, otherwise, span);
        self.current = rhs_block;
        let rhs = self.as_operand(rhs);
        self.assign(dest.clone(), Rvalue::Use(rhs), span);
        let rhs_end = self.current;
        self.current = short_block;
        self.assign(
            dest.clone(),
            Rvalue::Use(Self::bool_constant(short_value, span)),
            span,
        );
        let join = self.new_block();
        self.terminate_block(rhs_end, TerminatorKind::Goto { target: join }, span);
        self.terminate_block(short_block, TerminatorKind::Goto { target: join }, span);
        self.current = join;
    }

    /// `if cond { then } else { els }`. `bool` is `0`/`1`, so `0` selects the else
    /// branch. Without `else`, the value is `()`.
    fn lower_if(
        &mut self,
        dest: Option<&Place>,
        cond: &Expr,
        then: &Block,
        els: Option<&Expr>,
        span: Span,
    ) {
        let discr = self.as_operand(cond);
        let then_block = self.new_block();
        let else_block = self.new_block();
        self.switch_int(discr, vec![(0, else_block)], then_block, span);
        self.current = then_block;
        match dest {
            Some(dest) => self.lower_block_into(dest, then),
            None => self.lower_block_discard(then),
        }
        let then_end = self.current;
        self.current = else_block;
        match (els, dest) {
            (Some(els), Some(dest)) => self.expr_into_dest(dest, els),
            (Some(els), None) => self.lower_expr_discard(els),
            (None, Some(dest)) => self.assign(dest.clone(), Self::unit_rvalue(span), span),
            (None, None) => {}
        }
        let else_end = self.current;
        let join = self.new_block();
        self.terminate_block(then_end, TerminatorKind::Goto { target: join }, span);
        self.terminate_block(else_end, TerminatorKind::Goto { target: join }, span);
        self.current = join;
    }

    /// `while cond { body }`. The condition's temporaries die at the start of both
    /// successors, so each iteration starts from the same storage state.
    fn lower_while(&mut self, cond: &Expr, body: &Block, span: Span) {
        let header = self.new_block();
        self.terminate(TerminatorKind::Goto { target: header }, span);
        self.current = header;
        self.push_scope();
        let discr = self.as_operand(cond);
        let cond_temps = self.scopes.pop().expect("condition scope");
        let body_block = self.new_block();
        let false_block = self.new_block();
        self.switch_int(discr, vec![(0, false_block)], body_block, span);
        self.loops.push(LoopScope {
            header,
            exit: None,
            dest: None,
            scope_depth: self.scopes.len(),
        });
        self.current = body_block;
        self.storage_dead(&cond_temps, span);
        self.lower_block_discard(body);
        self.terminate(TerminatorKind::Goto { target: header }, span);
        let scope = self.loops.pop().expect("loop scope pushed above");
        self.current = false_block;
        self.storage_dead(&cond_temps, span);
        let exit = scope.exit.unwrap_or_else(|| self.new_block());
        self.terminate(TerminatorKind::Goto { target: exit }, span);
        self.current = exit;
    }

    /// `loop { body }`. Only `break` leaves it; without one nothing after the loop is
    /// reachable.
    fn lower_loop(&mut self, dest: Option<Place>, body: &Block, span: Span) {
        let header = self.new_block();
        self.terminate(TerminatorKind::Goto { target: header }, span);
        self.current = header;
        self.loops.push(LoopScope {
            header,
            exit: None,
            dest,
            scope_depth: self.scopes.len(),
        });
        self.lower_block_discard(body);
        self.terminate(TerminatorKind::Goto { target: header }, span);
        let scope = self.loops.pop().expect("loop scope pushed above");
        self.current = scope.exit.unwrap_or_else(|| self.new_block());
    }

    fn lower_break(&mut self, value: Option<&Expr>, span: Span) {
        let depth = self.loops.len() - 1;
        let dest = self.loops[depth].dest.clone();
        match (value, dest) {
            (Some(value), Some(dest)) => self.expr_into_dest(&dest, value),
            (Some(value), None) => self.lower_expr_discard(value),
            (None, Some(dest)) => {
                if dest.ty(&self.body).is_unit() {
                    self.assign(dest, Self::unit_rvalue(span), span);
                }
            }
            (None, None) => {}
        }
        self.exit_scopes(self.loops[depth].scope_depth, span);
        let exit = if let Some(bb) = self.loops[depth].exit {
            bb
        } else {
            let bb = self.new_block();
            self.loops[depth].exit = Some(bb);
            bb
        };
        self.jump_away(TerminatorKind::Goto { target: exit }, span);
    }

    fn lower_continue(&mut self, span: Span) {
        let scope = self.loops.last().expect("`continue` inside a loop");
        let (header, depth) = (scope.header, scope.scope_depth);
        self.exit_scopes(depth, span);
        self.jump_away(TerminatorKind::Goto { target: header }, span);
    }

    fn lower_return(&mut self, value: Option<&Expr>, span: Span) {
        let ret = Place::local(Local::RETURN);
        match value {
            Some(value) if self.is_mut_ref_place_expr(value) => {
                let target = ret.ty(&self.body);
                let op = self.coerced_operand(value, Some(&target));
                self.assign(ret, Rvalue::Use(op), span);
            }
            Some(value) => self.expr_into_dest(&ret, value),
            None => self.assign(ret, Self::unit_rvalue(span), span),
        }
        self.exit_scopes(0, span);
        let target = self.return_block();
        self.jump_away(TerminatorKind::Goto { target }, span);
    }

    // ----- blocks and statements -----

    /// Lowers a block, writing its tail into `dest`. A block without a tail yields `()`
    /// when the destination is a unit place; a diverging one yields nothing.
    fn lower_block_into(&mut self, dest: &Place, block: &Block) {
        self.push_scope();
        for stmt in &block.stmts {
            self.lower_stmt(stmt);
        }
        match &block.tail {
            Some(tail) => self.expr_into_dest(dest, tail),
            None => {
                if dest.ty(&self.body).is_unit() {
                    self.assign(dest.clone(), Self::unit_rvalue(block.span), block.span);
                }
            }
        }
        self.pop_scope(block.span);
    }

    /// Lowers a block whose value is not needed.
    fn lower_block_discard(&mut self, block: &Block) {
        self.push_scope();
        for stmt in &block.stmts {
            self.lower_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.lower_expr_discard(tail);
        }
        self.pop_scope(block.span);
    }

    /// `let (a, (b, _)) = init;`. A place initialiser is destructured where it is, as in
    /// rustc: each binding copies or moves its own element out of it, and `_` touches
    /// nothing (`let (a, _) = t;` leaves `t.1` usable). Any other initialiser is
    /// evaluated into a temporary first; the referent of `&<rvalue>` lives as long as the
    /// bindings (temporary lifetime extension, as for `let r = &<rvalue>;`). Without an
    /// initialiser the bindings are only declared.
    fn lower_let_tuple(&mut self, pat: &Pat, init: Option<&Expr>) {
        let Some(init) = init else {
            self.declare_pattern_locals(pat);
            return;
        };
        let place = if is_place_expr(self.resolve, init) {
            self.as_place(init)
        } else if let Some(temp) = self.extended_ref(init) {
            Place::local(temp)
        } else if let ExprKind::Tuple(_) = strip_parens(init).kind {
            let ty = self.ty(init).clone();
            let temp = self.new_temp(ty, init.span);
            self.tuple_into(&Place::local(temp), init, true);
            Place::local(temp)
        } else {
            Place::local(self.as_temp(init))
        };
        if self.ty(init).is_uninhabited() {
            // `let (a, b) = return;`: nothing to bind; the code after it is unreachable.
            self.declare_pattern_locals(pat);
            return;
        }
        self.bind_pattern(pat, place);
    }

    /// Binds the names of `pat` to the parts of `place`, which holds a value of the
    /// pattern's type (or a reference to it, for default binding modes).
    fn bind_pattern(&mut self, pat: &Pat, place: Place) {
        match &pat.kind {
            PatKind::Ident { .. } => {
                let id = self
                    .resolve
                    .local_of_pat(pat.id)
                    .expect("identifier patterns introduce locals");
                let local = self.declare_user_local(id, pat.span);
                let rvalue = match self.types.ref_bindings.get(&pat.id) {
                    Some(&m) => Rvalue::Ref(m, place),
                    None => Rvalue::Use(self.place_operand(place)),
                };
                self.assign(Place::local(local), rvalue, pat.span);
            }
            PatKind::Wild => {}
            PatKind::Tuple(elems) => {
                let mut place = place;
                for _ in 0..self.types.pat_derefs.get(&pat.id).copied().unwrap_or(0) {
                    place = place.deref();
                }
                let elem_tys = match place.ty(&self.body) {
                    Ty::Tuple(tys) => *tys,
                    _ => Vec::new(),
                };
                for (i, (p, ty)) in elems.iter().zip(elem_tys).enumerate() {
                    self.bind_pattern(p, place.clone().field(i, ty));
                }
            }
        }
    }

    /// Declares every binding of `pat` without initialising it (`let (a, b);`).
    fn declare_pattern_locals(&mut self, pat: &Pat) {
        match &pat.kind {
            PatKind::Ident { .. } => {
                let id = self
                    .resolve
                    .local_of_pat(pat.id)
                    .expect("identifier patterns introduce locals");
                self.declare_user_local(id, pat.span);
            }
            PatKind::Wild => {}
            PatKind::Tuple(elems) => {
                for p in elems {
                    self.declare_pattern_locals(p);
                }
            }
        }
    }

    /// Lowers one statement. Temporaries it creates die when it ends.
    fn lower_stmt(&mut self, stmt: &Stmt) {
        self.push_scope();
        match &stmt.kind {
            StmtKind::Let(l) => match &l.pat.kind {
                PatKind::Ident { .. } => {
                    let id = self
                        .resolve
                        .local_of_pat(l.pat.id)
                        .expect("identifier patterns introduce locals");
                    let local = self.declare_user_local(id, l.pat.span);
                    if let Some(init) = &l.init {
                        if l.ty.is_some() && self.is_mut_ref_place_expr(init) {
                            // `let r: &mut T = p;` reborrows; `let r = p;` moves.
                            let target = Place::local(local).ty(&self.body);
                            let op = self.coerced_operand(init, Some(&target));
                            self.assign(Place::local(local), Rvalue::Use(op), init.span);
                        } else if let Some((mutable, inner)) = Self::ref_of_temporary(init)
                            .filter(|(_, inner)| !self.ty(inner).is_never())
                        {
                            // Temporary lifetime extension: in `let r = &<rvalue>;` the
                            // temporary lives as long as `r`'s block, as in Rust.
                            let ty = self.ty(inner).clone();
                            let temp = self.new_temp_in_block_scope(ty, inner.span);
                            self.expr_into_dest(&Place::local(temp), inner);
                            self.assign(
                                Place::local(local),
                                Rvalue::Ref(Mutability::from_bool(mutable), Place::local(temp)),
                                init.span,
                            );
                        } else if let ExprKind::Tuple(_) = strip_parens(init).kind {
                            // `let p = (&<rvalue>, ..);` extends the rvalue too.
                            self.tuple_into(&Place::local(local), init, true);
                        } else {
                            self.expr_into_dest(&Place::local(local), init);
                        }
                    }
                }
                // TODO: rustc only mentions a place initialiser here (a liveness use of its
                // local, no read, move or initialisation check: `let _ = p;` leaves a
                // non-Copy `p` usable); Sabiko reads it, which rejects some valid programs.
                PatKind::Wild => {
                    if let Some(init) = &l.init {
                        self.as_temp(init);
                    }
                }
                PatKind::Tuple(_) => self.lower_let_tuple(&l.pat, l.init.as_ref()),
            },
            StmtKind::Expr { expr, .. } => self.lower_expr_discard(expr),
            StmtKind::Empty => {}
        }
        self.pop_scope(stmt.span);
    }
}

/// Whether `expr` denotes a place (a local, a field of a place, a dereference) rather than
/// a value to evaluate.
fn is_place_expr(resolve: &ResolveResults, expr: &Expr) -> bool {
    match &strip_parens(expr).kind {
        ExprKind::Path(_) => matches!(resolve.res(strip_parens(expr).id), Res::Local(_)),
        ExprKind::Field(..) | ExprKind::Deref(_) => true,
        _ => false,
    }
}

/// The span after the parameters' end, for the storage markers of their bindings.
fn param_span(f: &FnDecl) -> Span {
    f.params.last().map_or(f.name.span, |p| p.span)
}

fn strip_parens(mut e: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    e
}
