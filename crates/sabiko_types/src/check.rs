//! Type checking of function bodies and constant initialisers.
//!
//! Bidirectional: every expression is checked against an optional expected type, which
//! flows down from annotations, parameter types, return types and struct field types, and
//! unification flows information back up. The only coercions are `!` to anything and
//! `&mut T` to `&T`.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{
    BinOp, Block, Expr, ExprKind, FnDecl, Lit, NodeId, Pat, PatKind, Stmt, StmtKind, Type,
    TypeKind, UnOp,
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
    /// How many references a tuple pattern looked through before matching (Rust's default
    /// binding modes: `let (a, b) = &t;`), keyed by the `PatKind::Tuple` node; absent
    /// for none.
    pub pat_derefs: HashMap<NodeId, usize>,
    /// Identifier patterns that bind a reference to the matched place instead of its
    /// value (`ref`/`ref mut` by default binding mode), keyed by the `PatKind::Ident`
    /// node; absent for a by-value binding.
    pub ref_bindings: HashMap<NodeId, Mutability>,
    /// Tuple expressions checked against an expected tuple type of their arity: each
    /// element is a coercion site (a `&mut` place there is reborrowed, not moved), as in
    /// rustc. Keyed by the `ExprKind::Tuple` node.
    pub coerced_tuples: HashSet<NodeId>,
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
    pat_derefs: HashMap<NodeId, usize>,
    ref_bindings: HashMap<NodeId, Mutability>,
    coerced_tuples: HashSet<NodeId>,
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
    /// Comparisons whose operand type was still an unknown variable: judged in `finish`.
    pending_comparisons: Vec<(BinOp, Ty, Span)>,
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
            pat_derefs: HashMap::new(),
            ref_bindings: HashMap::new(),
            coerced_tuples: HashSet::new(),
            ret_ty,
            loops: Vec::new(),
            diverges: false,
            negating: false,
            int_lits: Vec::new(),
            casts: Vec::new(),
            pending_negs: Vec::new(),
            diverging_vars: Vec::new(),
            pending_comparisons: Vec::new(),
            errors_before,
        }
    }

    pub(crate) fn check_fn(mut self, f: &FnDecl, def: DefId) -> BodyTypes {
        let sig = self.items.fn_sig(def);
        for (p, ty) in f.params.iter().zip(sig.inputs.clone()) {
            self.check_pat(&p.pat, &ty, None);
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
            Ty::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(|e| self.ty_str(e)).collect();
                if parts.len() == 1 {
                    format!("({},)", parts[0])
                } else {
                    format!("({})", parts.join(", "))
                }
            }
            t => t.display(self.resolve),
        }
    }

    fn error(&mut self, d: Diagnostic) {
        self.sink.emit(d);
    }

    fn mismatch(&mut self, span: Span, expected: &Ty, found: &Ty) {
        let (e, f) = (self.ty_str(expected), self.ty_str(found));
        let mut d = Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
            .with_label(span, format!("expected `{e}`, found `{f}`"));
        if let (Ty::Tuple(ee), Ty::Tuple(fe)) = (self.shallow(expected), self.shallow(found)) {
            if ee.len() != fe.len() {
                d = d.with_note(format!(
                    "expected a tuple with {}, found one with {}",
                    elements(ee.len()),
                    elements(fe.len())
                ));
            }
        }
        self.error(d);
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
            if self.infer.is_cyclic(found, expected) {
                let (e, f) = (self.ty_str(expected), self.ty_str(found));
                self.error(
                    Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
                        .with_label(span, format!("expected `{e}`, found `{f}`"))
                        .with_note("cyclic type of infinite size"),
                );
                // The variable stays open (`let r; r = &r;`); that is the same problem.
                self.infer.bind_error(found);
                self.infer.bind_error(expected);
            } else {
                self.mismatch(span, expected, found);
            }
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
            TypeKind::Tuple(elems) => Ty::Tuple(
                elems
                    .iter()
                    .map(|t| self.lower_type_unchecked(t))
                    .collect::<Vec<_>>()
                    .into(),
            ),
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
            // The tail's type even when an earlier statement diverges, as in rustc:
            // `{ return; 2i32 }` is `i32`, so `if c { 1u8 } else { return; 2i32 }` is a
            // mismatch, and the `else` of `if c { &a } else { &return 1 }` is `&!`.
            Some(tail) => self.check_expr(tail, expected),
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
                let init_span = l.init.as_ref().map(|e| e.span);
                self.check_pat(&l.pat, &ty, init_span);
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
            ExprKind::Tuple(elems) => self.check_tuple(expr.id, elems, expected),
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
                if matches!(op, BinOp::Shl | BinOp::Shr) {
                    self.check_shift_operands(*op, &lhs_ty, &rhs_ty, expr.span);
                } else {
                    self.check_arith_operands(*op, &lhs_ty, &rhs_ty, expr.span);
                }
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
                    // Only a diverging operand fits `&!`; checking against `!` would
                    // report the operand rather than the reference (rustc: "expected
                    // `&!`, found `&{integer}`").
                    Some(Ty::Ref(_, t)) if !t.is_never() => Some(*t),
                    _ => None,
                };
                let errors_before = self.sink.error_count();
                // A diverging operand gives `&!`, as in rustc: the borrow is not a
                // coercion site, so `let q: &Point = &loop {};` is a mismatch, while
                // `*&return 9` is `!` again. The code after it is unreachable.
                let inner_ty = self.check_expr(inner, inner_expected.as_ref());
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
                // An unsuffixed literal operand takes the target type when the families
                // agree (`300 as u8` is out of range), like rustc's castable expectation;
                // `unify` refuses a mismatched family. A variable keeps its own type.
                if matches!((&from_s, &to_s), (Ty::Infer(_), Ty::Prim(_))) && is_cast_literal(inner)
                {
                    let _ = self.infer.unify(&from_s, &to_s);
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

    /// `(a, b, ...)`. With an expected tuple type of the same arity, each element is
    /// checked (and coerced) against its expected element type, which becomes the
    /// element's type, so the tuple as a whole matches the expectation (rustc's
    /// expectation flows into tuple elements the same way). An element that diverges
    /// without an expectation gets a fresh variable that falls back to `()`, as a
    /// diverging `let` initialiser does: `!` itself never becomes an element type.
    fn check_tuple(&mut self, id: NodeId, elems: &[Expr], expected: Option<&Ty>) -> Ty {
        let expected_elems = match expected.map(|e| self.shallow(e)) {
            Some(Ty::Tuple(tys)) if tys.len() == elems.len() => Some(tys),
            _ => None,
        };
        if expected_elems.is_some() {
            self.coerced_tuples.insert(id);
        }
        let mut tys = Vec::with_capacity(elems.len());
        for (i, e) in elems.iter().enumerate() {
            let exp = expected_elems.as_ref().map(|tys| tys[i].clone());
            let found = self.check_expr(e, exp.as_ref());
            let ty = match exp {
                Some(exp) => exp,
                None if self.shallow(&found).is_never() => {
                    let v = self.infer.new_var(VarKind::General);
                    self.diverging_vars.push(v.clone());
                    v
                }
                None => found,
            };
            tys.push(ty);
        }
        Ty::Tuple(tys.into())
    }

    /// Checks a binding pattern against the type of the value it binds, recording the
    /// type of every local it introduces. `init_span` points at the initialiser, for
    /// diagnostics. A tuple pattern matched against a reference to a tuple looks through
    /// the reference and binds references to the elements (Rust 2021's default binding
    /// modes: `let (a, b) = &t;` gives `a: &A`); a `mut x` binding in that mode binds by
    /// value again, as in Rust 2021.
    fn check_pat(&mut self, pat: &Pat, ty: &Ty, init_span: Option<Span>) {
        self.check_pat_in_mode(pat, ty, None, init_span);
    }

    fn check_pat_in_mode(
        &mut self,
        pat: &Pat,
        ty: &Ty,
        by_ref: Option<Mutability>,
        init_span: Option<Span>,
    ) {
        match &pat.kind {
            PatKind::Ident { mutable, .. } => {
                let Some(local) = self.resolve.local_of_pat(pat.id) else {
                    return;
                };
                let binding_ty = match by_ref {
                    Some(m) if !mutable => {
                        self.ref_bindings.insert(pat.id, m);
                        Ty::Ref(m, Box::new(ty.clone()))
                    }
                    _ => ty.clone(),
                };
                self.local_types.insert(local, binding_ty);
                self.local_spans.insert(local, pat.span);
            }
            PatKind::Wild => {}
            PatKind::Tuple(elems) => {
                let mut ty = self.shallow(ty);
                let mut by_ref = by_ref;
                let mut derefs = 0;
                while let Ty::Ref(m, inner) = ty {
                    // `&mut` keeps `ref mut` unless an outer `&` already made it `ref`.
                    by_ref = Some(match by_ref {
                        Some(Mutability::Not) => Mutability::Not,
                        _ => m,
                    });
                    derefs += 1;
                    ty = self.shallow(&inner);
                }
                if derefs > 0 {
                    self.pat_derefs.insert(pat.id, derefs);
                }
                let elem_tys = match ty {
                    Ty::Tuple(tys) if tys.len() == elems.len() => *tys,
                    Ty::Unit if elems.is_empty() => Vec::new(),
                    Ty::Error => vec![Ty::Error; elems.len()],
                    Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                        let tys: Vec<Ty> = elems
                            .iter()
                            .map(|_| self.infer.new_var(VarKind::General))
                            .collect();
                        let tuple = if tys.is_empty() {
                            Ty::Unit
                        } else {
                            Ty::Tuple(tys.clone().into())
                        };
                        let _ = self.infer.unify(&Ty::Infer(v), &tuple);
                        tys
                    }
                    other => {
                        self.pat_mismatch(pat, elems.len(), &other, init_span);
                        vec![Ty::Error; elems.len()]
                    }
                };
                for (p, t) in elems.iter().zip(&elem_tys) {
                    self.check_pat_in_mode(p, t, by_ref, init_span);
                }
            }
        }
    }

    /// A tuple pattern of `arity` elements against a value of type `found`.
    fn pat_mismatch(&mut self, pat: &Pat, arity: usize, found: &Ty, init_span: Option<Span>) {
        let f = self.ty_str(found);
        let pat_ty = if arity == 0 {
            "`()`".to_owned()
        } else {
            format!("a tuple with {}", elements(arity))
        };
        let mut d = Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
            .with_label(pat.span, format!("expected `{f}`, found {pat_ty}"));
        if let Ty::Tuple(tys) = found {
            d = d.with_note(format!(
                "expected a tuple with {}, found one with {}",
                elements(tys.len()),
                elements(arity)
            ));
        }
        if let Some(span) = init_span {
            d = d.with_label(span, format!("this expression has type `{f}`"));
        }
        self.error(d);
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
            Lit::Char(c) => {
                if u32::from(*c) > 0xFFFF {
                    self.non_bmp_char(*c, span);
                }
                Ty::CHAR
            }
            Lit::Str(_) => Ty::shared_ref(Ty::Prim(PrimTy::Str)),
        }
    }

    /// A `char` is one UTF-16 code unit (decision 31), so a literal outside the Basic
    /// Multilingual Plane, which needs a surrogate pair, has no `char` value. A deliberate
    /// divergence from Rust, where `char` is a Unicode scalar value.
    fn non_bmp_char(&mut self, c: char, span: Span) {
        let code = u32::from(c);
        self.error(
            Diagnostic::error(
                ErrorCode::NON_BMP_CHAR,
                format!("character literal `U+{code:04X}` does not fit in a `char`"),
            )
            .with_label(span, "outside the Basic Multilingual Plane")
            .with_note(
                "a Sabiko `char` is one UTF-16 code unit (U+0000 to U+FFFF); this character needs two",
            )
            .with_note(format!(
                "use a string literal instead: `\"\\u{{{code:X}}}\"`"
            )),
        );
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
                if let Ty::Ref(..) = self.shallow(&l) {
                    self.check_ref_comparison(op, &l, rhs, span);
                    return Ty::BOOL;
                }
                let errors_before = self.sink.error_count();
                self.check_expr(rhs, Some(&l));
                let resolved = self.shallow(&l);
                if matches!(resolved, Ty::Tuple(_)) && self.sink.error_count() > errors_before {
                    // `(1, 2) == (1, 2, 3)`: the mismatch is the problem worth reporting.
                    return Ty::BOOL;
                }
                if let Ty::Infer(v) = resolved {
                    if self.infer.kind(v) == VarKind::General {
                        // Judged once the variable is known (`finish`).
                        self.pending_comparisons.push((op, l.clone(), span));
                    }
                } else if !is_comparable(&resolved) {
                    self.binary_op_error(op, &resolved, span);
                }
                Ty::BOOL
            }
            BinOp::Shl | BinOp::Shr => {
                let l = self.check_expr(lhs, expected);
                let r = self.check_expr(rhs, None);
                if self.check_shift_operands(op, &l, &r, span) {
                    l
                } else {
                    Ty::Error
                }
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

    /// `lhs op rhs` where the left operand has reference type `l`. As in Rust, whose
    /// `PartialEq`/`PartialOrd` impls for `&A` compare the referents, both operands must be
    /// references of the same depth to a comparable type; MIR building dereferences them.
    /// `==` and `!=` accept any mix of `&` and `&mut` at every level (`impl PartialEq<&mut
    /// B> for &A` and its siblings). The ordering operators have only `impl PartialOrd<&B>
    /// for &A` and `impl PartialOrd<&mut B> for &mut A`: rustc infers the right operand's
    /// outer reference from the left one and coerces to it, so a `&mut` right operand of a
    /// `&` left one is accepted, but every inner level must match exactly.
    fn check_ref_comparison(&mut self, op: BinOp, lhs_ty: &Ty, rhs: &Expr, span: Span) {
        let rhs_ty = self.check_expr(rhs, None);
        let r_res = self.shallow(&rhs_ty);
        match r_res {
            Ty::Never | Ty::Error => return,
            Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                self.coerce(rhs.span, &rhs_ty, lhs_ty);
                return;
            }
            _ => {}
        }
        let ordering = !matches!(op, BinOp::Eq | BinOp::Ne);
        let (mut left, mut right) = (self.shallow(lhs_ty), r_res);
        let mut outer = true;
        let mut mutability_ok = true;
        while let (Ty::Ref(ma, ia), Ty::Ref(mb, ib)) = (&left, &right) {
            if ordering && ma != mb && !(outer && *ma == Mutability::Not) {
                mutability_ok = false;
            }
            outer = false;
            let (na, nb) = (self.shallow(ia), self.shallow(ib));
            left = na;
            right = nb;
        }
        // An unknown referent (`let x; .. &x == r`) may still become a reference.
        let open = |this: &mut Self, ty: &Ty| matches!(ty, Ty::Infer(v) if this.infer.kind(*v) == VarKind::General);
        let depth_ok = !(matches!(left, Ty::Ref(..)) || matches!(right, Ty::Ref(..)))
            || open(self, &left)
            || open(self, &right);
        if outer || !depth_ok || !mutability_ok {
            // Different depths (`&i32 == i32`), or a mutability `PartialOrd` lacks.
            let (ls, rs) = (self.ty_str(lhs_ty), self.ty_str(&rhs_ty));
            let mut diag = Diagnostic::error(
                ErrorCode::INVALID_BINARY_OP,
                format!("can't compare `{ls}` with `{rs}`"),
            )
            .with_label(
                span,
                format!("no implementation for `{ls} {} {rs}`", op.as_str()),
            );
            if depth_ok && !outer {
                diag = diag.with_note(
                    "`<`, `<=`, `>` and `>=` need references of the same mutability, except that an outermost `&mut` on the right weakens to `&`",
                );
            }
            self.error(diag);
            return;
        }
        if self.infer.unify(&left, &right).is_err() {
            self.mismatch(rhs.span, lhs_ty, &rhs_ty);
            return;
        }
        // An open side unified with a reference above: peel what it became.
        let mut referent = self.shallow(&left);
        while let Ty::Ref(_, inner) = referent {
            referent = self.shallow(&inner);
        }
        if let Ty::Infer(v) = referent {
            if self.infer.kind(v) == VarKind::General {
                self.pending_comparisons.push((op, lhs_ty.clone(), span));
            }
        } else if !is_comparable(&referent) {
            let resolved = self.infer.resolve(lhs_ty);
            self.binary_op_error(op, &resolved, span);
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

    /// Checks that both operands of `<<` or `>>` (or their compound forms) are integers,
    /// each of its own type. Returns whether the left one is, so a bad right operand
    /// still leaves the result typed.
    fn check_shift_operands(&mut self, op: BinOp, lhs: &Ty, rhs: &Ty, span: Span) -> bool {
        let lr = self.shallow(lhs);
        let rr = self.shallow(rhs);
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
        }
        l_ok
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
            (Ty::Prim(p), _) => p.is_numeric(),
            _ => false,
        };
        if !ok {
            self.binary_op_error(op, &resolved, span);
        }
        ok
    }

    fn binary_op_error(&mut self, op: BinOp, ty: &Ty, span: Span) {
        let mut referent = self.shallow(ty);
        while let Ty::Ref(_, inner) = referent {
            referent = self.shallow(&inner);
        }
        if op.is_comparison() && matches!(referent, Ty::Tuple(_)) {
            // Valid Rust (tuples of comparable elements are `PartialEq`/`PartialOrd`), but
            // the comparison traits arrive in Phase 6, as for structs.
            self.error(
                Diagnostic::error(
                    ErrorCode::UNSUPPORTED_SYNTAX,
                    "not yet supported: comparing tuples",
                )
                .with_label(span, format!("`{}` on tuples is not yet supported", op.as_str()))
                .with_note(
                    "tuple comparison arrives with the comparison traits in Phase 6; see docs/PLAN.md. Compare the elements one by one instead",
                ),
            );
            return;
        }
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
        if let ExprKind::Tuple(elems) = &target.kind {
            // `(a, b) = (b, a)` is valid Rust (destructuring assignment) but not Sabiko yet.
            for e in elems {
                self.check_expr(e, None);
            }
            self.error(
                Diagnostic::error(
                    ErrorCode::UNSUPPORTED_SYNTAX,
                    "not yet supported: destructuring assignment",
                )
                .with_label(expr.span, "a tuple on the left of `=` is not yet supported")
                .with_note(
                    "assign each element (`t.0 = ...`), or bind new names with `let (a, b) = ...;`",
                ),
            );
            return self.record(expr, Ty::Error);
        }
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
            let known = self.has_no_general_var(&ty);
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
            // `&loop {}` prints like `loop {}` (rustc: `!: Display`); neither runs.
            if matches!(ty, Ty::Infer(_) | Ty::Error)
                || ty.is_uninhabited()
                || collect::is_printable(&ty)
            {
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

    /// Whether `ty` (resolved) holds no general inference variable: literal variables
    /// (`{integer}`, `{float}`) count as known.
    fn has_no_general_var(&mut self, ty: &Ty) -> bool {
        match ty {
            Ty::Infer(v) => self.infer.kind(*v) != VarKind::General,
            Ty::Ref(_, inner) => self.has_no_general_var(inner),
            Ty::Tuple(elems) => elems.iter().all(|e| self.has_no_general_var(e)),
            _ => true,
        }
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
                // The type as written, references included (`&P`), as rustc reports it.
                let shown = self.ty_str(base_ty);
                let mut d = Diagnostic::error(
                    ErrorCode::UNKNOWN_FIELD,
                    format!("no field `{name}` on type `{shown}`"),
                )
                .with_label(name_span, "unknown field");
                if !available.is_empty() {
                    d = d.with_note(format!("available fields are: {}", available.join(", ")));
                }
                self.error(d);
                Ty::Error
            }
            Ty::Tuple(elems) => {
                // Only a plain decimal index names an element: `t.01` and `t.0x1` do not.
                let index = name
                    .parse::<usize>()
                    .ok()
                    .filter(|i| i.to_string() == name && *i < elems.len());
                if let Some(index) = index {
                    self.field_accesses
                        .insert(expr.id, FieldAccess { index, autoderefs });
                    return elems[index].clone();
                }
                let s = self.ty_str(base_ty);
                let n = elems.len();
                self.error(
                    Diagnostic::error(
                        ErrorCode::UNKNOWN_FIELD,
                        format!("no field `{name}` on type `{s}`"),
                    )
                    .with_label(name_span, "unknown field")
                    .with_note(format!(
                        "the tuple has {}, `.0` to `.{}`",
                        elements(n),
                        n - 1
                    )),
                );
                Ty::Error
            }
            Ty::Error => Ty::Error,
            // `!` has no fields (rustc's E0609); `loop {}.Y` must not reach MIR building
            // without a recorded field access.
            _ => {
                let s = self.ty_str(base_ty);
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

        for (op, ty, span) in std::mem::take(&mut self.pending_comparisons) {
            let t = self.infer.resolve_with_defaults(&ty);
            let mut referent = &t;
            while let Ty::Ref(_, inner) = referent {
                referent = inner;
            }
            // An unknown type was reported with its local.
            if !t.has_infer() && !is_comparable(referent) {
                self.binary_op_error(op, &t, span);
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
            pat_derefs: self.pat_derefs,
            ref_bindings: self.ref_bindings,
            coerced_tuples: self.coerced_tuples,
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

/// Whether `==`, `<` and the other comparisons apply to values of type `ty` (resolved at
/// the top level): the primitives but `str`, and `()`.
fn is_comparable(ty: &Ty) -> bool {
    match ty {
        Ty::Error | Ty::Never | Ty::Infer(_) | Ty::Unit => true,
        Ty::Prim(p) => *p != PrimTy::Str,
        _ => false,
    }
}

/// Whether the cast target reaches a literal in `expr`, as rustc's expectation does:
/// through parentheses, `-`, `!` and a block's tail, but not into `if` branches.
fn is_cast_literal(expr: &Expr) -> bool {
    match &strip_parens(expr).kind {
        ExprKind::Lit(_) => true,
        ExprKind::Unary(_, inner) => is_cast_literal(inner),
        ExprKind::Block(b) => b.tail.as_deref().is_some_and(is_cast_literal),
        _ => false,
    }
}

fn strip_parens(mut e: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    e
}

/// `1 element`, `3 elements`.
fn elements(n: usize) -> String {
    if n == 1 {
        "1 element".to_owned()
    } else {
        format!("{n} elements")
    }
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
