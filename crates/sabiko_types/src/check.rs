//! Type checking of function bodies and constant initialisers.
//!
//! Bidirectional: every expression is checked against an optional expected type, which
//! flows down from annotations, parameter types, return types and struct field types, and
//! unification flows information back up. The only coercions are `!` to anything and
//! `&mut T` to `&T`.

use std::collections::HashMap;

use sabiko_ast::{
    BinOp, Block, Expr, ExprKind, FnDecl, Lit, NodeId, PatKind, Stmt, StmtKind, Type, TypeKind,
    UnOp,
};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{Builtin, DefId, DefKind, LocalId, PrimTy, Res, ResolveResults};
use sabiko_span::Span;

use crate::collect::{self, ItemTypes};
use crate::infer::{InferCtxt, VarKind};
use crate::ty::{Mutability, Ty};

/// A field access `e.f`: which field, and how many references were peeled off `e` first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldAccess {
    pub index: usize,
    pub autoderefs: usize,
}

/// Per-body results, with every inference variable resolved.
#[derive(Clone, Debug, Default)]
pub struct BodyTypes {
    /// Types of expressions, keyed by `Expr::id`.
    pub expr_types: HashMap<NodeId, Ty>,
    pub local_types: HashMap<LocalId, Ty>,
    /// Keyed by the `ExprKind::Field` node.
    pub field_accesses: HashMap<NodeId, FieldAccess>,
    /// `true` when at least one error was reported inside this body.
    pub has_errors: bool,
}

impl BodyTypes {
    #[must_use]
    pub fn expr_ty(&self, id: NodeId) -> &Ty {
        self.expr_types
            .get(&id)
            .unwrap_or_else(|| panic!("no type recorded for expression {id:?}"))
    }

    #[must_use]
    pub fn local_ty(&self, id: LocalId) -> &Ty {
        &self.local_types[&id]
    }
}

struct LoopCtxt {
    is_loop: bool,
    /// Type carried by `break value` (for `loop` only); `None` until the first `break`.
    break_ty: Option<Ty>,
    /// What the `loop` expression is expected to produce.
    expected: Option<Ty>,
    has_break: bool,
}

struct IntLit {
    span: Span,
    value: u128,
    negated: bool,
    ty: Ty,
}

struct PendingCast {
    span: Span,
    from: Ty,
    to: Ty,
}

pub(crate) struct FnCtxt<'a> {
    resolve: &'a ResolveResults,
    items: &'a ItemTypes,
    sink: &'a mut DiagnosticSink,
    infer: InferCtxt,
    expr_types: HashMap<NodeId, Ty>,
    local_types: HashMap<LocalId, Ty>,
    /// Pattern spans of locals, for "type annotations needed".
    local_spans: HashMap<LocalId, Span>,
    field_accesses: HashMap<NodeId, FieldAccess>,
    ret_ty: Ty,
    loops: Vec<LoopCtxt>,
    diverges: bool,
    /// Set while checking the operand of unary `-`, so `-128i8` is in range.
    negating: bool,
    int_lits: Vec<IntLit>,
    casts: Vec<PendingCast>,
    /// `-e` where `e` was still `{integer}`: rejected at the end if it becomes unsigned.
    pending_negs: Vec<(Span, Ty)>,
    /// Variables for `let x = <diverging>;`: unconstrained ones fall back to `()`.
    diverging_vars: Vec<Ty>,
    errors_before: usize,
}

impl<'a> FnCtxt<'a> {
    pub(crate) fn new(
        resolve: &'a ResolveResults,
        items: &'a ItemTypes,
        sink: &'a mut DiagnosticSink,
        ret_ty: Ty,
    ) -> Self {
        let errors_before = sink.error_count();
        Self {
            resolve,
            items,
            sink,
            infer: InferCtxt::new(),
            expr_types: HashMap::new(),
            local_types: HashMap::new(),
            local_spans: HashMap::new(),
            field_accesses: HashMap::new(),
            ret_ty,
            loops: Vec::new(),
            diverges: false,
            negating: false,
            int_lits: Vec::new(),
            casts: Vec::new(),
            pending_negs: Vec::new(),
            diverging_vars: Vec::new(),
            errors_before,
        }
    }

    pub(crate) fn check_fn(mut self, f: &FnDecl, def: DefId) -> BodyTypes {
        let sig = self.items.fn_sig(def);
        for (p, ty) in f.params.iter().zip(sig.inputs.clone()) {
            if let Some(local) = self.resolve.local_of_pat(p.pat.id) {
                self.local_types.insert(local, ty);
                self.local_spans.insert(local, p.pat.span);
            }
        }
        let ret = self.ret_ty.clone();
        self.check_block(&f.body, Some(&ret));
        self.finish()
    }

    pub(crate) fn check_const(mut self, value: &Expr) -> BodyTypes {
        let ty = self.ret_ty.clone();
        self.check_expr(value, Some(&ty));
        self.finish()
    }

    // ----- helpers -----

    fn ty_str(&mut self, ty: &Ty) -> String {
        let ty = self.infer.resolve(ty);
        match ty {
            Ty::Infer(v) => match self.infer.kind(v) {
                VarKind::Int => "{integer}".to_owned(),
                VarKind::Float => "{float}".to_owned(),
                VarKind::General => "_".to_owned(),
            },
            Ty::Ref(m, inner) => format!("&{}{}", m.prefix_str(), self.ty_str(&inner)),
            t => t.display(self.resolve),
        }
    }

    fn error(&mut self, d: Diagnostic) {
        self.sink.emit(d);
    }

    fn mismatch(&mut self, span: Span, expected: &Ty, found: &Ty) {
        let (e, f) = (self.ty_str(expected), self.ty_str(found));
        self.error(
            Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
                .with_label(span, format!("expected `{e}`, found `{f}`")),
        );
    }

    fn unsupported(&mut self, what: &str, span: Span, phase: &str) {
        self.error(
            Diagnostic::error(
                ErrorCode::UNSUPPORTED_SYNTAX,
                format!("not yet supported: {what}"),
            )
            .with_label(span, format!("{what} are not yet supported"))
            .with_note(format!("{phase}; see docs/PLAN.md")),
        );
    }

    fn shallow(&mut self, ty: &Ty) -> Ty {
        self.infer.shallow_resolve(ty)
    }

    /// Makes `found` acceptable where `expected` is required, reporting a mismatch at
    /// `span` if it is not.
    fn coerce(&mut self, span: Span, found: &Ty, expected: &Ty) {
        if self.try_coerce(found, expected).is_err() {
            self.mismatch(span, expected, found);
        }
    }

    fn try_coerce(&mut self, found: &Ty, expected: &Ty) -> Result<(), ()> {
        let found = self.shallow(found);
        let expected = self.shallow(expected);
        if found.is_never() {
            return Ok(());
        }
        if self.infer.unify(&found, &expected).is_ok() {
            return Ok(());
        }
        // `&mut T` weakens to `&T`.
        if let (Ty::Ref(Mutability::Mut, a), Ty::Ref(Mutability::Not, b)) = (&found, &expected) {
            if self.infer.unify(a, b).is_ok() {
                return Ok(());
            }
        }
        Err(())
    }

    fn lower_type(&mut self, ty: &Type) -> Ty {
        let t = self.lower_type_unchecked(ty);
        collect::check_sized(&t, ty.span, self.sink);
        t
    }

    fn lower_type_unchecked(&mut self, ty: &Type) -> Ty {
        match &ty.kind {
            TypeKind::Infer => self.infer.new_var(VarKind::General),
            TypeKind::Ref { mutable, inner } => {
                let inner = self.lower_type_unchecked(inner);
                Ty::Ref(Mutability::from_bool(*mutable), Box::new(inner))
            }
            _ => collect::lower_sig_type(ty, self.resolve),
        }
    }

    fn record(&mut self, expr: &Expr, ty: Ty) -> Ty {
        self.expr_types.insert(expr.id, ty.clone());
        ty
    }

    // ----- blocks and statements -----

    fn check_block(&mut self, block: &Block, expected: Option<&Ty>) -> Ty {
        let outer_diverges = self.diverges;
        self.diverges = false;
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        let ty = match &block.tail {
            Some(tail) => {
                let t = self.check_expr(tail, expected);
                if self.diverges {
                    Ty::Never
                } else {
                    t
                }
            }
            None if self.diverges => Ty::Never,
            None => {
                if let Some(exp) = expected {
                    // Point at the closing brace, like rustc.
                    let close = Span::new(
                        block.span.file,
                        block.span.end.saturating_sub(1),
                        block.span.end,
                    );
                    self.coerce(close, &Ty::Unit, exp);
                }
                Ty::Unit
            }
        };
        self.diverges |= outer_diverges;
        ty
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let(l) => {
                let annot = l.ty.as_ref().map(|t| self.lower_type(t));
                let init_ty = l
                    .init
                    .as_ref()
                    .map(|init| self.check_expr(init, annot.as_ref()));
                let ty = match (annot, init_ty) {
                    (Some(a), _) => a,
                    (None, Some(i)) => {
                        // `let x = return;` gives `x: !`; keep it as a fresh variable so
                        // uses of `x` are not all typed `!`.
                        if self.shallow(&i).is_never() {
                            let v = self.infer.new_var(VarKind::General);
                            self.diverging_vars.push(v.clone());
                            v
                        } else {
                            i
                        }
                    }
                    (None, None) => self.infer.new_var(VarKind::General),
                };
                if let PatKind::Ident { .. } = l.pat.kind {
                    let local = self
                        .resolve
                        .local_of_pat(l.pat.id)
                        .expect("identifier patterns introduce locals");
                    self.local_types.insert(local, ty);
                    self.local_spans.insert(local, l.pat.span);
                }
            }
            StmtKind::Expr { expr, semi } => {
                if *semi {
                    self.check_expr(expr, None);
                } else {
                    // A block-like expression used as a statement must be `()`.
                    self.check_expr(expr, Some(&Ty::Unit));
                }
            }
            StmtKind::Empty => {}
        }
    }

    // ----- expressions -----

    /// Checks `expr` against `expected` (coercing and reporting mismatches) and returns
    /// the expression's own type, which is also recorded.
    fn check_expr(&mut self, expr: &Expr, expected: Option<&Ty>) -> Ty {
        let ty = self.check_expr_kind(expr, expected);
        if let Some(exp) = expected {
            // Blocks and `if` report mismatches on their tails / branches themselves.
            if !matches!(
                expr.kind,
                ExprKind::Block(_) | ExprKind::If { .. } | ExprKind::Paren(_)
            ) {
                self.coerce(expr.span, &ty, exp);
            }
        }
        self.record(expr, ty)
    }

    #[allow(clippy::too_many_lines)]
    fn check_expr_kind(&mut self, expr: &Expr, expected: Option<&Ty>) -> Ty {
        match &expr.kind {
            ExprKind::Lit(lit) => self.check_lit(lit, expr.span, expected),
            ExprKind::Unit => Ty::Unit,
            ExprKind::Path(_) => self.check_path(expr, false),
            ExprKind::Paren(inner) => self.check_expr(inner, expected),
            ExprKind::Unary(op, operand) => self.check_unary(*op, operand, expected),
            ExprKind::Binary(op, lhs, rhs) => self.check_binary(*op, lhs, rhs, expected, expr.span),
            ExprKind::Assign(lhs, rhs) => {
                let lhs_ty = self.check_place(lhs);
                self.check_expr(rhs, Some(&lhs_ty));
                Ty::Unit
            }
            ExprKind::AssignOp(op, lhs, rhs) => {
                let lhs_ty = self.check_place(lhs);
                let rhs_expected = if matches!(op, BinOp::Shl | BinOp::Shr) {
                    None
                } else {
                    Some(&lhs_ty)
                };
                let rhs_ty = self.check_expr(rhs, rhs_expected);
                self.check_arith_operands(*op, &lhs_ty, &rhs_ty, expr.span);
                Ty::Unit
            }
            ExprKind::Call { callee, args } => self.check_call(callee, args, expr.span),
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => {
                self.check_expr(receiver, None);
                for a in args {
                    self.check_expr(a, None);
                }
                self.unsupported("method calls", method.span, "planned for Phase 5");
                Ty::Error
            }
            ExprKind::Field(base, name) => {
                let base_ty = self.check_expr(base, None);
                self.check_field(expr, &base_ty, &name.name, name.span)
            }
            ExprKind::Index(base, index) => {
                self.check_expr(base, None);
                self.check_expr(index, None);
                self.unsupported("indexing", expr.span, "planned for Phase 5");
                Ty::Error
            }
            ExprKind::Ref {
                mutable,
                expr: inner,
            } => {
                let m = Mutability::from_bool(*mutable);
                let inner_expected = match expected.map(|e| self.shallow(e)) {
                    Some(Ty::Ref(_, t)) => Some(*t),
                    _ => None,
                };
                let errors_before = self.sink.error_count();
                let inner_ty = self.check_expr(inner, inner_expected.as_ref());
                if self.shallow(&inner_ty).is_never() {
                    return Ty::Never;
                }
                if inner_expected.is_some() && self.sink.error_count() > errors_before {
                    // The operand already failed against the expected referent; a second
                    // report for the reference would say the same thing.
                    return Ty::Error;
                }
                Ty::Ref(m, Box::new(inner_ty))
            }
            ExprKind::Deref(inner) => {
                let inner_ty = self.check_expr(inner, None);
                match self.shallow(&inner_ty) {
                    Ty::Ref(_, t) => {
                        if !collect::check_sized(&t, expr.span, self.sink) {
                            return Ty::Error;
                        }
                        *t
                    }
                    Ty::Error => Ty::Error,
                    // `*never` is rejected like rustc's E0614; accepting it would leave
                    // MIR building a deref of a non-reference.
                    other => {
                        let s = self.ty_str(&other);
                        self.error(
                            Diagnostic::error(
                                ErrorCode::INVALID_DEREF,
                                format!("type `{s}` cannot be dereferenced"),
                            )
                            .with_label(expr.span, "can't be dereferenced"),
                        );
                        Ty::Error
                    }
                }
            }
            ExprKind::Cast(inner, ty) => {
                let to = self.lower_type(ty);
                let from = self.check_expr(inner, None);
                let from_s = self.shallow(&from);
                let to_s = self.shallow(&to);
                // An unsuffixed literal takes the target type when the families agree.
                if let (Ty::Infer(v), Ty::Prim(p)) = (&from_s, &to_s) {
                    let kind = self.infer.kind(*v);
                    if (kind == VarKind::Int && p.is_integer())
                        || (kind == VarKind::Float && p.is_float())
                    {
                        let _ = self.infer.unify(&from_s, &to_s);
                    }
                }
                self.casts.push(PendingCast {
                    span: expr.span,
                    from: from_s,
                    to: to.clone(),
                });
                to
            }
            ExprKind::Block(block) => self.check_block(block, expected),
            ExprKind::If { cond, then, els } => {
                self.check_if(cond, then, els.as_deref(), expected, expr.span)
            }
            ExprKind::While { cond, body } => {
                self.check_expr(cond, Some(&Ty::BOOL));
                let cond_diverges = self.diverges;
                self.loops.push(LoopCtxt {
                    is_loop: false,
                    break_ty: None,
                    expected: None,
                    has_break: false,
                });
                self.check_block(body, Some(&Ty::Unit));
                self.loops.pop();
                // The body may never run, so only a diverging condition diverges.
                self.diverges = cond_diverges;
                Ty::Unit
            }
            ExprKind::Loop { body } => {
                self.loops.push(LoopCtxt {
                    is_loop: true,
                    break_ty: None,
                    expected: expected.cloned(),
                    has_break: false,
                });
                let before = self.diverges;
                self.check_block(body, Some(&Ty::Unit));
                let ctxt = self.loops.pop().expect("loop context pushed above");
                if ctxt.has_break {
                    self.diverges = before;
                    ctxt.break_ty.unwrap_or(Ty::Unit)
                } else {
                    self.diverges = true;
                    Ty::Never
                }
            }
            ExprKind::Break(value) => {
                self.check_break(value.as_deref(), expr.span);
                self.diverges = true;
                Ty::Never
            }
            ExprKind::Continue => {
                if self.loops.is_empty() {
                    self.error(
                        Diagnostic::error(
                            ErrorCode::BREAK_OUTSIDE_LOOP,
                            "`continue` outside of a loop",
                        )
                        .with_label(expr.span, "cannot `continue` outside of a loop"),
                    );
                }
                self.diverges = true;
                Ty::Never
            }
            ExprKind::Return(value) => {
                let ret = self.ret_ty.clone();
                match value {
                    Some(v) => {
                        self.check_expr(v, Some(&ret));
                    }
                    None => {
                        if self.try_coerce(&Ty::Unit, &ret).is_err() {
                            let r = self.ty_str(&ret);
                            self.error(
                                Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
                                    .with_label(expr.span, format!("expected `{r}`, found `()`"))
                                    .with_note(
                                        "`return;` in a function whose return type is not `()`",
                                    ),
                            );
                        }
                    }
                }
                self.diverges = true;
                Ty::Never
            }
            ExprKind::Struct { fields, .. } => self.check_struct_literal(expr, fields),
        }
    }

    fn check_lit(&mut self, lit: &Lit, span: Span, expected: Option<&Ty>) -> Ty {
        match lit {
            Lit::Int { value, suffix } => {
                let negated = std::mem::take(&mut self.negating);
                let ty = if let Some(s) = suffix {
                    Ty::Prim(int_suffix_prim(*s))
                } else {
                    let v = self.infer.new_var(VarKind::Int);
                    // Take the expectation eagerly so the range check sees it.
                    if let Some(e) = expected {
                        let _ = self.try_coerce(&v, e);
                    }
                    v
                };
                self.int_lits.push(IntLit {
                    span,
                    value: *value,
                    negated,
                    ty: ty.clone(),
                });
                ty
            }
            Lit::Float { suffix, .. } => {
                self.negating = false;
                match suffix {
                    Some(sabiko_ast::FloatSuffix::F32) => Ty::Prim(PrimTy::F32),
                    Some(sabiko_ast::FloatSuffix::F64) => Ty::Prim(PrimTy::F64),
                    None => self.infer.new_var(VarKind::Float),
                }
            }
            Lit::Bool(_) => Ty::BOOL,
            Lit::Char(_) => Ty::CHAR,
            Lit::Str(_) => Ty::shared_ref(Ty::Prim(PrimTy::Str)),
        }
    }

    fn check_path(&mut self, expr: &Expr, callee: bool) -> Ty {
        let ExprKind::Path(path) = &expr.kind else {
            unreachable!()
        };
        match self.resolve.res(expr.id) {
            Res::Local(l) => self.local_types[&l].clone(),
            Res::Def(d) => {
                let info = self.resolve.def(d);
                match info.kind {
                    DefKind::Fn => {
                        if !callee {
                            self.error(
                                Diagnostic::error(
                                    ErrorCode::FN_ITEM_AS_VALUE,
                                    format!("function `{}` cannot be used as a value yet", info.name),
                                )
                                .with_label(expr.span, "function items are only callable")
                                .with_note("function pointers and closures are planned for Phase 6; see docs/PLAN.md"),
                            );
                            return Ty::Error;
                        }
                        Ty::FnDef(d)
                    }
                    DefKind::Const => self.items.const_ty(d).clone(),
                    DefKind::Struct => {
                        let adt = self.items.adt(d);
                        if adt.is_unit {
                            Ty::Adt(d)
                        } else {
                            let name = adt.name.clone();
                            self.error(
                                Diagnostic::error(
                                    ErrorCode::WRONG_NAMESPACE,
                                    format!("expected value, found struct `{name}`"),
                                )
                                .with_label(path.span, "not a value")
                                .with_note(format!(
                                    "use struct literal syntax instead: `{name} {{ .. }}`"
                                )),
                            );
                            Ty::Error
                        }
                    }
                }
            }
            Res::Prim(_) => unreachable!("the resolver rejects primitive names in value position"),
            Res::Err => Ty::Error,
        }
    }

    fn check_unary(&mut self, op: UnOp, operand: &Expr, expected: Option<&Ty>) -> Ty {
        if op == UnOp::Neg {
            self.negating = matches!(strip_parens(operand).kind, ExprKind::Lit(Lit::Int { .. }));
        }
        let ty = self.check_expr(operand, expected);
        self.negating = false;
        let resolved = self.shallow(&ty);
        let ok = match (&resolved, op) {
            (Ty::Error | Ty::Never, _) => true,
            (Ty::Infer(v), _) => {
                let kind = self.infer.kind(*v);
                match op {
                    UnOp::Neg => {
                        if kind == VarKind::Int {
                            self.pending_negs.push((operand.span, ty.clone()));
                        }
                        true
                    }
                    UnOp::Not => kind != VarKind::Float,
                }
            }
            (Ty::Prim(p), UnOp::Neg) => p.is_signed_int() || p.is_float(),
            (Ty::Prim(p), UnOp::Not) => p.is_integer() || *p == PrimTy::Bool,
            _ => false,
        };
        if !ok {
            let s = self.ty_str(&resolved);
            self.error(
                Diagnostic::error(
                    ErrorCode::INVALID_UNARY_OP,
                    format!(
                        "cannot apply unary operator `{}` to type `{s}`",
                        op.as_str()
                    ),
                )
                .with_label(
                    operand.span,
                    format!("cannot apply unary operator `{}`", op.as_str()),
                ),
            );
            return Ty::Error;
        }
        ty
    }

    fn check_binary(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> Ty {
        match op {
            BinOp::And | BinOp::Or => {
                self.check_expr(lhs, Some(&Ty::BOOL));
                self.check_expr(rhs, Some(&Ty::BOOL));
                Ty::BOOL
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.check_expr(lhs, None);
                let r = self.check_expr(rhs, Some(&l));
                let _ = r;
                let resolved = self.shallow(&l);
                let ok = match &resolved {
                    Ty::Error | Ty::Never | Ty::Infer(_) | Ty::Unit => true,
                    Ty::Prim(p) => *p != PrimTy::Str,
                    _ => false,
                };
                if !ok {
                    self.binary_op_error(op, &resolved, span);
                }
                Ty::BOOL
            }
            BinOp::Shl | BinOp::Shr => {
                let l = self.check_expr(lhs, expected);
                let r = self.check_expr(rhs, None);
                let lr = self.shallow(&l);
                let rr = self.shallow(&r);
                let l_ok = self.is_integer_like(&lr);
                let r_ok = self.is_integer_like(&rr);
                if !l_ok || !r_ok {
                    let (ls, rs) = (self.ty_str(&lr), self.ty_str(&rr));
                    let bad = if l_ok { &rs } else { &ls };
                    self.error(
                        Diagnostic::error(
                            ErrorCode::INVALID_BINARY_OP,
                            format!(
                                "binary operation `{}` cannot be applied to type `{bad}`",
                                op.as_str()
                            ),
                        )
                        .with_label(
                            span,
                            format!("no implementation for `{ls} {} {rs}`", op.as_str()),
                        ),
                    );
                    if !l_ok {
                        return Ty::Error;
                    }
                }
                l
            }
            _ => {
                let l = self.check_expr(lhs, expected);
                let r = self.check_expr(rhs, Some(&l));
                if self.check_arith_operands(op, &l, &r, span) {
                    l
                } else {
                    Ty::Error
                }
            }
        }
    }

    fn is_integer_like(&mut self, ty: &Ty) -> bool {
        match ty {
            Ty::Error | Ty::Never => true,
            Ty::Infer(v) => self.infer.kind(*v) != VarKind::Float,
            Ty::Prim(p) => p.is_integer(),
            _ => false,
        }
    }

    /// Checks that `+ - * / % & | ^` (and their compound forms) apply to `lhs`'s type.
    /// `rhs` has already been unified with `lhs`.
    fn check_arith_operands(&mut self, op: BinOp, lhs: &Ty, _rhs: &Ty, span: Span) -> bool {
        let resolved = self.shallow(lhs);
        let ok = match (&resolved, op) {
            (Ty::Infer(v), BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor) => {
                self.infer.kind(*v) != VarKind::Float
            }
            (Ty::Error | Ty::Never | Ty::Infer(_), _) => true,
            (Ty::Prim(p), BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor) => {
                p.is_integer() || *p == PrimTy::Bool
            }
            (Ty::Prim(p), BinOp::Shl | BinOp::Shr) => p.is_integer(),
            (Ty::Prim(p), _) => p.is_numeric(),
            _ => false,
        };
        if !ok {
            self.binary_op_error(op, &resolved, span);
        }
        ok
    }

    fn binary_op_error(&mut self, op: BinOp, ty: &Ty, span: Span) {
        let s = self.ty_str(ty);
        self.error(
            Diagnostic::error(
                ErrorCode::INVALID_BINARY_OP,
                format!(
                    "binary operation `{}` cannot be applied to type `{s}`",
                    op.as_str()
                ),
            )
            .with_label(
                span,
                format!("no implementation for `{s} {} {s}`", op.as_str()),
            ),
        );
    }

    /// Checks an assignment target and returns its type.
    fn check_place(&mut self, expr: &Expr) -> Ty {
        let target = strip_parens(expr);
        let is_place = match &target.kind {
            ExprKind::Path(_) => matches!(self.resolve.res(target.id), Res::Local(_) | Res::Err),
            ExprKind::Field(..) | ExprKind::Deref(_) | ExprKind::Index(..) => true,
            _ => false,
        };
        let ty = if is_place || !matches!(target.kind, ExprKind::Path(_)) {
            self.check_expr(expr, None)
        } else {
            // An item path: evaluating it would only add "cannot be used as a value".
            self.record(expr, Ty::Error)
        };
        if !is_place {
            self.error(
                Diagnostic::error(
                    ErrorCode::INVALID_ASSIGN_TARGET,
                    "invalid left-hand side of assignment",
                )
                .with_label(expr.span, "cannot assign to this expression"),
            );
            return Ty::Error;
        }
        ty
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Ty {
        let target = strip_parens(callee);
        let callee_ty = if let ExprKind::Path(_) = &target.kind {
            let t = self.check_path(target, true);
            self.record(target, t.clone());
            if !std::ptr::eq(target, callee) {
                self.record(callee, t.clone());
            }
            t
        } else {
            self.check_expr(callee, None)
        };
        let def = match self.shallow(&callee_ty) {
            Ty::FnDef(d) => d,
            Ty::Error => {
                for a in args {
                    self.check_expr(a, None);
                }
                return Ty::Error;
            }
            other => {
                let s = self.ty_str(&other);
                self.error(
                    Diagnostic::error(
                        ErrorCode::NOT_CALLABLE,
                        format!("expected function, found `{s}`"),
                    )
                    .with_label(callee.span, "call expression requires function"),
                );
                for a in args {
                    self.check_expr(a, None);
                }
                return Ty::Error;
            }
        };
        if let Some(builtin @ (Builtin::Print | Builtin::PrintLine)) = self.items.builtin(def) {
            return self.check_print_call(builtin, args, span);
        }
        let sig = self.items.fn_sig(def).clone();
        if sig.inputs.len() != args.len() {
            let d = Self::wrong_arg_count(sig.inputs.len(), args.len(), span);
            let name = self.resolve.def(def).name.clone();
            // A builtin has no source to point at.
            let d = if sig.name_span.is_dummy() {
                d
            } else {
                d.with_label(sig.name_span, format!("function `{name}` defined here"))
            };
            self.error(d);
        }
        for (i, a) in args.iter().enumerate() {
            let expected = sig.inputs.get(i).cloned();
            self.check_expr(a, expected.as_ref());
        }
        if sig.output.is_never() {
            self.diverges = true;
        }
        sig.output
    }

    fn wrong_arg_count(expected: usize, found: usize, span: Span) -> Diagnostic {
        let plural = |n: usize, s: &str| {
            if n == 1 {
                s.to_owned()
            } else {
                format!("{s}s")
            }
        };
        Diagnostic::error(
            ErrorCode::WRONG_ARG_COUNT,
            format!(
                "this function takes {expected} {} but {found} {} supplied",
                plural(expected, "argument"),
                if found == 1 {
                    "argument was"
                } else {
                    "arguments were"
                }
            ),
        )
        .with_label(
            span,
            format!("expected {expected} {}", plural(expected, "argument")),
        )
    }

    /// `Print(x)` / `PrintLine(x)`: one argument of any printable type (see
    /// [`collect::is_printable`]); an unsuffixed literal takes its default later. Both
    /// return `()`.
    fn check_print_call(&mut self, builtin: Builtin, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            let d = Self::wrong_arg_count(1, args.len(), span);
            self.error(d);
        }
        for arg in args {
            let ty = self.check_expr(arg, None);
            let ty = self.infer.resolve(&ty);
            let known = match &ty {
                Ty::Infer(v) => self.infer.kind(*v) != VarKind::General,
                other => !other.has_infer(),
            };
            if !known {
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(
                        arg.span,
                        format!(
                            "cannot infer the type of this argument to `{}`",
                            builtin.name()
                        ),
                    ),
                );
                // The local behind it is the same problem, not a new one.
                self.infer.bind_error(&ty);
                continue;
            }
            if matches!(ty, Ty::Infer(_) | Ty::Error | Ty::Never) || collect::is_printable(&ty) {
                continue;
            }
            let s = self.ty_str(&ty);
            self.error(
                Diagnostic::error(
                    ErrorCode::TYPE_MISMATCH,
                    format!("`{}` cannot print a value of type `{s}`", builtin.name()),
                )
                .with_label(arg.span, format!("expected a printable value, found `{s}`"))
                .with_note("printable types are integers, floats, `bool`, `char` and `&str`"),
            );
        }
        Ty::Unit
    }

    fn check_field(&mut self, expr: &Expr, base_ty: &Ty, name: &str, name_span: Span) -> Ty {
        let mut ty = self.shallow(base_ty);
        let mut autoderefs = 0;
        while let Ty::Ref(_, inner) = ty {
            ty = self.shallow(&inner);
            autoderefs += 1;
        }
        match ty {
            Ty::Adt(d) => {
                let adt = self.items.adt(d);
                if let Some(index) = adt.field_index(name) {
                    self.field_accesses
                        .insert(expr.id, FieldAccess { index, autoderefs });
                    return adt.fields[index].ty.clone();
                }
                let available: Vec<String> =
                    adt.fields.iter().map(|f| format!("`{}`", f.name)).collect();
                let adt_name = adt.name.clone();
                let mut d = Diagnostic::error(
                    ErrorCode::UNKNOWN_FIELD,
                    format!("no field `{name}` on type `{adt_name}`"),
                )
                .with_label(name_span, "unknown field");
                if !available.is_empty() {
                    d = d.with_note(format!("available fields are: {}", available.join(", ")));
                }
                self.error(d);
                Ty::Error
            }
            Ty::Error => Ty::Error,
            // `!` has no fields (rustc's E0609); `loop {}.Y` must not reach MIR building
            // without a recorded field access.
            other => {
                let s = self.ty_str(&other);
                self.error(
                    Diagnostic::error(
                        ErrorCode::UNKNOWN_FIELD,
                        format!("no field `{name}` on type `{s}`"),
                    )
                    .with_label(name_span, "unknown field"),
                );
                Ty::Error
            }
        }
    }

    fn check_if(
        &mut self,
        cond: &Expr,
        then: &Block,
        els: Option<&Expr>,
        expected: Option<&Ty>,
        span: Span,
    ) -> Ty {
        self.check_expr(cond, Some(&Ty::BOOL));
        let cond_diverges = self.diverges;
        let Some(els) = els else {
            let needs_value = expected
                .map(|e| self.shallow(e))
                .is_some_and(|e| !matches!(e, Ty::Unit | Ty::Error | Ty::Infer(_)));
            if needs_value {
                // Check the branch against what the `if` must produce, so the real
                // problem (no `else`) is the only one reported.
                self.check_block(then, expected);
                let e = self.ty_str(expected.expect("checked above"));
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_MISMATCH,
                        "`if` may be missing an `else` clause",
                    )
                    .with_label(span, format!("expected `{e}`, found `()`"))
                    .with_note("`if` expressions without `else` evaluate to `()`"),
                );
            } else {
                self.check_block(then, Some(&Ty::Unit));
            }
            self.diverges = cond_diverges;
            return Ty::Unit;
        };
        let then_ty = self.check_block(then, expected);
        let then_diverges = self.diverges;
        self.diverges = cond_diverges;
        let els_ty = self.check_expr(els, expected);
        let els_diverges = self.diverges;
        self.diverges = cond_diverges || (then_diverges && els_diverges);
        if expected.is_some() {
            // Both branches were already coerced to the expectation.
            return match (self.shallow(&then_ty), self.shallow(&els_ty)) {
                (Ty::Never, e) => e,
                (t, _) => t,
            };
        }
        let then_r = self.shallow(&then_ty);
        let els_r = self.shallow(&els_ty);
        match (&then_r, &els_r) {
            (Ty::Never, _) => els_ty,
            (_, Ty::Never) => then_ty,
            _ => {
                if self.try_coerce(&els_r, &then_r).is_err() {
                    let (t, e) = (self.ty_str(&then_r), self.ty_str(&els_r));
                    self.error(
                        Diagnostic::error(
                            ErrorCode::TYPE_MISMATCH,
                            "`if` and `else` have incompatible types",
                        )
                        .with_label(els.span, format!("expected `{t}`, found `{e}`"))
                        .with_label(span, "`if` and `else` have incompatible types"),
                    );
                    return Ty::Error;
                }
                then_ty
            }
        }
    }

    fn check_break(&mut self, value: Option<&Expr>, span: Span) {
        let Some(depth) = self.loops.len().checked_sub(1) else {
            if let Some(v) = value {
                self.check_expr(v, None);
            }
            self.error(
                Diagnostic::error(ErrorCode::BREAK_OUTSIDE_LOOP, "`break` outside of a loop")
                    .with_label(span, "cannot `break` outside of a loop"),
            );
            return;
        };
        if !self.loops[depth].is_loop {
            if let Some(v) = value {
                self.check_expr(v, None);
                self.error(
                    Diagnostic::error(
                        ErrorCode::BREAK_WITH_VALUE_OUTSIDE_LOOP,
                        "can only break with a value inside `loop`",
                    )
                    .with_label(span, "can only break with a value inside `loop`")
                    .with_note("`while` loops evaluate to `()`; use `loop` with `break value`"),
                );
            }
            self.loops[depth].has_break = true;
            return;
        }
        self.loops[depth].has_break = true;
        let expected = self.loops[depth]
            .break_ty
            .clone()
            .or_else(|| self.loops[depth].expected.clone());
        let ty = if let Some(v) = value {
            self.check_expr(v, expected.as_ref())
        } else {
            if let Some(e) = &expected {
                self.coerce(span, &Ty::Unit, e);
            }
            Ty::Unit
        };
        if self.loops[depth].break_ty.is_none() {
            let ty = if self.shallow(&ty).is_never() {
                expected.unwrap_or_else(|| self.infer.new_var(VarKind::General))
            } else {
                expected.unwrap_or(ty)
            };
            self.loops[depth].break_ty = Some(ty);
        }
    }

    fn check_struct_literal(&mut self, expr: &Expr, fields: &[sabiko_ast::FieldInit]) -> Ty {
        let ExprKind::Struct { path, .. } = &expr.kind else {
            unreachable!()
        };
        let Res::Def(def) = self.resolve.res(expr.id) else {
            for f in fields {
                self.check_expr(&f.value, None);
            }
            return Ty::Error;
        };
        let adt = self.items.adt(def).clone();
        let mut seen: Vec<Option<Span>> = vec![None; adt.fields.len()];
        for f in fields {
            if let Some(i) = adt.field_index(&f.name.name) {
                if let Some(prev) = seen[i] {
                    self.error(
                        Diagnostic::error(
                            ErrorCode::DUPLICATE_FIELD_INIT,
                            format!("field `{}` specified more than once", f.name.name),
                        )
                        .with_label(f.name.span, "used more than once")
                        .with_label(prev, "first use of the field"),
                    );
                }
                seen[i] = Some(f.name.span);
                let ty = adt.fields[i].ty.clone();
                self.check_expr(&f.value, Some(&ty));
            } else {
                self.check_expr(&f.value, None);
                let mut d = Diagnostic::error(
                    ErrorCode::UNKNOWN_FIELD,
                    format!("struct `{}` has no field named `{}`", adt.name, f.name.name),
                )
                .with_label(
                    f.name.span,
                    format!("`{}` does not have this field", adt.name),
                );
                if !adt.fields.is_empty() {
                    let available: Vec<String> =
                        adt.fields.iter().map(|f| format!("`{}`", f.name)).collect();
                    d = d.with_note(format!("available fields are: {}", available.join(", ")));
                }
                self.error(d);
            }
        }
        let missing: Vec<String> = adt
            .fields
            .iter()
            .zip(&seen)
            .filter(|(_, s)| s.is_none())
            .map(|(f, _)| format!("`{}`", f.name))
            .collect();
        if !missing.is_empty() {
            let list = match missing.len() {
                1 => missing[0].clone(),
                _ => format!(
                    "{} and {}",
                    missing[..missing.len() - 1].join(", "),
                    missing[missing.len() - 1]
                ),
            };
            self.error(
                Diagnostic::error(
                    ErrorCode::MISSING_FIELDS,
                    format!(
                        "missing {} {list} in initializer of `{}`",
                        if missing.len() == 1 {
                            "field"
                        } else {
                            "fields"
                        },
                        adt.name
                    ),
                )
                .with_label(path.span, format!("missing {list}")),
            );
        }
        Ty::Adt(def)
    }

    // ----- finalisation -----

    fn finish(mut self) -> BodyTypes {
        // Never-type fallback: `let x = return;` leaves `x` unconstrained; it becomes `()`.
        for v in std::mem::take(&mut self.diverging_vars) {
            if let Ty::Infer(_) = self.infer.resolve(&v) {
                let _ = self.infer.unify(&v, &Ty::Unit);
            }
        }
        // Report locals whose type is still unknown, then apply literal defaults.
        let mut local_types = HashMap::new();
        let mut locals: Vec<(LocalId, Ty)> =
            std::mem::take(&mut self.local_types).into_iter().collect();
        locals.sort_by_key(|(l, _)| *l);
        for (id, ty) in locals {
            let mut t = self.infer.resolve_with_defaults(&ty);
            if t.has_infer() {
                let name = self.resolve.local(id).name.clone();
                let span = self.local_spans[&id];
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(span, format!("consider giving `{name}` an explicit type")),
                );
                // Other locals sharing the variable are the same problem, not a new one.
                self.infer.bind_error(&t);
                t = Ty::Error;
            }
            local_types.insert(id, t);
        }
        let mut expr_types = HashMap::new();
        for (id, ty) in std::mem::take(&mut self.expr_types) {
            let mut t = self.infer.resolve_with_defaults(&ty);
            // Anything still unknown involves an unknown local (reported above).
            if t.has_infer() {
                t = Ty::Error;
            }
            expr_types.insert(id, t);
        }
        for (span, ty) in std::mem::take(&mut self.pending_negs) {
            let t = self.infer.resolve_with_defaults(&ty);
            if let Ty::Prim(p) = t {
                if p.is_unsigned_int() {
                    self.error(
                        Diagnostic::error(
                            ErrorCode::INVALID_UNARY_OP,
                            format!("cannot apply unary operator `-` to type `{}`", p.name()),
                        )
                        .with_label(span, "cannot apply unary operator `-`")
                        .with_note("unsigned values cannot be negated"),
                    );
                }
            }
        }

        let lits = std::mem::take(&mut self.int_lits);
        for lit in lits {
            let ty = self.infer.resolve_with_defaults(&lit.ty);
            if let Ty::Prim(p) = ty {
                self.check_int_range(&lit, p);
            }
        }
        let casts = std::mem::take(&mut self.casts);
        for cast in casts {
            let from = self.infer.resolve_with_defaults(&cast.from);
            let to = self.infer.resolve_with_defaults(&cast.to);
            self.check_cast(cast.span, &from, &to);
        }

        let has_errors = self.sink.error_count() > self.errors_before;
        BodyTypes {
            expr_types,
            local_types,
            field_accesses: self.field_accesses,
            has_errors,
        }
    }

    fn check_int_range(&mut self, lit: &IntLit, p: PrimTy) {
        let Some(bits) = p.int_bits() else {
            return;
        };
        let _ = bits;
        if lit.negated && !p.is_signed_int() {
            // `-1u8`: the unary operator check already reported.
            return;
        }
        let (min, max) = crate::consts::int_bounds(p);
        let fits = if lit.negated {
            // `-v` must be >= min, i.e. v <= |min|.
            lit.value <= min.unsigned_abs()
        } else {
            lit.value <= max
        };
        if !fits {
            let shown = if lit.negated {
                format!("-{}", lit.value)
            } else {
                lit.value.to_string()
            };
            self.error(
                Diagnostic::error(
                    ErrorCode::LITERAL_OUT_OF_RANGE,
                    format!("literal out of range for `{}`", p.name()),
                )
                .with_label(lit.span, format!("does not fit in `{}`", p.name()))
                .with_note(format!(
                    "the literal `{shown}` does not fit into the type `{}` whose range is `{min}..={max}`",
                    p.name()
                )),
            );
        }
    }

    fn check_cast(&mut self, span: Span, from: &Ty, to: &Ty) {
        if from.is_error() || to.is_error() || from.is_never() || from == to {
            return;
        }
        let ok = match (from, to) {
            (Ty::Prim(f), Ty::Prim(t)) => {
                (f.is_numeric() && t.is_numeric())
                    || (*f == PrimTy::Bool && t.is_integer())
                    || (*f == PrimTy::Char && t.is_integer())
                    || (*f == PrimTy::U8 && *t == PrimTy::Char)
            }
            _ => false,
        };
        if ok {
            return;
        }
        let (f, t) = (self.ty_str(from), self.ty_str(to));
        let d = match (from, to) {
            (Ty::Prim(PrimTy::Char), _) | (_, Ty::Prim(PrimTy::Char))
                if from.is_integer() || to.is_integer() =>
            {
                Diagnostic::error(
                    ErrorCode::INVALID_CAST,
                    format!("only `u8` can be cast as `char`, not `{f}`"),
                )
                .with_label(span, "invalid cast")
            }
            (Ty::Prim(_), Ty::Prim(_)) => Diagnostic::error(
                ErrorCode::INVALID_CAST,
                format!("casting `{f}` as `{t}` is invalid"),
            )
            .with_label(span, "invalid cast"),
            _ => Diagnostic::error(
                ErrorCode::INVALID_CAST,
                format!("non-primitive cast: `{f}` as `{t}`"),
            )
            .with_label(span, "invalid cast")
            .with_note("only primitive types can be cast with `as`"),
        };
        self.error(d);
    }
}

fn strip_parens(mut e: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    e
}

pub(crate) fn int_suffix_prim(s: sabiko_ast::IntSuffix) -> PrimTy {
    use sabiko_ast::IntSuffix as S;
    match s {
        S::I8 => PrimTy::I8,
        S::I16 => PrimTy::I16,
        S::I32 => PrimTy::I32,
        S::I64 => PrimTy::I64,
        S::I128 => PrimTy::I128,
        S::Isize => PrimTy::Isize,
        S::U8 => PrimTy::U8,
        S::U16 => PrimTy::U16,
        S::U32 => PrimTy::U32,
        S::U64 => PrimTy::U64,
        S::U128 => PrimTy::U128,
        S::Usize => PrimTy::Usize,
    }
}
