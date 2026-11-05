//! Work on patterns and enums that needs constant values, so it runs after constant
//! evaluation: enum discriminants (decision 15), the values literal, constant and range
//! patterns compare with, range validity, and exhaustiveness and reachability
//! (`crate::usefulness`).

// Discriminants are `isize` values carried as `i128`.
#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use std::collections::HashMap;

use sabiko_ast::{
    EnumDecl, Expr, ExprKind, ItemKind, Lit, Pat, PatKind, RangeEnd, SourceFile, UnOp,
};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{DefId, DefKind, PrimTy, Res, ResolveResults};

use crate::check::{BodyTypes, PatConst};
use crate::collect::{ItemTypes, VariantKind};
use crate::consts::ConstValue;
use crate::ty::Ty;
use crate::usefulness;

/// Computes the discriminant of every variant: an explicit `= value` (evaluated as an
/// `isize` constant), or one more than the previous variant's, 0 for the first. Reports
/// duplicates (E0333, rustc's E0081), an implicit discriminant past `isize::MAX` (E0370)
/// and explicit discriminants on an enum with a field (E0732), which needs a `#[repr]`
/// Sabiko does not interpret.
pub(crate) fn discriminants(
    file: &SourceFile,
    resolve: &ResolveResults,
    consts: &HashMap<DefId, ConstValue>,
    items: &mut ItemTypes,
    sink: &mut DiagnosticSink,
) {
    for item in &file.items {
        let ItemKind::Enum(e) = &item.kind else {
            continue;
        };
        let def = resolve.def_of_item(item.id);
        let values = enum_discriminants(e, def, resolve, consts, items, sink);
        items.set_discriminants(def, &values);
    }
}

fn enum_discriminants(
    e: &EnumDecl,
    def: DefId,
    resolve: &ResolveResults,
    consts: &HashMap<DefId, ConstValue>,
    items: &ItemTypes,
    sink: &mut DiagnosticSink,
) -> Vec<i128> {
    let adt = items.adt(def);
    let has_fields = adt.variants().iter().any(|v| v.kind != VariantKind::Unit);
    let mut values: Vec<i128> = Vec::new();
    let mut spans = Vec::new();
    let mut prev: Option<i128> = None;
    for v in &e.variants {
        // A duplicate variant name has no definition (reported by the resolver).
        let Some(vdef) = crate::variant_def(resolve, def, v) else {
            continue;
        };
        let value = if let Some(expr) = &v.discriminant {
            if has_fields {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::INVALID_DISCRIMINANT,
                        "`#[repr(inttype)]` must be specified for enums with explicit discriminants and non-unit variants",
                    )
                    .with_label(expr.span, "explicit discriminant specified here")
                    .with_note("Sabiko does not interpret `#[repr]` yet: an enum with a field cannot have explicit discriminants"),
                );
            }
            match consts.get(&vdef) {
                Some(ConstValue::Int(i)) => *i,
                // Failed evaluation was reported; keep counting from something.
                _ => prev.map_or(0, |p| p.saturating_add(1)),
            }
        } else {
            match prev {
                None => 0,
                Some(p) if p == i128::from(i64::MAX) => {
                    sink.emit(
                        Diagnostic::error(
                            ErrorCode::INVALID_DISCRIMINANT,
                            "enum discriminant overflowed",
                        )
                        .with_label(v.name.span, format!("overflowed on value after {p}"))
                        .with_note(format!(
                            "explicitly set `{} = {}` if that is desired outcome",
                            v.name.name,
                            i64::MIN
                        )),
                    );
                    i128::from(i64::MIN)
                }
                Some(p) => p + 1,
            }
        };
        if let Some(first) = values.iter().position(|&x| x == value) {
            sink.emit(
                Diagnostic::error(
                    ErrorCode::INVALID_DISCRIMINANT,
                    format!("discriminant value `{value}` assigned more than once"),
                )
                .with_label(e.name.span, "discriminants must be unique")
                .with_label(spans[first], format!("`{value}` assigned here"))
                .with_label(v.span, format!("`{value}` assigned here")),
            );
        }
        values.push(value);
        spans.push(v.span);
        prev = Some(value);
    }
    values
}

/// Fills in [`BodyTypes::pat_consts`] for every function body, and reports empty or
/// reversed ranges (E0332, rustc's E0030 and E0579).
pub(crate) fn pattern_values(
    file: &SourceFile,
    resolve: &ResolveResults,
    consts: &HashMap<DefId, ConstValue>,
    bodies: &mut HashMap<DefId, BodyTypes>,
    sink: &mut DiagnosticSink,
) {
    for item in file.all_items() {
        let ItemKind::Fn(f) = &item.kind else {
            continue;
        };
        let def = resolve.def_of_item(item.id);
        let Some(body) = bodies.get_mut(&def) else {
            continue;
        };
        if body.has_errors {
            continue;
        }
        let mut cx = PatValues {
            resolve,
            consts,
            body,
            sink,
            failed: false,
        };
        for p in &f.params {
            cx.pat(&p.pat);
        }
        cx.block(&f.body);
        if cx.failed {
            body.has_errors = true;
        }
    }
}

struct PatValues<'a, 'b> {
    resolve: &'a ResolveResults,
    consts: &'a HashMap<DefId, ConstValue>,
    body: &'b mut BodyTypes,
    sink: &'a mut DiagnosticSink,
    failed: bool,
}

impl PatValues<'_, '_> {
    fn block(&mut self, b: &sabiko_ast::Block) {
        for s in &b.stmts {
            match &s.kind {
                sabiko_ast::StmtKind::Let(l) => {
                    if let Some(init) = &l.init {
                        self.expr(init);
                    }
                    if let Some(els) = &l.els {
                        self.block(els);
                    }
                    self.pat(&l.pat);
                }
                sabiko_ast::StmtKind::Expr { expr, .. } => self.expr(expr),
                sabiko_ast::StmtKind::Empty => {}
            }
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.pat(&arm.pat);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
            }
            ExprKind::Let(pat, scrutinee) => {
                self.expr(scrutinee);
                self.pat(pat);
            }
            _ => for_each_child(e, &mut |c| match c {
                Child::Expr(c) => self.expr(c),
                Child::Block(b) => self.block(b),
            }),
        }
    }

    fn pat(&mut self, pat: &Pat) {
        match &pat.kind {
            PatKind::Ident { sub, .. } => {
                if let Some(Res::Def(def)) = self.resolve.pat_res(pat.id) {
                    if self.resolve.def(def).kind == DefKind::Const {
                        match self.consts.get(&def) {
                            Some(v) => {
                                self.body
                                    .pat_consts
                                    .insert(pat.id, PatConst::Value(v.clone()));
                            }
                            // Reported with the constant.
                            None => self.failed = true,
                        }
                    }
                }
                if let Some(sub) = sub {
                    self.pat(sub);
                }
            }
            PatKind::Wild | PatKind::Rest | PatKind::Path(_) => {}
            PatKind::Tuple(ps) | PatKind::TupleStruct(_, ps) | PatKind::Or(ps) => {
                for p in ps {
                    self.pat(p);
                }
            }
            PatKind::Struct { fields, .. } => {
                for f in fields {
                    self.pat(&f.pat);
                }
            }
            PatKind::Lit(e) => {
                if let Some(v) = self.value(e) {
                    self.body.pat_consts.insert(pat.id, PatConst::Value(v));
                }
            }
            PatKind::Range(lo, hi, end) => {
                let (Some(l), Some(h)) = (self.value(lo), self.value(hi)) else {
                    return;
                };
                let inclusive = *end == RangeEnd::Included;
                let order = compare(&l, &h);
                if inclusive && order == Some(std::cmp::Ordering::Greater) {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::INVALID_RANGE_PATTERN,
                            "lower bound for range pattern must be less than or equal to upper bound",
                        )
                        .with_label(lo.span, "lower bound larger than upper bound"),
                    );
                    self.failed = true;
                } else if !inclusive && order != Some(std::cmp::Ordering::Less) {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::INVALID_RANGE_PATTERN,
                            "lower bound for range pattern must be less than upper bound",
                        )
                        .with_label(pat.span, "this range is empty"),
                    );
                    self.failed = true;
                }
                self.body.pat_consts.insert(
                    pat.id,
                    PatConst::Range {
                        lo: l,
                        hi: h,
                        inclusive,
                    },
                );
            }
        }
    }

    /// The value of a literal, a negated literal or a constant in a pattern, in the type
    /// the type checker gave it.
    fn value(&mut self, e: &Expr) -> Option<ConstValue> {
        let ty = self.body.expr_ty(e.id).clone();
        match &e.kind {
            ExprKind::Lit(lit) => Some(lit_value(lit, &ty, false)),
            ExprKind::Unary(UnOp::Neg, inner) => match &inner.kind {
                ExprKind::Lit(lit) => Some(lit_value(lit, &ty, true)),
                _ => None,
            },
            ExprKind::Path(_) => match self.resolve.res(e.id) {
                Res::Def(def) if self.resolve.def(def).kind == DefKind::Const => {
                    let v = self.consts.get(&def).cloned();
                    if v.is_none() {
                        self.failed = true;
                    }
                    v
                }
                Res::Local(_) => {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::NON_CONSTANT_EXPRESSION,
                            "runtime values cannot be referenced in patterns",
                        )
                        .with_label(e.span, "references a runtime value"),
                    );
                    self.failed = true;
                    None
                }
                // Reported by the resolver or the type checker.
                _ => {
                    self.failed = true;
                    None
                }
            },
            _ => None,
        }
    }
}

/// The value of a literal of type `ty`, negated (`-5`) when `negated`.
fn lit_value(lit: &Lit, ty: &Ty, negated: bool) -> ConstValue {
    match lit {
        Lit::Int { value, .. } => {
            let signed = ty.as_prim().is_some_and(PrimTy::is_signed_int);
            if signed {
                // `-128i8` and `-170141183460469231731687303715884105728i128`.
                let v = *value as i128;
                ConstValue::Int(if negated { v.wrapping_neg() } else { v })
            } else {
                ConstValue::Uint(*value)
            }
        }
        Lit::Float { text, .. } => {
            let f = sabiko_lexer::literal::parse_float(text).map_or(0.0, |f| f.value);
            let f = if negated { -f } else { f };
            ConstValue::Float(match ty.as_prim() {
                Some(PrimTy::F32) => f64::from(f as f32),
                _ => f,
            })
        }
        Lit::Bool(b) => ConstValue::Bool(*b),
        Lit::Char(c) => ConstValue::Char(*c),
        Lit::Str(s) => ConstValue::Str(s.clone()),
    }
}

fn compare(a: &ConstValue, b: &ConstValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (ConstValue::Int(x), ConstValue::Int(y)) => Some(x.cmp(y)),
        (ConstValue::Uint(x), ConstValue::Uint(y)) => Some(x.cmp(y)),
        (ConstValue::Char(x), ConstValue::Char(y)) => Some(x.cmp(y)),
        (ConstValue::Float(x), ConstValue::Float(y)) => x.partial_cmp(y),
        _ => None,
    }
}

/// A direct child of an expression.
pub(crate) enum Child<'a> {
    Expr(&'a Expr),
    Block(&'a sabiko_ast::Block),
}

/// Calls `f` for every direct subexpression and block of `e`, the guards and bodies of a
/// `match` included, but no pattern.
pub(crate) fn for_each_child<'a>(e: &'a Expr, f: &mut impl FnMut(Child<'a>)) {
    let mut expr = |x: &'a Expr| f(Child::Expr(x));
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Path(_) | ExprKind::Unit | ExprKind::Continue => {}
        ExprKind::Tuple(es) | ExprKind::Array(es) => es.iter().for_each(expr),
        // A repeat's count is a constant of its own, checked separately.
        ExprKind::Repeat(x, _)
        | ExprKind::Paren(x)
        | ExprKind::Unary(_, x)
        | ExprKind::Deref(x)
        | ExprKind::Field(x, _)
        | ExprKind::Cast(x, _)
        | ExprKind::Ref { expr: x, .. } => expr(x),
        ExprKind::Binary(_, a, b)
        | ExprKind::Assign(a, b)
        | ExprKind::AssignOp(_, a, b)
        | ExprKind::Index(a, b) => {
            expr(a);
            expr(b);
        }
        ExprKind::Range(lo, hi, _) => [lo, hi].into_iter().flatten().for_each(|x| expr(x)),
        ExprKind::Call { callee, args } => {
            expr(callee);
            args.iter().for_each(expr);
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            expr(receiver);
            args.iter().for_each(expr);
        }
        ExprKind::Block(b) | ExprKind::Loop { body: b } => f(Child::Block(b)),
        ExprKind::If { cond, then, els } => {
            f(Child::Expr(cond));
            f(Child::Block(then));
            if let Some(e) = els {
                f(Child::Expr(e));
            }
        }
        ExprKind::While { cond, body } => {
            f(Child::Expr(cond));
            f(Child::Block(body));
        }
        ExprKind::Break(x) | ExprKind::Return(x) => {
            if let Some(x) = x {
                expr(x);
            }
        }
        ExprKind::Struct { fields, .. } => fields.iter().for_each(|fi| expr(&fi.value)),
        ExprKind::Match { scrutinee, arms } => {
            expr(scrutinee);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    expr(g);
                }
                expr(&arm.body);
            }
        }
        ExprKind::Let(_, scrutinee) => expr(scrutinee),
    }
}

// ------------------------------------------------------------------------------------------
// Exhaustiveness and reachability
// ------------------------------------------------------------------------------------------

/// The item tables as the usefulness checker sees them.
struct Shapes<'a> {
    items: &'a ItemTypes,
}

impl usefulness::PatCx for Shapes<'_> {
    fn adt(&self, def: DefId) -> usefulness::AdtShape {
        let adt = self.items.adt(def);
        let kind = |k: VariantKind| match k {
            VariantKind::Unit => usefulness::VariantKind::Unit,
            VariantKind::Tuple => usefulness::VariantKind::Tuple,
            VariantKind::Named => usefulness::VariantKind::Named,
        };
        let fields = |fs: &[crate::collect::FieldDef]| {
            fs.iter()
                .map(|f| usefulness::FieldShape {
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                })
                .collect()
        };
        match &adt.variants {
            Some(vs) => usefulness::AdtShape::Enum {
                name: adt.name.clone(),
                variants: vs
                    .iter()
                    .map(|v| usefulness::VariantShape {
                        name: v.name.clone(),
                        kind: kind(v.kind),
                        fields: fields(&v.fields),
                    })
                    .collect(),
            },
            None => usefulness::AdtShape::Struct {
                name: adt.name.clone(),
                fields: fields(&adt.fields),
                kind: if adt.is_unit {
                    usefulness::VariantKind::Unit
                } else {
                    usefulness::VariantKind::Named
                },
            },
        }
    }
}

/// Checks every `match` for exhaustiveness (E0328, rustc's E0004) and every `let`,
/// parameter, `if let`, `while let` and `let`-`else` pattern for refutability (E0329,
/// rustc's E0005; for the conditional forms a warning when the pattern always matches),
/// and warns about unreachable patterns (rustc's `unreachable_patterns`).
pub(crate) fn exhaustiveness(
    file: &SourceFile,
    resolve: &ResolveResults,
    items: &ItemTypes,
    bodies: &mut HashMap<DefId, BodyTypes>,
    sink: &mut DiagnosticSink,
) {
    for item in file.all_items() {
        let ItemKind::Fn(f) = &item.kind else {
            continue;
        };
        let def = resolve.def_of_item(item.id);
        let Some(body) = bodies.get_mut(&def) else {
            continue;
        };
        if body.has_errors {
            continue;
        }
        let errors_before = sink.error_count();
        let mut cx = Exhaustive {
            resolve,
            items,
            body,
            sink,
        };
        for p in &f.params {
            cx.refutable(&p.pat, "function argument");
        }
        cx.block(&f.body);
        if sink.error_count() > errors_before {
            body.has_errors = true;
        }
    }
}

struct Exhaustive<'a, 'b> {
    resolve: &'a ResolveResults,
    items: &'a ItemTypes,
    body: &'b BodyTypes,
    sink: &'a mut DiagnosticSink,
}

impl Exhaustive<'_, '_> {
    fn block(&mut self, b: &sabiko_ast::Block) {
        for s in &b.stmts {
            match &s.kind {
                sabiko_ast::StmtKind::Let(l) => {
                    if let Some(init) = &l.init {
                        self.expr(init);
                    }
                    if let Some(els) = &l.els {
                        self.block(els);
                        self.conditional(&l.pat, None, ConditionalKind::LetElse, els.span);
                    } else {
                        self.refutable(&l.pat, "local binding");
                    }
                }
                sabiko_ast::StmtKind::Expr { expr, .. } => self.expr(expr),
                sabiko_ast::StmtKind::Empty => {}
            }
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
                self.check_match(scrutinee, arms);
            }
            ExprKind::If { cond, then, els } => {
                if let ExprKind::Let(pat, scrutinee) = &strip_parens(cond).kind {
                    self.expr(scrutinee);
                    self.conditional(pat, Some(scrutinee), ConditionalKind::IfLet, cond.span);
                } else {
                    self.expr(cond);
                }
                self.block(then);
                if let Some(els) = els {
                    self.expr(els);
                }
            }
            ExprKind::While { cond, body } => {
                if let ExprKind::Let(pat, scrutinee) = &strip_parens(cond).kind {
                    self.expr(scrutinee);
                    self.conditional(pat, Some(scrutinee), ConditionalKind::WhileLet, cond.span);
                } else {
                    self.expr(cond);
                }
                self.block(body);
            }
            _ => for_each_child(e, &mut |c| match c {
                Child::Expr(c) => self.expr(c),
                Child::Block(b) => self.block(b),
            }),
        }
    }

    /// rustc's deny-by-default E0170: a binding named like a unit variant of the enum it
    /// matches binds everything, which is almost never meant. A lint in rustc, so it does
    /// not stop the other checks of the match.
    fn bindings_named_like_variants(&mut self, pat: &Pat) {
        let mut found = Vec::new();
        walk_all_idents(pat, &mut |p| {
            let PatKind::Ident {
                name, sub: None, ..
            } = &p.kind
            else {
                return;
            };
            if self.resolve.local_of_pat(p.id).is_none() {
                return;
            }
            let mut t = self.body.pat_types.get(&p.id).cloned().unwrap_or(Ty::Error);
            while let Ty::Ref(_, inner) = t {
                t = *inner;
            }
            let Some(adt) = self.items.as_enum(&t) else {
                return;
            };
            if adt
                .variants()
                .iter()
                .any(|v| v.kind == VariantKind::Unit && v.name == name.name)
            {
                found.push((name.clone(), adt.name.clone()));
            }
        });
        for (name, enum_name) in found {
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::BINDING_NAMED_LIKE_VARIANT,
                    format!(
                        "pattern binding `{}` is named the same as one of the variants of the type `{enum_name}`",
                        name.name
                    ),
                )
                .with_label(
                    name.span,
                    format!("to match on the variant, qualify the path: `{enum_name}::{}`", name.name),
                ),
            );
        }
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[sabiko_ast::Arm]) {
        for arm in arms {
            self.bindings_named_like_variants(&arm.pat);
        }
        let ty = self.body.expr_ty(scrutinee.id).clone();
        let lowered: Vec<usefulness::Arm> = arms
            .iter()
            .map(|arm| usefulness::Arm {
                pat: self.lower(&arm.pat),
                has_guard: arm.guard.is_some(),
            })
            .collect();
        let cx = Shapes { items: self.items };
        let report = usefulness::check_match_with_validity(
            &cx,
            &ty,
            &lowered,
            !through_deref(scrutinee, self.body),
        );
        self.unreachable(&report.unreachable);
        if report.witnesses.is_empty() {
            return;
        }
        let message = if usefulness::reports_type_non_empty(&cx, &ty, &lowered) {
            format!(
                "non-exhaustive patterns: type `{}` is non-empty",
                ty.display(self.resolve)
            )
        } else {
            format!(
                "non-exhaustive patterns: {}",
                usefulness::joined_witnesses(&report.witnesses)
            )
        };
        self.sink.emit(
            Diagnostic::error(ErrorCode::NON_EXHAUSTIVE_MATCH, message)
                .with_label(scrutinee.span, usefulness::witnesses_label(&report.witnesses))
                .with_note("ensure that all possible cases are handled, by adding a match arm with a wildcard pattern or an explicit pattern"),
        );
    }

    /// A `let` or parameter pattern must be irrefutable.
    fn refutable(&mut self, pat: &Pat, what: &str) {
        let report = self.check_one(pat);
        if report.witnesses.is_empty() {
            return;
        }
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::REFUTABLE_PATTERN,
                format!("refutable pattern in {what}"),
            )
            .with_label(pat.span, usefulness::witnesses_label(&report.witnesses))
            .with_note("a `let` or parameter pattern must match every value; use `match`, `if let` or `let`-`else` to handle the others"),
        );
    }

    /// `if let`, `while let` and `let`-`else`: a pattern that always matches is a warning.
    fn conditional(
        &mut self,
        pat: &Pat,
        _scrutinee: Option<&Expr>,
        kind: ConditionalKind,
        span: sabiko_span::Span,
    ) {
        let report = self.check_one(pat);
        if !report.witnesses.is_empty() {
            return;
        }
        let (message, label, at) = match kind {
            ConditionalKind::IfLet => (
                "irrefutable `if let` pattern",
                "this pattern will always match",
                span,
            ),
            ConditionalKind::WhileLet => (
                "irrefutable `while let` pattern",
                "this pattern will always match",
                span,
            ),
            ConditionalKind::LetElse => (
                "unreachable `else` clause",
                "assigning to binding pattern will always succeed",
                span,
            ),
        };
        self.sink.emit(
            Diagnostic::warning(ErrorCode::IRREFUTABLE_LET_PATTERN, message).with_label(at, label),
        );
    }

    fn check_one(&mut self, pat: &Pat) -> usefulness::Report {
        self.bindings_named_like_variants(pat);
        let ty = self
            .body
            .pat_types
            .get(&pat.id)
            .cloned()
            .unwrap_or(Ty::Error);
        let arm = usefulness::Arm {
            pat: self.lower(pat),
            has_guard: false,
        };
        let cx = Shapes { items: self.items };
        usefulness::check_match(&cx, &ty, &[arm])
    }

    fn unreachable(&mut self, spans: &[sabiko_span::Span]) {
        for &span in spans {
            self.sink.emit(
                Diagnostic::warning(ErrorCode::UNREACHABLE_PATTERN, "unreachable pattern")
                    .with_label(span, "no value can reach this"),
            );
        }
    }

    /// The checker's form of `pat`: references it looks through become `Deref` nodes.
    fn lower(&self, pat: &Pat) -> usefulness::Pat {
        if !self.names_a_constructor(pat) {
            // The resolver reported the path; an error type keeps the checker from
            // reporting the match as well, as rustc does.
            return Self::wild(Ty::Error, pat.span);
        }
        let ty = self
            .body
            .pat_types
            .get(&pat.id)
            .cloned()
            .unwrap_or(Ty::Error);
        let derefs = self.body.pat_derefs.get(&pat.id).copied().unwrap_or(0);
        let mut tys = vec![ty];
        for _ in 0..derefs {
            let inner = match tys.last().expect("not empty") {
                Ty::Ref(_, inner) => (**inner).clone(),
                _ => Ty::Error,
            };
            tys.push(inner);
        }
        let inner_ty = tys.pop().expect("not empty");
        let mut out = usefulness::Pat {
            kind: self.lower_kind(pat, &inner_ty),
            ty: inner_ty,
            span: pat.span,
        };
        while let Some(ty) = tys.pop() {
            out = usefulness::Pat {
                kind: usefulness::PatKind::Deref(Box::new(out)),
                ty,
                span: pat.span,
            };
        }
        out
    }

    /// Whether the path of a path, tuple-struct or struct pattern names what such a
    /// pattern can match (a variant, or a struct for `S { .. }` and a unit struct's
    /// `Self`): one that does not was reported by the resolver (`E::Missing`, `F(x)` for a
    /// function `F`, `C { .. }` for a constant `C`) without making the body erroneous.
    fn names_a_constructor(&self, pat: &Pat) -> bool {
        let is_struct = || {
            matches!(self.resolve.pat_res(pat.id),
                Some(Res::Def(def)) if self.resolve.def(def).kind == DefKind::Struct)
        };
        match &pat.kind {
            PatKind::TupleStruct(..) => self.body.pat_variants.contains_key(&pat.id),
            PatKind::Path(_) | PatKind::Struct { .. } => {
                self.body.pat_variants.contains_key(&pat.id) || is_struct()
            }
            _ => true,
        }
    }

    fn wild(ty: Ty, span: sabiko_span::Span) -> usefulness::Pat {
        usefulness::Pat {
            kind: usefulness::PatKind::Wild,
            ty,
            span,
        }
    }

    fn lower_kind(&self, pat: &Pat, ty: &Ty) -> usefulness::PatKind {
        use usefulness::PatKind as K;
        if let (PatKind::Ident { .. }, Some(Res::Def(def))) =
            (&pat.kind, self.resolve.pat_res(pat.id))
        {
            return match self.resolve.def(def).kind {
                DefKind::Const => self.value(pat),
                _ => K::Ctor {
                    variant: None,
                    fields: Vec::new(),
                },
            };
        }
        match &pat.kind {
            PatKind::Ident { sub: Some(sub), .. } => self.lower(sub).kind,
            PatKind::Ident { .. } | PatKind::Wild | PatKind::Rest => K::Wild,
            PatKind::Or(alts) => K::Or(alts.iter().map(|a| self.lower(a)).collect()),
            PatKind::Lit(_) | PatKind::Range(..) => self.value(pat),
            PatKind::Path(_) => match self.body.pat_variants.get(&pat.id) {
                Some(&(_, index)) => K::Ctor {
                    variant: Some(index),
                    fields: Vec::new(),
                },
                None => match self.resolve.pat_res(pat.id) {
                    Some(Res::Def(def)) if self.resolve.def(def).kind == DefKind::Const => {
                        self.value(pat)
                    }
                    _ => K::Ctor {
                        variant: None,
                        fields: Vec::new(),
                    },
                },
            },
            PatKind::Tuple(elems) | PatKind::TupleStruct(_, elems) => {
                let variant = self.body.pat_variants.get(&pat.id).map(|&(_, i)| i);
                let tys = self.field_tys(ty, variant);
                let mut fields: Vec<usefulness::Pat> = tys
                    .iter()
                    .map(|t| Self::wild(t.clone(), pat.span))
                    .collect();
                for (p, pos) in elems
                    .iter()
                    .zip(crate::check::rest_positions(elems, tys.len()))
                {
                    if let Some(i) = pos {
                        if i < fields.len() {
                            fields[i] = self.lower(p);
                        }
                    }
                }
                K::Ctor { variant, fields }
            }
            PatKind::Struct { fields: fps, .. } => {
                let variant = self.body.pat_variants.get(&pat.id).map(|&(_, i)| i);
                let defs = self.field_defs(ty, variant);
                let mut fields: Vec<usefulness::Pat> = defs
                    .iter()
                    .map(|d| Self::wild(d.ty.clone(), pat.span))
                    .collect();
                for fp in fps {
                    if let Some(i) = defs.iter().position(|d| d.name == fp.name.name) {
                        fields[i] = self.lower(&fp.pat);
                    }
                }
                K::Ctor { variant, fields }
            }
        }
    }

    fn field_defs(&self, ty: &Ty, variant: Option<usize>) -> Vec<crate::collect::FieldDef> {
        match ty {
            Ty::Adt(def) => {
                let adt = self.items.adt(*def);
                match variant {
                    Some(v) => adt.variant(v).fields.clone(),
                    None => adt.fields.clone(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn field_tys(&self, ty: &Ty, variant: Option<usize>) -> Vec<Ty> {
        match ty {
            Ty::Tuple(tys) => tys.to_vec(),
            _ => self
                .field_defs(ty, variant)
                .into_iter()
                .map(|f| f.ty)
                .collect(),
        }
    }

    /// A literal, constant or range pattern.
    fn value(&self, pat: &Pat) -> usefulness::PatKind {
        use usefulness::PatKind as K;
        let int = |v: &ConstValue| -> Option<i128> {
            match v {
                ConstValue::Int(i) => Some(*i),
                ConstValue::Uint(u) => Some(*u as i128),
                ConstValue::Char(c) => Some(i128::from(u32::from(*c))),
                _ => None,
            }
        };
        match self.body.pat_consts.get(&pat.id) {
            Some(PatConst::Value(v)) => match v {
                ConstValue::Bool(b) => K::Bool(*b),
                ConstValue::Float(f) => K::Float(*f),
                ConstValue::Str(s) => K::Str(s.clone()),
                other => match int(other) {
                    Some(i) => K::Range { lo: i, hi: i },
                    None => K::Opaque,
                },
            },
            Some(PatConst::Range { lo, hi, inclusive }) => match (int(lo), int(hi)) {
                (Some(l), Some(h)) => K::Range {
                    lo: l,
                    hi: if *inclusive { h } else { h - 1 },
                },
                _ => K::Opaque,
            },
            None => K::Opaque,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConditionalKind {
    IfLet,
    WhileLet,
    LetElse,
}

/// Calls `f` for every identifier pattern in `pat`, in every alternative.
fn walk_all_idents<'p>(pat: &'p Pat, f: &mut impl FnMut(&'p Pat)) {
    match &pat.kind {
        PatKind::Ident { sub, .. } => {
            f(pat);
            if let Some(sub) = sub {
                walk_all_idents(sub, f);
            }
        }
        PatKind::Tuple(ps) | PatKind::TupleStruct(_, ps) | PatKind::Or(ps) => {
            for p in ps {
                walk_all_idents(p, f);
            }
        }
        PatKind::Struct { fields, .. } => {
            for fp in fields {
                walk_all_idents(&fp.pat, f);
            }
        }
        PatKind::Wild | PatKind::Rest | PatKind::Path(_) | PatKind::Lit(_) | PatKind::Range(..) => {
        }
    }
}

fn strip_parens(mut e: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    e
}

/// Whether a scrutinee is a place reached through a dereference (`*r`, `(*r).F`, or `r.F`
/// for a reference `r`, which dereferences it implicitly), which rustc does not assume to
/// hold a valid value.
fn through_deref(e: &Expr, body: &BodyTypes) -> bool {
    let e = strip_parens(e);
    match &e.kind {
        ExprKind::Deref(_) => true,
        ExprKind::Field(base, _) => {
            body.field_accesses
                .get(&e.id)
                .is_some_and(|access| access.autoderefs > 0)
                || through_deref(base, body)
        }
        _ => false,
    }
}
