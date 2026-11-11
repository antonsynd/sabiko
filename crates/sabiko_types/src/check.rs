//! Type checking of function bodies and constant initialisers.
//!
//! Bidirectional: every expression is checked against an optional expected type, which
//! flows down from annotations, parameter types, return types and struct field types, and
//! unification flows information back up. The only coercions are `!` to anything and
//! `&mut T` to `&T`.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{
    Arm, BinOp, Block, Expr, ExprKind, FnDecl, Lit, NodeId, Pat, PatKind, Stmt, StmtKind, Type,
    TypeKind, UnOp,
};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{
    Builtin, CoreItem, DefId, DefKind, Intrinsic, LangItem, LocalId, Predicate, PrimTy, Res,
    ResolveResults,
};
use sabiko_span::Span;

use crate::collect::{self, FieldDef, ItemTypes, VariantKind};
use crate::infer::{InferCtxt, VarKind};
use crate::traits::{has_error, Solution};
use crate::ty::{Mutability, Ty, TyVid};

/// A field access `e.f`: which field, and how many references were peeled off `e` first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldAccess {
    pub index: usize,
    pub autoderefs: usize,
}

/// The function a method call `e.Name(..)` calls, and how the receiver `e` becomes its
/// first argument: `autoderefs` dereferences, then a borrow (`autoref`) or, for a
/// by-value `self`, a copy or move of the place reached. A receiver reached by value
/// through a reference is reborrowed (`&*r`, `&mut *m`), as in rustc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodCallee {
    pub def: DefId,
    pub autoderefs: usize,
    pub autoref: Option<Mutability>,
    /// The borrowed receiver, an array, is unsized to a slice (`a.get(1)` for `a: [T;
    /// N]` calls `<[T]>::get(&a as &[T], 1)`); only with `autoref`.
    pub unsize: bool,
}

/// A bound a use requires: `ty` (with inference variables until the body is done) must
/// implement `trait_`. Judged in `finish`.
struct Obligation {
    ty: Ty,
    trait_: DefId,
    span: Span,
}

/// A use of a generic function in a body: its generic arguments (inference variables
/// until the body is done), the function, and the span of the use.
type GenericUse = (Vec<Ty>, DefId, Span);

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
    /// Expressions of type `&mut T` that are reborrowed where they are coerced, rather
    /// than moved: `&mut *e`, or `&*e` (`Mutability::Not`) where a `&T` is expected.
    /// rustc's reborrow adjustment, which its coercion inserts only when the target type
    /// is already known to be a reference at the coercion site: a parameter of type
    /// `&mut u8` reborrows its argument, one of type `T` (an inference variable then)
    /// moves it, and so do `let x = r;` and an element of a tuple nothing types (#132).
    /// Keyed by the coerced expression's node inside any parentheses; read through
    /// [`BodyTypes::reborrow`].
    pub(crate) reborrows: HashMap<NodeId, Mutability>,
    /// The callee of every method call, keyed by the `ExprKind::MethodCall` node.
    pub method_calls: HashMap<NodeId, MethodCallee>,
    /// The function a path names where the type checker chose it rather than the
    /// resolver: `Type::Name` naming a trait's method, among the traits with a method of
    /// that name the one the type implements. Keyed by the `ExprKind::Path` node.
    pub path_defs: HashMap<NodeId, DefId>,
    /// The generic arguments of each use of a generic function, one per type parameter:
    /// keyed by the callee's `ExprKind::Path` node for a call, by the
    /// `ExprKind::MethodCall` node for a method call. Absent for a function that is not
    /// generic.
    pub generic_args: HashMap<NodeId, Vec<Ty>>,
    /// The type of the value each pattern node is matched against, before it looks
    /// through references (see `pat_derefs`), keyed by the `Pat` node.
    pub pat_types: HashMap<NodeId, Ty>,
    /// The variant a tuple-struct, struct or path pattern names, as `(enum, index)`.
    pub pat_variants: HashMap<NodeId, (DefId, usize)>,
    /// The variant an expression builds, as `(enum, index)`: a unit variant path, a
    /// tuple-variant constructor call (keyed by the call) or a struct-variant literal.
    pub variant_exprs: HashMap<NodeId, (DefId, usize)>,
    /// The values literal, range and constant patterns compare with, keyed by the `Pat`
    /// node; filled in after constant evaluation (`crate::patterns`).
    pub pat_consts: HashMap<NodeId, PatConst>,
    /// How many references an index expression's base looks through to reach the array
    /// or slice (`r[1]` for `r: &[T; 2]`), keyed by the `ExprKind::Index` node.
    pub index_derefs: HashMap<NodeId, usize>,
    /// `e.len()` on an array or slice, with how many references `e` looks through first,
    /// keyed by the `ExprKind::MethodCall` node.
    pub len_calls: HashMap<NodeId, usize>,
    /// Expressions whose value is coerced to another pointer representation where it is
    /// used, with the target type: `&[T; N]` or `&mut [T; N]` to `&[T]` / `&mut [T]`
    /// (unsizing), and `&mut [T]` to `&[T]`. Keyed by the expression's node inside any
    /// parentheses, read through [`BodyTypes::coercion`]; or by the pattern node of a
    /// destructuring assignment's assignee ([`BodyTypes::assignee_coercion`]).
    pub(crate) coercions: HashMap<NodeId, Ty>,
    /// The evaluated count of each repeat expression `[v; N]`, keyed by its node.
    pub repeat_counts: HashMap<NodeId, u64>,
    /// `e?`, keyed by the `ExprKind::Try` node: whether `e` is an `Option` or a `Result`
    /// (the function returns the same kind, decision 93).
    pub try_exprs: HashMap<NodeId, CoreItem>,
    /// `true` when at least one error was reported inside this body.
    pub has_errors: bool,
}

/// What a literal, constant or range pattern compares the matched value with.
#[derive(Clone, Debug, PartialEq)]
pub enum PatConst {
    /// Equal to the value (a literal or a constant).
    Value(crate::ConstValue),
    /// Within `lo..=hi` (`inclusive`) or `lo..hi`; a missing bound is unbounded (#49):
    /// `..=hi`, `..hi`, and `lo..`, which includes the type's maximum (`inclusive`, as
    /// rustc records it).
    Range {
        lo: Option<crate::ConstValue>,
        hi: Option<crate::ConstValue>,
        inclusive: bool,
    },
}

impl BodyTypes {
    /// The pointer coercion `expr`'s value gets where it is used (see `coercions`). Side
    /// tables of adjustments are keyed by the expression inside any parentheses, so `(r)`
    /// and `r` agree (a reader keying by the parenthesised node missed them).
    #[must_use]
    pub fn coercion(&self, expr: &Expr) -> Option<&Ty> {
        self.coercions.get(&strip_parens(expr).id)
    }

    /// The pointer coercion of the value a destructuring assignment writes to the
    /// assignee pattern `pat`.
    #[must_use]
    pub fn assignee_coercion(&self, pat: NodeId) -> Option<&Ty> {
        self.coercions.get(&pat)
    }

    /// How `expr`, a `&mut T`, is reborrowed where it is coerced (see `reborrows`);
    /// `None` when it is moved. Through parentheses, as [`BodyTypes::coercion`].
    #[must_use]
    pub fn reborrow(&self, expr: &Expr) -> Option<Mutability> {
        self.reborrows.get(&strip_parens(expr).id).copied()
    }

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

/// `e?` on a `Result<_, E1>` in a function returning `Result<_, E2>`: `E2` must convert
/// from `E1`, judged in `finish` once both are known.
struct PendingTry {
    from: Ty,
    to: Ty,
    /// The `?`.
    span: Span,
    /// The operand.
    operand_span: Span,
}

struct PendingCast {
    span: Span,
    from: Ty,
    to: Ty,
}

/// A binary operation with an operand of type `!` (#87), or typed by a diverging variable
/// nothing has decided yet (`let y = return; y + 1`), judged in `finish` before the
/// never-type fallback. rustc types an operator's right operand against a fresh variable
/// that only the operator trait's impl for the left type decides, so `!` reaches the
/// trait instead of coercing: no arithmetic, bitwise or shift trait has an impl for `!`
/// (`i32` has `Add<i32>` and `Add<&i32>`, so nothing decides the variable), and a
/// comparison takes one only when the left type is known before the literal defaults
/// and has a single impl. A variable decided by `finish` (`let _: i32 = y;`) makes the
/// operation an ordinary one, checked then.
struct NeverOperand {
    op: BinOp,
    /// `a op= b`.
    compound: bool,
    lhs: Ty,
    rhs: Ty,
    span: Span,
    /// The result of an arithmetic or shift operation, unknown until it is judged.
    out: Option<Ty>,
}

/// A failed coercion: where, the expected type and the type found.
type Mismatch = (Span, Ty, Ty);

#[allow(clippy::struct_excessive_bools)] // independent flags of the expression being checked
pub(crate) struct FnCtxt<'a> {
    resolve: &'a ResolveResults,
    items: &'a ItemTypes,
    sink: &'a mut DiagnosticSink,
    infer: InferCtxt,
    expr_types: HashMap<NodeId, Ty>,
    /// Expression spans, for "type annotations needed" on an expression no local holds.
    expr_spans: HashMap<NodeId, Span>,
    local_types: HashMap<LocalId, Ty>,
    /// Pattern spans of locals, for "type annotations needed".
    local_spans: HashMap<LocalId, Span>,
    field_accesses: HashMap<NodeId, FieldAccess>,
    pat_derefs: HashMap<NodeId, usize>,
    ref_bindings: HashMap<NodeId, Mutability>,
    reborrows: HashMap<NodeId, Mutability>,
    /// While a call argument is checked against an [`Self::expected_inputs`] hint: its
    /// node (inside any parentheses) and, once checked, its own failed coercion to the
    /// hint (where, expected, found), which [`Self::check_args`] reports instead of
    /// [`Self::coerce_to_expected`].
    deferred_mismatch: Option<(NodeId, Option<Mismatch>)>,
    method_calls: HashMap<NodeId, MethodCallee>,
    /// See [`BodyTypes::generic_args`]; with the function and the span to report an
    /// argument that inference leaves unknown at.
    generic_args: HashMap<NodeId, GenericUse>,
    pat_types: HashMap<NodeId, Ty>,
    pat_variants: HashMap<NodeId, (DefId, usize)>,
    variant_exprs: HashMap<NodeId, (DefId, usize)>,
    /// Bindings already typed by an earlier alternative of an or-pattern: a later
    /// alternative's binding of the same local must have the same type.
    or_bindings: HashSet<LocalId>,
    /// Set while checking the pattern of a destructuring assignment, which has no default
    /// binding modes (rustc: `(a, b) = &t` is a mismatch, not a look through `&`).
    in_assignee: bool,
    ret_ty: Ty,
    loops: Vec<LoopCtxt>,
    diverges: bool,
    /// Set while checking the operand of unary `-`, so `-128i8` is in range.
    negating: bool,
    /// Set while checking a `-` that is itself the operand of a `-`: as in rustc, only the
    /// outer negation counts, so the literal of `-(-128i8)` is `128i8`, out of range.
    double_negation: bool,
    /// Set while checking an operand that is used as a place where an unsized type is
    /// fine: the operand of `&`/`&mut`, the base of an index and the receiver of
    /// `.len()`. There `*s` for `s: &[T]` is a valid place (`&*s`, `(*s)[i]`,
    /// `(*s).len()`); only its value has no size. Consumed by the next expression checked
    /// (passed through parentheses).
    unsized_place: bool,
    int_lits: Vec<IntLit>,
    casts: Vec<PendingCast>,
    /// `-e` where `e` was still `{integer}`: rejected at the end if it becomes unsigned.
    pending_negs: Vec<(Span, Ty)>,
    /// Variables for `let x = <diverging>;`: unconstrained ones fall back to `!`.
    diverging_vars: Vec<Ty>,
    /// `Print`/`PrintLine` arguments typed by such a variable (`let v = return;
    /// PrintLine(v);`), checked once the fallback decided them.
    pending_prints: Vec<(Builtin, Span, Ty)>,
    /// Comparisons whose operand type was still an unknown variable: judged in `finish`.
    pending_comparisons: Vec<(BinOp, Ty, Span)>,
    never_operands: Vec<NeverOperand>,
    /// The values of the `const` items, for array lengths; `None` while the constants
    /// themselves are checked (arrays are not constants yet).
    consts: Option<&'a crate::consts::ConstCx<'a, 'a>>,
    index_derefs: HashMap<NodeId, usize>,
    len_calls: HashMap<NodeId, usize>,
    coercions: HashMap<NodeId, Ty>,
    repeat_counts: HashMap<NodeId, u64>,
    /// Repeat expressions with more than one element, whose operand must be `Copy`
    /// (unless it names a `const` item): judged in `finish`, once the type is known.
    pending_repeats: Vec<(Span, Ty)>,
    errors_before: usize,
    /// The bounds that hold in the function being checked (`ItemTypes::param_env`).
    env: Vec<Predicate>,
    /// What `Self` names in a trait `impl` block's function: its `Self::V`, `Self { .. }`
    /// take their generic arguments from it.
    self_ty: Option<Ty>,
    obligations: Vec<Obligation>,
    path_defs: HashMap<NodeId, DefId>,
    try_exprs: HashMap<NodeId, CoreItem>,
    pending_tries: Vec<PendingTry>,
    /// The function's signature, from its name to its return type, while a function's
    /// body is checked; `None` for a constant.
    fn_span: Option<Span>,
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
            expr_spans: HashMap::new(),
            local_types: HashMap::new(),
            local_spans: HashMap::new(),
            field_accesses: HashMap::new(),
            pat_derefs: HashMap::new(),
            ref_bindings: HashMap::new(),
            reborrows: HashMap::new(),
            deferred_mismatch: None,
            method_calls: HashMap::new(),
            generic_args: HashMap::new(),
            pat_types: HashMap::new(),
            pat_variants: HashMap::new(),
            variant_exprs: HashMap::new(),
            or_bindings: HashSet::new(),
            in_assignee: false,
            ret_ty,
            loops: Vec::new(),
            diverges: false,
            negating: false,
            double_negation: false,
            unsized_place: false,
            int_lits: Vec::new(),
            casts: Vec::new(),
            pending_negs: Vec::new(),
            diverging_vars: Vec::new(),
            pending_prints: Vec::new(),
            pending_comparisons: Vec::new(),
            never_operands: Vec::new(),
            consts: None,
            index_derefs: HashMap::new(),
            len_calls: HashMap::new(),
            coercions: HashMap::new(),
            repeat_counts: HashMap::new(),
            pending_repeats: Vec::new(),
            errors_before,
            env: Vec::new(),
            self_ty: None,
            obligations: Vec::new(),
            path_defs: HashMap::new(),
            try_exprs: HashMap::new(),
            pending_tries: Vec::new(),
            fn_span: None,
        }
    }

    /// Gives the checker the evaluated constants, so array lengths can be evaluated.
    pub(crate) fn with_consts(mut self, consts: &'a crate::consts::ConstCx<'a, 'a>) -> Self {
        self.consts = Some(consts);
        self
    }

    /// Checks and evaluates an array length or repeat count, a `usize` constant of its
    /// own. `None` after an error, which was reported.
    fn array_len(&mut self, len: &Expr) -> Option<u64> {
        let Some(consts) = self.consts else {
            self.error(
                Diagnostic::error(
                    ErrorCode::NON_CONSTANT_EXPRESSION,
                    "arrays cannot be used in constants yet",
                )
                .with_label(len.span, "an array length inside a constant")
                .with_note("constants are scalars for now; see docs/PLAN.md"),
            );
            return None;
        };
        // The bounds of the function hold in it: `[0; T::N]` is reported as depending on
        // `T` (E0345), not as `T` lacking the bound.
        let mut cx = FnCtxt::new(self.resolve, self.items, self.sink, Ty::Prim(PrimTy::Usize))
            .with_consts(consts);
        cx.env.clone_from(&self.env);
        cx.self_ty.clone_from(&self.self_ty);
        let body = cx.check_const(len);
        if body.has_errors {
            return None;
        }
        crate::consts::eval_len(consts, len, &body, self.sink)
    }

    pub(crate) fn check_fn(mut self, f: &FnDecl, def: DefId) -> BodyTypes {
        let Some(body) = &f.body else {
            // A required method of a trait has nothing to check.
            return self.finish();
        };
        self.env = self.items.param_env(def).to_vec();
        if self.resolve.parent_kind(def) == Some(DefKind::Impl) {
            let imp = self.resolve.def(def).parent.expect("a parent");
            self.self_ty = Some(self.items.impl_self_ty(imp));
        }
        let end = f.ret.as_ref().map_or(f.name.span.end, |t| t.span.end);
        self.fn_span = Some(Span::new(f.name.span.file, f.name.span.start, end));
        let sig = self.items.fn_sig(def).clone();
        let mut inputs = sig.inputs.iter();
        if let Some(sp) = &f.self_param {
            let ty = inputs
                .next()
                .expect("a method's signature starts with its receiver");
            if let Some(local) = self.resolve.local_of_pat(sp.id) {
                self.local_types.insert(local, ty.clone());
                self.local_spans.insert(local, sp.span);
            }
        }
        for (p, ty) in f.params.iter().zip(inputs) {
            self.check_pat(&p.pat, ty, None);
        }
        let ret = self.ret_ty.clone();
        self.check_block(body, Some(&ret));
        if self.resolve.parent_kind(def) == Some(DefKind::Trait) {
            self.check_self_sized(def, f);
        }
        self.finish()
    }

    /// The value of the constant `def` of a trait or of a trait `impl` block (#116),
    /// checked where the bounds of its trait (`Self: Trait`) or block hold.
    pub(crate) fn check_assoc_const(mut self, value: &Expr, def: DefId) -> BodyTypes {
        self.env = self.items.param_env(def).to_vec();
        if self.resolve.parent_kind(def) == Some(DefKind::Impl) {
            let imp = self.resolve.def(def).parent.expect("a parent");
            self.self_ty = Some(self.items.impl_self_ty(imp));
        }
        self.check_const(value)
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
            Ty::Array(elem, n) => format!("[{}; {n}]", self.ty_str(&elem)),
            Ty::Slice(elem) => format!("[{}]", self.ty_str(&elem)),
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
        if let (Ty::Array(_, ne), Ty::Array(_, nf)) = (self.shallow(expected), self.shallow(found))
        {
            if ne != nf {
                d = d.with_note(format!(
                    "expected an array with a size of {ne}, found one with a size of {nf}"
                ));
            }
        }
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

    fn shallow(&mut self, ty: &Ty) -> Ty {
        self.infer.shallow_resolve(ty)
    }

    /// Makes `found` acceptable where `expected` is required, reporting a mismatch at
    /// `span` if it is not.
    fn coerce(&mut self, span: Span, found: &Ty, expected: &Ty) {
        if let Some((span, expected, found)) = self.coerce_or_mismatch(span, found, expected) {
            self.mismatch(span, &expected, &found);
        }
    }

    /// [`Self::coerce`], returning a plain mismatch (where, expected, found) instead of
    /// reporting it. A cyclic type is reported here.
    fn coerce_or_mismatch(&mut self, span: Span, found: &Ty, expected: &Ty) -> Option<Mismatch> {
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
                return Some((span, expected.clone(), found.clone()));
            }
        }
        None
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
        // Unsizing: `&[T; N]` to `&[T]`, `&mut [T; N]` to `&mut [T]` or `&[T]`.
        if let (Ty::Ref(mf, a), Ty::Ref(me, b)) = (&found, &expected) {
            if mf.is_mut() || !me.is_mut() {
                if let (Ty::Array(ea, _), Ty::Slice(eb)) = (self.shallow(a), self.shallow(b)) {
                    if self.infer.unify(&ea, &eb).is_ok() {
                        return Ok(());
                    }
                }
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
            TypeKind::Never => Ty::Never,
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
            TypeKind::Array(elem, len) => {
                let elem = self.lower_type_unchecked(elem);
                match self.array_len(len) {
                    Some(n) => Ty::Array(Box::new(elem), n),
                    None => Ty::Error,
                }
            }
            TypeKind::Slice(elem) => Ty::Slice(Box::new(self.lower_type_unchecked(elem))),
            _ => collect::lower_sig_type(ty, self.resolve, &self.items.sig_cx),
        }
    }

    fn record(&mut self, expr: &Expr, ty: Ty) -> Ty {
        self.expr_types.insert(expr.id, ty.clone());
        self.expr_spans.insert(expr.id, expr.span);
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
            // A tail coerced to the expectation gives the block the expectation's type,
            // as a `match` has (rustc's coercion at the tail): `({ &a }, 1)` as `(&[i32],
            // i32)` is a `&[i32]` block, not a `&[i32; 2]` one (#94). A diverging tail
            // keeps `!`.
            Some(tail) => {
                let ty = self.check_expr(tail, expected);
                match expected {
                    Some(exp) if !self.shallow(&ty).is_never() && !ty.is_error() => exp.clone(),
                    _ => ty,
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
                        // uses of `x` are not all typed `!`. An explicit `ref` binding
                        // takes the initialiser's own type, as in rustc (#99).
                        if self.shallow(&i).is_never() && !has_explicit_ref_binding(&l.pat) {
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
                if let Some(els) = &l.els {
                    // The `else` block must diverge: it is checked against `!`, with the
                    // pattern's bindings not in scope.
                    let before = self.diverges;
                    self.diverges = false;
                    self.check_block(els, None);
                    if !self.diverges {
                        self.error(
                            Diagnostic::error(
                                ErrorCode::TYPE_MISMATCH,
                                "`else` clause of `let...else` does not diverge",
                            )
                            .with_label(els.span, "expected `!`, found `()`")
                            .with_note("try adding a diverging expression, such as `return` or `Panic(..)`"),
                        );
                    }
                    self.diverges = before;
                }
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
            // Blocks, `if` and parentheses pass the expectation on to their tails,
            // branches and operand, which are the coercion sites.
            if !matches!(
                expr.kind,
                ExprKind::Block(_) | ExprKind::If { .. } | ExprKind::Paren(_)
            ) {
                self.note_reborrow(expr, &ty, exp);
            }
        }
        self.coerce_to_expected(expr, &ty, expected);
        self.record(expr, ty)
    }

    /// Records that `expr`, of type `found`, is reborrowed where it is coerced to
    /// `target` (see [`BodyTypes::reborrows`]): when it is a `&mut` and the target is
    /// already known to be a reference. A target that is still an inference variable (a
    /// generic parameter's `T`, an unannotated `let`) is unified with `found` instead,
    /// and the reference is moved, as in rustc's `coerce`.
    fn note_reborrow(&mut self, expr: &Expr, found: &Ty, target: &Ty) {
        if let (Ty::Ref(Mutability::Mut, _), Ty::Ref(m, _)) =
            (self.shallow(found), self.shallow(target))
        {
            self.reborrows.insert(strip_parens(expr).id, m);
        }
    }

    /// rustc's `adjust_for_branches`: the expectation the branches of an `if` or the arms
    /// of a `match` are checked against. One that is still an inference variable says
    /// nothing and is dropped, so a branch's tail is not coerced to it: in `Id(if c { r }
    /// else { s })` the `else` block's tail `s` is moved even though `r` already decided
    /// the variable, as rustc does. The caller coerces the result to the variable.
    fn expectation_for_branches(&mut self, expected: Option<&Ty>) -> Option<Ty> {
        let exp = expected?;
        match self.shallow(exp) {
            Ty::Infer(v) if self.infer.kind(v) == VarKind::General => None,
            _ => Some(exp.clone()),
        }
    }

    /// rustc's `expected_inputs_for_expected_output`: the types the arguments of a call
    /// whose result is expected to be `expected` are checked against, found by unifying
    /// the callee's `output` with `expected` without keeping the result. `None` without an
    /// (informative) expectation or when the two do not unify: the arguments are then
    /// checked against the parameters. So `let t: &mut u8 = Id(r);` reborrows `r`, as the
    /// parameter `T` is known to be `&mut u8` when `r` is coerced to it.
    fn expected_inputs(
        &mut self,
        expected: Option<&Ty>,
        output: &Ty,
        inputs: &[Ty],
    ) -> Option<Vec<Ty>> {
        let exp = self.shallow(expected?);
        if matches!(exp, Ty::Infer(_) | Ty::Error | Ty::Never) {
            return None;
        }
        // Nothing to learn when the output has no variable an expectation could decide.
        if !self.has_infer_vars(output) {
            return None;
        }
        self.infer.probe(|infer| {
            infer
                .unify(output, &exp)
                .is_ok()
                .then(|| inputs.iter().map(|t| infer.resolve(t)).collect())
        })
    }

    /// Whether `ty` still has a variable nothing has decided.
    fn has_infer_vars(&mut self, ty: &Ty) -> bool {
        self.infer.has_undecided(ty)
    }

    /// rustc's hint for a struct literal: its type `adt` (with fresh variables for the
    /// generic arguments not written) is unified with the expected type first, when they
    /// unify, so the fields are checked against what the expectation says of them: in
    /// `let w: W<&mut u8> = W { F: r };` the field `F` is known to be a `&mut u8`, and `r`
    /// is reborrowed.
    fn unify_with_expected_adt(&mut self, adt: &Ty, expected: Option<&Ty>) {
        let Some(exp) = expected.map(|e| self.shallow(e)) else {
            return;
        };
        if !matches!(exp, Ty::Adt(..)) {
            return;
        }
        if self.has_infer_vars(adt) {
            let _ = self.infer.unify_or_rollback(adt, &exp);
        }
    }

    /// Checks the arguments of a call against the parameter types `formals`, or the
    /// [`Self::expected_inputs`] `hints` for them. With hints, several arguments may fail
    /// against the same expectation (`let c: bool = Pick(1, 2);`): their mismatches are
    /// reported as one error with a label per argument, as rustc's "arguments to this
    /// function are incorrect" (E0308), at the callee (`callee_span`).
    fn check_args(
        &mut self,
        args: &[Expr],
        formals: &[Ty],
        hints: Option<&[Ty]>,
        callee_span: Span,
    ) {
        let mut failed = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let hint = hints.and_then(|h| h.get(i));
            if let Some(m) = self.check_arg(a, formals.get(i), hint) {
                failed.push(m);
            }
        }
        if failed.len() < 2 {
            for (span, expected, found) in failed {
                self.mismatch(span, &expected, &found);
            }
            return;
        }
        let mut merged = Diagnostic::error(
            ErrorCode::TYPE_MISMATCH,
            "arguments to this function are incorrect",
        )
        .with_label(callee_span, "arguments to this function are incorrect");
        for (span, expected, found) in failed {
            let (e, f) = (self.ty_str(&expected), self.ty_str(&found));
            merged = merged.with_label(span, format!("expected `{e}`, found `{f}`"));
        }
        self.error(merged);
    }

    /// Checks the argument `arg` of a call against its parameter type `formal`, or the
    /// [`Self::expected_inputs`] hint for it, which the parameter is then unified with
    /// (rustc's `check_argument_types`).
    /// Its own failed coercion to the hint is returned, not reported (see
    /// `deferred_mismatch`); a mismatch inside it is reported as usual.
    fn check_arg(
        &mut self,
        arg: &Expr,
        formal: Option<&Ty>,
        hint: Option<&Ty>,
    ) -> Option<Mismatch> {
        match (formal, hint) {
            (Some(formal), Some(hint)) => {
                let outer = self.deferred_mismatch.replace((strip_parens(arg).id, None));
                self.check_expr(arg, Some(hint));
                let own =
                    std::mem::replace(&mut self.deferred_mismatch, outer).and_then(|(_, m)| m);
                let unified = self.infer.unify(formal, hint).is_ok();
                if own.is_none() && !unified {
                    self.mismatch(arg.span, formal, hint);
                }
                own
            }
            (formal, _) => {
                self.check_expr(arg, formal);
                None
            }
        }
    }

    fn coerce_to_expected(&mut self, expr: &Expr, ty: &Ty, expected: Option<&Ty>) {
        if let Some(exp) = expected {
            // Blocks and `if` report mismatches on their tails / branches themselves.
            if !matches!(
                expr.kind,
                ExprKind::Block(_) | ExprKind::If { .. } | ExprKind::Paren(_)
            ) {
                let deferred = matches!(self.deferred_mismatch, Some((id, None)) if id == expr.id);
                if deferred {
                    let m = self.coerce_or_mismatch(expr.span, ty, exp);
                    self.deferred_mismatch = Some((expr.id, m));
                } else {
                    self.coerce(expr.span, ty, exp);
                }
                self.note_pointer_coercion(strip_parens(expr).id, ty, exp);
            }
        }
    }

    /// Checks an operand of an operator as [`Self::check_expr`] does, also saying whether
    /// its type is `!` or a diverging variable still open (see [`NeverOperand`]). Such an
    /// operand is not coerced to `expected`, which then only guides the checking: the
    /// operator decides what it becomes.
    fn check_operand(&mut self, expr: &Expr, expected: Option<&Ty>) -> (Ty, bool) {
        if let ExprKind::Paren(inner) = &expr.kind {
            let (ty, never) = self.check_operand(inner, expected);
            return (self.record(expr, ty), never);
        }
        let ty = self.check_expr_kind(expr, expected);
        let never = self.is_never_like(&ty);
        if !never {
            self.coerce_to_expected(expr, &ty, expected);
        }
        (self.record(expr, ty), never)
    }

    /// Whether `ty` is `!` or a diverging variable nothing has decided yet.
    fn is_never_like(&mut self, ty: &Ty) -> bool {
        match self.shallow(ty) {
            Ty::Never => true,
            Ty::Infer(v) => self.is_diverging_var(v),
            _ => false,
        }
    }

    /// Records a coercion of `found` to `expected` that changes the pointer's
    /// representation (see [`BodyTypes::coercions`]).
    fn note_pointer_coercion(&mut self, id: NodeId, found: &Ty, expected: &Ty) {
        let (Ty::Ref(mf, a), Ty::Ref(me, b)) = (self.shallow(found), self.shallow(expected)) else {
            return;
        };
        let changes = match (self.shallow(&a), self.shallow(&b)) {
            (Ty::Array(..), Ty::Slice(_)) => true,
            (Ty::Slice(_), Ty::Slice(_)) => mf != me,
            _ => false,
        };
        if changes {
            self.coercions.insert(id, expected.clone());
        }
    }

    #[allow(clippy::too_many_lines)]
    fn check_expr_kind(&mut self, expr: &Expr, expected: Option<&Ty>) -> Ty {
        let unsized_place = std::mem::take(&mut self.unsized_place);
        match &expr.kind {
            ExprKind::Lit(lit) => self.check_lit(lit, expr.span, expected),
            ExprKind::Unit => Ty::Unit,
            ExprKind::Tuple(elems) => self.check_tuple(elems, expected),
            ExprKind::Array(elems) => self.check_array(elems, expected),
            ExprKind::Repeat(value, count) => self.check_repeat(expr, value, count, expected),
            ExprKind::Path(_) => self.check_path(expr, false),
            ExprKind::Paren(inner) => {
                self.unsized_place = unsized_place;
                self.check_expr(inner, expected)
            }
            ExprKind::Unary(op, operand) => self.check_unary(*op, operand, expected),
            ExprKind::Binary(op, lhs, rhs) => self.check_binary(*op, lhs, rhs, expected, expr.span),
            ExprKind::Assign(lhs, rhs) => {
                let lhs_ty = self.check_place(lhs);
                self.check_expr(rhs, Some(&lhs_ty));
                Ty::Unit
            }
            ExprKind::DestructuringAssign(pat, rhs) => {
                self.check_destructuring_assign(pat, rhs);
                Ty::Unit
            }
            ExprKind::Underscore => unreachable!("the parser rejects `_` outside an assignee"),
            ExprKind::AssignOp(op, lhs, rhs) => {
                let lhs_ty = self.check_place(lhs);
                let lhs_never = self.is_never_like(&lhs_ty);
                let rhs_expected = if matches!(op, BinOp::Shl | BinOp::Shr) || lhs_never {
                    None
                } else {
                    Some(&lhs_ty)
                };
                let (rhs_ty, rhs_never) = self.check_operand(rhs, rhs_expected);
                if lhs_never || rhs_never {
                    self.never_operands.push(NeverOperand {
                        op: *op,
                        compound: true,
                        lhs: lhs_ty,
                        rhs: rhs_ty,
                        span: expr.span,
                        out: None,
                    });
                } else if matches!(op, BinOp::Shl | BinOp::Shr) {
                    self.check_shift_operands(*op, &lhs_ty, &rhs_ty, expr.span);
                } else {
                    self.check_arith_operands(*op, &lhs_ty, &rhs_ty, expr.span);
                }
                Ty::Unit
            }
            ExprKind::Call { callee, args } => {
                self.check_call(expr.id, callee, args, expected, expr.span)
            }
            ExprKind::MethodCall {
                receiver,
                method,
                generic_args,
                args,
            } => {
                let turbofish = generic_args.as_deref();
                self.check_method_call(expr, receiver, method, turbofish, args, expected)
            }
            ExprKind::Field(base, name) => {
                let base_ty = self.check_expr(base, None);
                self.check_field(expr, &base_ty, &name.name, name.span)
            }
            ExprKind::Try(operand) => self.check_try(expr, operand),
            ExprKind::Index(base, index) => self.check_index(expr, base, index, unsized_place),
            ExprKind::Range(..) => unreachable!("a range is only parsed as the index of `e[..]`"),
            ExprKind::Ref {
                mutable,
                expr: inner,
            } => {
                let m = Mutability::from_bool(*mutable);
                let inner_expected = match expected.map(|e| self.shallow(e)) {
                    // Only a diverging operand fits `&!`; checking against `!` would
                    // report the operand rather than the reference (rustc: "expected
                    // `&!`, found `&{integer}`"). Where a `&[T]` is expected the operand
                    // is an array (`&[1, 2]`, `&a`), which the reference then unsizes:
                    // that coercion unifies the element types.
                    Some(Ty::Ref(_, t))
                        if !t.is_never() && !matches!(self.shallow(&t), Ty::Slice(_)) =>
                    {
                        Some(*t)
                    }
                    // An array literal under a `&[T]` expectation has `T` elements (rustc's
                    // rvalue hint for an unsized referent), so each element is a coercion
                    // site: `let c: &[&[i32]] = &[&[1, 2], &[3]];` (#94).
                    Some(Ty::Ref(_, t)) => match (self.shallow(&t), &strip_parens(inner).kind) {
                        (Ty::Slice(elem), ExprKind::Array(elems)) => Some(Ty::Array(
                            elem,
                            u64::try_from(elems.len()).expect("fewer than 2^64 elements"),
                        )),
                        _ => None,
                    },
                    _ => None,
                };
                let errors_before = self.sink.error_count();
                // A diverging operand gives `&!`, as in rustc: the borrow is not a
                // coercion site, so `let q: &Point = &loop {};` is a mismatch, while
                // `*&return 9` is `!` again. The code after it is unreachable.
                self.unsized_place = true;
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
                        // A slice place is fine where it is borrowed, indexed or measured.
                        let place_ok = unsized_place
                            && matches!(self.shallow(&t), Ty::Slice(_) | Ty::Prim(PrimTy::Str));
                        if !place_ok && !collect::check_sized(&t, expr.span, self.sink) {
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
            ExprKind::Struct { fields, .. } => self.check_struct_literal(expr, fields, expected),
            ExprKind::Match { scrutinee, arms } => {
                self.check_match(expr, scrutinee, arms, expected)
            }
            ExprKind::Let(pat, scrutinee) => {
                // Only in the condition of an `if` or `while`, where it is a `bool`.
                let ty = self.check_expr(scrutinee, None);
                let ty = self.scrutinee_ty(ty, [&**pat]);
                self.check_pat(pat, &ty, Some(scrutinee.span));
                Ty::BOOL
            }
        }
    }

    /// `(a, b, ...)`. With an expected tuple type of the same arity, each element is
    /// checked (and coerced) against its expected element type, which becomes the
    /// element's type, so the tuple as a whole matches the expectation (rustc's
    /// expectation flows into tuple elements the same way). An element that diverges
    /// without an expectation gets a fresh variable that falls back to `!` when nothing
    /// else decides it, as a diverging `let` initialiser does.
    fn check_tuple(&mut self, elems: &[Expr], expected: Option<&Ty>) -> Ty {
        let expected_elems = match expected.map(|e| self.shallow(e)) {
            Some(Ty::Tuple(tys)) if tys.len() == elems.len() => Some(tys),
            _ => None,
        };
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

    /// The element type an array expression is checked against: that of an expected array
    /// or slice (`&[1, 2]` where `&[i32]` is expected).
    fn expected_elem(&mut self, expected: Option<&Ty>) -> Option<Ty> {
        match expected.map(|e| self.shallow(e)) {
            Some(Ty::Array(t, _) | Ty::Slice(t)) => Some(*t),
            _ => None,
        }
    }

    /// `[a, b, c]`: every element has the element type; the first fixes it when nothing
    /// is expected, and each later one is checked against it.
    fn check_array(&mut self, elems: &[Expr], expected: Option<&Ty>) -> Ty {
        let elem = self
            .expected_elem(expected)
            .unwrap_or_else(|| self.infer.new_var(VarKind::General));
        for e in elems {
            self.check_expr(e, Some(&elem));
        }
        let n = u64::try_from(elems.len()).expect("fewer than 2^64 elements");
        Ty::Array(Box::new(elem), n)
    }

    /// `[value; count]`: `count` is a `usize` constant; for a count other than 0 or 1 the
    /// operand is copied, so it must be `Copy` unless it names a `const` item (judged in
    /// `finish`, once its type is known).
    fn check_repeat(
        &mut self,
        expr: &Expr,
        value: &Expr,
        count: &Expr,
        expected: Option<&Ty>,
    ) -> Ty {
        let expected_elem = self.expected_elem(expected);
        let found = self.check_expr(value, expected_elem.as_ref());
        let elem = expected_elem.unwrap_or(found);
        let Some(n) = self.array_len(count) else {
            return Ty::Error;
        };
        self.repeat_counts.insert(expr.id, n);
        let names_const = matches!(strip_parens(value).kind, ExprKind::Path(_))
            && matches!(
                self.resolve.res(strip_parens(value).id),
                Res::Def(d) if self.resolve.def(d).kind == DefKind::Const
            );
        if n > 1 && !names_const {
            self.pending_repeats.push((value.span, elem.clone()));
        }
        Ty::Array(Box::new(elem), n)
    }

    /// `base[index]`: the base looks through references to an array or a slice, and the
    /// index is a `usize`. The result is the element, a place.
    fn check_index(&mut self, expr: &Expr, base: &Expr, index: &Expr, unsized_ok: bool) -> Ty {
        self.unsized_place = true;
        let base_ty = self.check_expr(base, None);
        if let ExprKind::Range(lo, hi, _) = &index.kind {
            return self.check_range_index(
                expr,
                &base_ty,
                lo.as_deref(),
                hi.as_deref(),
                unsized_ok,
            );
        }
        let index_ty = self.check_expr(index, None);
        let mut ty = self.shallow(&base_ty);
        let mut autoderefs = 0;
        while let Ty::Ref(_, inner) = ty {
            ty = self.shallow(&inner);
            autoderefs += 1;
        }
        let elem = match ty {
            Ty::Array(elem, _) | Ty::Slice(elem) => *elem,
            Ty::Error => return Ty::Error,
            Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(base.span, "the type of the indexed value must be known"),
                );
                self.infer.bind_error(&Ty::Infer(v));
                return Ty::Error;
            }
            _ => {
                let s = self.ty_str(&base_ty);
                self.error(
                    Diagnostic::error(
                        ErrorCode::NOT_INDEXABLE,
                        format!("cannot index into a value of type `{s}`"),
                    )
                    .with_label(expr.span, "not an array or a slice")
                    .with_note("only arrays and slices can be indexed"),
                );
                return Ty::Error;
            }
        };
        let usize_ty = Ty::Prim(PrimTy::Usize);
        if !self.shallow(&index_ty).is_never() && self.infer.unify(&index_ty, &usize_ty).is_err() {
            let slice = self.ty_str(&Ty::Slice(Box::new(elem.clone())));
            let found = self.ty_str(&index_ty);
            self.error(
                Diagnostic::error(
                    ErrorCode::INVALID_INDEX_TYPE,
                    format!("the type `{slice}` cannot be indexed by `{found}`"),
                )
                .with_label(index.span, "slice indices are of type `usize`")
                .with_note("convert the index with `as usize`"),
            );
        }
        self.index_derefs.insert(expr.id, autoderefs);
        elem
    }

    /// `e[a..b]` (and the other range forms, #10): `e` is a string, an array or a slice,
    /// through references, and the place is a `str` or a `[T]`, valid only where an
    /// unsized place is (borrowed, indexed, measured). Each bound is a `usize`.
    fn check_range_index(
        &mut self,
        expr: &Expr,
        base_ty: &Ty,
        lo: Option<&Expr>,
        hi: Option<&Expr>,
        unsized_ok: bool,
    ) -> Ty {
        let usize_ty = Ty::Prim(PrimTy::Usize);
        for bound in lo.into_iter().chain(hi) {
            self.check_expr(bound, Some(&usize_ty));
        }
        let mut ty = self.shallow(base_ty);
        let mut autoderefs = 0;
        while let Ty::Ref(_, inner) = ty {
            ty = self.shallow(&inner);
            autoderefs += 1;
        }
        let place_ty = match ty {
            Ty::Prim(PrimTy::Str) => Ty::Prim(PrimTy::Str),
            Ty::Array(elem, _) | Ty::Slice(elem) => Ty::Slice(elem),
            Ty::Error => return Ty::Error,
            Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(expr.span, "the type of the indexed value must be known"),
                );
                self.infer.bind_error(&Ty::Infer(v));
                return Ty::Error;
            }
            _ => {
                let s = self.ty_str(base_ty);
                self.error(
                    Diagnostic::error(
                        ErrorCode::NOT_INDEXABLE,
                        format!("cannot index into a value of type `{s}`"),
                    )
                    .with_label(expr.span, "not a string, an array or a slice")
                    .with_note("only strings, arrays and slices can be indexed by a range"),
                );
                return Ty::Error;
            }
        };
        if !unsized_ok && !collect::check_sized(&place_ty, expr.span, self.sink) {
            return Ty::Error;
        }
        self.index_derefs.insert(expr.id, autoderefs);
        place_ty
    }

    /// `e.len()` where `e` is an array or a slice, through references: a `usize`. `None`
    /// when `e` is something else; another method of an array is reported here.
    fn check_len_call(
        &mut self,
        expr: &Expr,
        recv_ty: &Ty,
        method: &sabiko_ast::Ident,
        args: &[Expr],
    ) -> Option<Ty> {
        let mut ty = self.shallow(recv_ty);
        let mut autoderefs = 0;
        while let Ty::Ref(_, inner) = ty {
            ty = self.shallow(&inner);
            autoderefs += 1;
        }
        if !matches!(ty, Ty::Array(..) | Ty::Slice(_) | Ty::Prim(PrimTy::Str)) {
            return None;
        }
        if method.name != "len" && !self.resolve.trait_items_named(&method.name).is_empty() {
            // A trait's method, which the probe finds (or reports).
            return None;
        }
        let slice_like = !matches!(ty, Ty::Prim(PrimTy::Str));
        if slice_like && self.resolve.slice_method(&method.name).is_some() {
            // A method of the prelude's `impl<T> [T]` (decision 93), which the probe finds.
            return None;
        }
        for a in args {
            self.check_expr(a, None);
        }
        if method.name != "len" {
            let shown = self.ty_str(recv_ty);
            self.error(
                Diagnostic::error(
                    ErrorCode::UNKNOWN_METHOD,
                    format!("no method named `{}` found for `{shown}`", method.name),
                )
                .with_label(method.span, "method not found")
                .with_note(if slice_like {
                    "arrays and slices have `len()` and the methods of `library/core/slice.sabi` for now"
                } else {
                    "strings have one method for now, `len()`"
                }),
            );
            return Some(Ty::Error);
        }
        if !args.is_empty() {
            let d = Self::wrong_arg_count("method", 0, args.len(), expr.span);
            self.error(d);
        }
        self.len_calls.insert(expr.id, autoderefs);
        Some(Ty::Prim(PrimTy::Usize))
    }

    /// Edition 2024 reserves binding modifiers under a reference default binding mode:
    /// `let (mut a, b) = &t;` and `let (ref a, b) = &t;` are errors (rustc's
    /// "implicitly-borrowing pattern"), where 2021 bound by value again or ignored the `ref`.
    fn binding_modifier_under_ref(&mut self, pat: &Pat, explicit_ref: bool, ty: &Ty) {
        let (message, label) = if explicit_ref {
            (
                "cannot explicitly borrow within an implicitly-borrowing pattern",
                "explicit `ref` binding modifier not allowed when implicitly borrowing",
            )
        } else {
            (
                "cannot mutably bind by value within an implicitly-borrowing pattern",
                "`mut` binding modifier not allowed when implicitly borrowing",
            )
        };
        let found = self.ty_str(ty);
        self.error(
            Diagnostic::error(ErrorCode::BINDING_MODIFIER_UNDER_REF, message)
                .with_label(pat.span, label)
                .with_note(format!(
                    "`{found}` is matched through a reference here, so the binding already borrows it (edition 2024)"
                ))
                .with_note("remove the modifier, or match on the value instead of the reference"),
        );
    }

    /// Checks a binding pattern against the type of the value it binds, recording the
    /// type of every local it introduces. `init_span` points at the initialiser, for
    /// diagnostics. A tuple pattern matched against a reference to a tuple looks through
    /// the reference and binds references to the elements (default binding modes: `let (a,
    /// b) = &t;` gives `a: &A`); a `mut`/`ref` modifier in that mode is an error (edition
    /// 2024, [`Self::binding_modifier_under_ref`]).
    fn check_pat(&mut self, pat: &Pat, ty: &Ty, init_span: Option<Span>) {
        self.check_pat_in_mode(pat, ty, None, init_span);
    }

    #[allow(clippy::too_many_lines)]
    fn check_pat_in_mode(
        &mut self,
        pat: &Pat,
        ty: &Ty,
        by_ref: Option<Mutability>,
        init_span: Option<Span>,
    ) {
        self.pat_types.insert(pat.id, ty.clone());
        // A lone identifier that names a constant, a unit struct or a variant is a path
        // pattern by now (the resolver rewrote it), as is an assignee naming one.
        match &pat.kind {
            PatKind::Ident {
                by_ref: explicit_ref,
                mutable,
                name,
                sub,
            } => {
                if by_ref.is_some() && (*explicit_ref || *mutable) {
                    self.binding_modifier_under_ref(pat, *explicit_ref, ty);
                }
                if let Some(local) = self.resolve.local_of_pat(pat.id) {
                    let binding_ty = if *explicit_ref {
                        let m = Mutability::from_bool(*mutable);
                        self.ref_bindings.insert(pat.id, m);
                        Ty::Ref(m, Box::new(ty.clone()))
                    } else {
                        match by_ref {
                            Some(m) if !mutable => {
                                self.ref_bindings.insert(pat.id, m);
                                Ty::Ref(m, Box::new(ty.clone()))
                            }
                            _ => ty.clone(),
                        }
                    };
                    if self.or_bindings.contains(&local) {
                        // A later alternative of an or-pattern: the same type as before.
                        let first = self.local_types[&local].clone();
                        if self.infer.unify(&first, &binding_ty).is_err() {
                            let (f, b) = (self.ty_str(&first), self.ty_str(&binding_ty));
                            self.error(
                                Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
                                    .with_label(name.span, format!("expected `{f}`, found `{b}`"))
                                    .with_note(
                                        "a binding must have the same type in every alternative of an or-pattern",
                                    ),
                            );
                        }
                    } else {
                        self.local_types.insert(local, binding_ty);
                        self.local_spans.insert(local, pat.span);
                    }
                }
                if let Some(sub) = sub {
                    self.check_pat_in_mode(sub, ty, by_ref, init_span);
                }
            }
            PatKind::Wild | PatKind::Rest => {}
            PatKind::Ref { pat: sub, mutable } => {
                let inner = self.check_ref_pat(pat, *mutable, ty, by_ref);
                // A `&` pattern resets the default binding mode to by-value.
                self.check_pat_in_mode(sub, &inner, None, init_span);
            }
            PatKind::Assignee(_) => {
                // A fresh local of the value's type, assigned to the place afterwards.
                if let Some(local) = self.resolve.local_of_pat(pat.id) {
                    self.local_types.insert(local, ty.clone());
                    self.local_spans.insert(local, pat.span);
                }
            }
            PatKind::Or(alts) => {
                let mut bound = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    self.check_pat_in_mode(alt, ty, by_ref, init_span);
                    if i == 0 {
                        alt.walk_idents(&mut |p| {
                            if let Some(l) = self.resolve.local_of_pat(p.id) {
                                bound.push(l);
                            }
                        });
                        for &l in &bound {
                            self.or_bindings.insert(l);
                        }
                    }
                }
                for l in bound {
                    self.or_bindings.remove(&l);
                }
            }
            PatKind::Tuple(elems) => {
                let (ty, by_ref) = self.peel_pat_refs(pat, ty, by_ref);
                let rest = elems.iter().position(|p| matches!(p.kind, PatKind::Rest));
                let fixed = elems.len() - usize::from(rest.is_some());
                let elem_tys = match ty {
                    Ty::Tuple(tys) if rest.is_none() && tys.len() == elems.len() => *tys,
                    Ty::Tuple(tys) if rest.is_some() && tys.len() >= fixed => *tys,
                    Ty::Unit if fixed == 0 => Vec::new(),
                    Ty::Error => vec![Ty::Error; fixed],
                    Ty::Infer(v) if rest.is_none() && self.infer.kind(v) == VarKind::General => {
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
                        vec![Ty::Error; fixed]
                    }
                };
                self.check_subpats(elems, &elem_tys, by_ref, init_span);
            }
            PatKind::Path(_) => {
                let res = self.resolve.pat_res(pat.id).unwrap_or(Res::Err);
                let ty = self.peel_unless_const(pat, res, ty, by_ref);
                self.check_path_pat(pat, res, &ty, init_span);
            }
            PatKind::TupleStruct(path, elems) => {
                let (ty, by_ref) = self.peel_pat_refs(pat, ty, by_ref);
                let rest = elems.iter().position(|p| matches!(p.kind, PatKind::Rest));
                let fixed = elems.len() - usize::from(rest.is_some());
                let field_tys = match self.resolve.pat_res(pat.id) {
                    Some(Res::Def(ctor)) if self.resolve.def(ctor).kind == DefKind::Variant => {
                        let (adt_def, index) = self.variant_of(ctor);
                        let adt = self.items.adt(adt_def).clone();
                        let variant = adt.variant(index);
                        let vname = self.resolve.qualified_name(ctor);
                        match variant.kind {
                            VariantKind::Tuple => {
                                let args = self.expect_enum_pat(pat, adt_def, &ty, init_span);
                                self.pat_variants.insert(pat.id, (adt_def, index));
                                let n = variant.fields.len();
                                if (rest.is_none() && fixed != n) || fixed > n {
                                    self.error(
                                        Diagnostic::error(
                                            ErrorCode::PATTERN_ARITY,
                                            format!(
                                                "this pattern has {} field{}, but the corresponding tuple variant has {n} field{}",
                                                fixed,
                                                if fixed == 1 { "" } else { "s" },
                                                if n == 1 { "" } else { "s" }
                                            ),
                                        )
                                        .with_label(pat.span, format!("expected {n} field{}, found {fixed}", if n == 1 { "" } else { "s" }))
                                        .with_label(variant.span, "tuple variant has fields here"),
                                    );
                                    None
                                } else {
                                    Some(
                                        variant
                                            .fields
                                            .iter()
                                            .map(|f| adt.field_ty(&f.ty, &args))
                                            .collect::<Vec<_>>(),
                                    )
                                }
                            }
                            VariantKind::Unit => {
                                self.shape_error(
                                    path.span,
                                    format!("expected tuple struct or tuple variant, found unit variant `{vname}`"),
                                    "not a tuple struct or tuple variant",
                                );
                                None
                            }
                            VariantKind::Named => {
                                self.shape_error(
                                    path.span,
                                    format!("expected tuple struct or tuple variant, found struct variant `{vname}`"),
                                    "not a tuple struct or tuple variant",
                                );
                                None
                            }
                        }
                    }
                    Some(Res::Def(def)) => {
                        let info = self.resolve.def(def);
                        let what = format!(
                            "expected tuple struct or tuple variant, found {} `{}`",
                            info.kind.describe(),
                            info.name
                        );
                        self.shape_error(path.span, what, "not a tuple struct or tuple variant");
                        None
                    }
                    _ => None,
                };
                let field_tys = field_tys.unwrap_or_else(|| vec![Ty::Error; fixed]);
                self.check_subpats(elems, &field_tys, by_ref, init_span);
            }
            PatKind::Struct { fields, rest, .. } => {
                let (ty, by_ref) = self.peel_pat_refs(pat, ty, by_ref);
                let target = match self.resolve.pat_res(pat.id) {
                    Some(Res::Def(def)) if self.resolve.def(def).kind == DefKind::Variant => {
                        let (adt_def, index) = self.variant_of(def);
                        let args = self.expect_enum_pat(pat, adt_def, &ty, init_span);
                        self.pat_variants.insert(pat.id, (adt_def, index));
                        let fields = self.items.adt(adt_def).variant(index).fields.clone();
                        Some((
                            self.instance_fields(adt_def, &fields, &args),
                            format!("variant `{}`", self.resolve.qualified_name(def)),
                        ))
                    }
                    Some(Res::Def(def)) if self.resolve.def(def).kind == DefKind::Struct => {
                        let args = match pat_path(pat) {
                            Some(path) => self.adt_args(def, path),
                            None => self.fresh_adt_args(def),
                        };
                        let inst = Ty::Adt(def, args.clone().into());
                        if self.try_coerce(&inst, &ty).is_err() {
                            self.mismatch(pat.span, &ty, &inst);
                        }
                        let adt = self.items.adt(def);
                        let (fields, name) = (adt.fields.clone(), adt.name.clone());
                        Some((
                            self.instance_fields(def, &fields, &args),
                            format!("struct `{name}`"),
                        ))
                    }
                    _ => None,
                };
                let Some((defs, what)) = target else {
                    for f in fields {
                        self.check_pat_in_mode(&f.pat, &Ty::Error, by_ref, init_span);
                    }
                    return;
                };
                let mut seen = vec![false; defs.len()];
                for f in fields {
                    if let Some(i) = defs.iter().position(|d| d.name == f.name.name) {
                        if seen[i] {
                            self.error(
                                Diagnostic::error(
                                    ErrorCode::DUPLICATE_FIELD_INIT,
                                    format!(
                                        "field `{}` bound multiple times in the pattern",
                                        f.name.name
                                    ),
                                )
                                .with_label(f.name.span, "multiple uses of field"),
                            );
                        }
                        seen[i] = true;
                        let fty = defs[i].ty.clone();
                        self.check_pat_in_mode(&f.pat, &fty, by_ref, init_span);
                    } else {
                        self.error(
                            Diagnostic::error(
                                ErrorCode::UNKNOWN_FIELD,
                                format!("{what} does not have a field named `{}`", f.name.name),
                            )
                            .with_label(f.name.span, format!("{what} does not have this field")),
                        );
                        self.check_pat_in_mode(&f.pat, &Ty::Error, by_ref, init_span);
                    }
                }
                if rest.is_none() {
                    let missing: Vec<String> = defs
                        .iter()
                        .zip(&seen)
                        .filter(|(_, s)| !**s)
                        .map(|(d, _)| format!("`{}`", d.name))
                        .collect();
                    if !missing.is_empty() {
                        let list = join_and(&missing);
                        self.error(
                            Diagnostic::error(
                                ErrorCode::MISSING_FIELDS,
                                format!(
                                    "pattern does not mention field{} {list}",
                                    if missing.len() == 1 { "" } else { "s" }
                                ),
                            )
                            .with_label(
                                pat.span,
                                format!(
                                    "missing field{} {list}",
                                    if missing.len() == 1 { "" } else { "s" }
                                ),
                            )
                            .with_note("ignore the other fields with `..`"),
                        );
                    }
                }
            }
            PatKind::Lit(e) => {
                // A string literal matches a `&str` itself; any other literal looks
                // through references.
                // (rustc does not peel references for it either: `match &s { "a" => .. }`
                // with `s: &str` is a mismatch.)
                let ty = if matches!(e.kind, ExprKind::Lit(Lit::Str(_))) {
                    ty.clone()
                } else {
                    self.peel_pat_refs(pat, ty, by_ref).0
                };
                self.check_pat_expr(e, &ty);
            }
            PatKind::Range(lo, hi, _) => {
                let (ty, _) = self.peel_pat_refs(pat, ty, by_ref);
                for bound in [lo, hi].into_iter().flatten() {
                    self.check_pat_expr(bound, &ty);
                }
                let t = self.shallow(&ty);
                let ok = match &t {
                    Ty::Prim(p) => p.is_numeric() || *p == PrimTy::Char,
                    Ty::Infer(v) => matches!(self.infer.kind(*v), VarKind::Int | VarKind::Float),
                    Ty::Error => true,
                    _ => false,
                };
                if !ok {
                    let found = self.ty_str(&t);
                    self.error(
                        Diagnostic::error(
                            ErrorCode::TYPE_MISMATCH,
                            "only `char` and numeric types are allowed in range patterns",
                        )
                        .with_label(
                            pat.span,
                            format!("this is of type `{found}` but it should be `char` or numeric"),
                        ),
                    );
                }
            }
        }
    }

    /// A `&p` or `&mut p` pattern (#49) against a value of `ty`: the type its subpattern
    /// matches. The pattern does not look through references (`&p` against `&&T` matches
    /// the outer one); a non-reference type or a reference of the other mutability is a
    /// mismatch (rustc's E0308), and under a by-reference default binding mode the pattern
    /// is E0337, edition 2024's "cannot explicitly dereference within an
    /// implicitly-borrowing pattern".
    fn check_ref_pat(
        &mut self,
        pat: &Pat,
        mutable: bool,
        ty: &Ty,
        by_ref: Option<Mutability>,
    ) -> Ty {
        let m = Mutability::from_bool(mutable);
        let written = if mutable { "&mut _" } else { "&_" };
        let inner = match self.shallow(ty) {
            Ty::Ref(found, inner) if found == m => *inner,
            Ty::Error => return Ty::Error,
            Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                let inner = self.infer.new_var(VarKind::General);
                let _ = self
                    .infer
                    .unify(&Ty::Infer(v), &Ty::Ref(m, Box::new(inner.clone())));
                inner
            }
            found @ Ty::Ref(..) => {
                let f = self.ty_str(&found);
                self.error(
                    Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
                        .with_label(pat.span, "types differ in mutability")
                        .with_note(format!("expected `{f}`, found `{written}`")),
                );
                return Ty::Error;
            }
            found => {
                let f = self.ty_str(&found);
                let mut d = Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
                    .with_label(pat.span, format!("expected `{f}`, found `{written}`"));
                if !matches!(found, Ty::Infer(_)) {
                    d = d.with_note(format!(
                        "a reference pattern matches a reference; `{f}` is not one"
                    ));
                }
                self.error(d);
                return Ty::Error;
            }
        };
        if by_ref.is_some() {
            let found = self.ty_str(ty);
            self.error(
                Diagnostic::error(
                    ErrorCode::BINDING_MODIFIER_UNDER_REF,
                    "cannot explicitly dereference within an implicitly-borrowing pattern",
                )
                .with_label(pat.span, "reference pattern not allowed when implicitly borrowing")
                .with_note(format!(
                    "`{found}` is matched through a reference here, so the pattern already borrows it (edition 2024)"
                ))
                .with_note("remove the `&`, or match on the value instead of the reference"),
            );
        }
        inner
    }

    /// Checks `pats` (with at most one `..`) against `tys`, the types of the elements or
    /// fields; `..` stands for the ones in the middle.
    fn check_subpats(
        &mut self,
        pats: &[Pat],
        tys: &[Ty],
        by_ref: Option<Mutability>,
        init_span: Option<Span>,
    ) {
        for (p, ty) in pats.iter().zip(rest_positions(pats, tys.len())) {
            match ty {
                Some(i) => {
                    let t = tys.get(i).cloned().unwrap_or(Ty::Error);
                    self.check_pat_in_mode(p, &t, by_ref, init_span);
                }
                None => self.pat_types.insert(p.id, Ty::Error).map_or((), |_| ()),
            }
        }
    }

    /// A pattern that is not a binding matched against a reference looks through it
    /// (default binding modes): every reference peeled makes the default
    /// binding mode `ref` (`ref mut` for `&mut`, unless a `&` was already crossed).
    fn peel_pat_refs(
        &mut self,
        pat: &Pat,
        ty: &Ty,
        by_ref: Option<Mutability>,
    ) -> (Ty, Option<Mutability>) {
        let mut ty = self.shallow(ty);
        if self.in_assignee {
            return (ty, by_ref);
        }
        let mut by_ref = by_ref;
        let mut derefs = 0;
        while let Ty::Ref(m, inner) = ty {
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
        (ty, by_ref)
    }

    /// A path pattern (`E::V`, a constant, a unit struct) against a value of `ty`
    /// (references already peeled).
    /// The type a path pattern is checked against. A unit struct or variant peels
    /// references off the matched type (default binding modes), a constant does not, as
    /// in rustc (`AdjustMode::Pass`): `match &mut k { Z => .. }` is E0300 for `const Z:
    /// u16`, though a literal there would peel.
    fn peel_unless_const(
        &mut self,
        pat: &Pat,
        res: Res,
        ty: &Ty,
        by_ref: Option<Mutability>,
    ) -> Ty {
        if matches!(res, Res::Def(def) if self.resolve.def(def).kind == DefKind::Const) {
            return ty.clone();
        }
        self.peel_pat_refs(pat, ty, by_ref).0
    }

    fn check_path_pat(&mut self, pat: &Pat, res: Res, ty: &Ty, init_span: Option<Span>) {
        let Res::Def(def) = res else {
            return;
        };
        let info = self.resolve.def(def);
        match info.kind {
            DefKind::Variant => {
                let (adt_def, index) = self.variant_of(def);
                let kind = self.items.adt(adt_def).variant(index).kind;
                let name = self.resolve.qualified_name(def);
                match kind {
                    VariantKind::Unit => {
                        self.expect_enum_pat(pat, adt_def, ty, init_span);
                        self.pat_variants.insert(pat.id, (adt_def, index));
                    }
                    VariantKind::Tuple => self.shape_error(
                        pat.span,
                        format!("expected unit struct, unit variant or constant, found tuple variant `{name}`"),
                        "not a unit struct, unit variant or constant",
                    ),
                    VariantKind::Named => self.shape_error(
                        pat.span,
                        format!("expected unit struct, unit variant or constant, found struct variant `{name}`"),
                        "not a unit struct, unit variant or constant",
                    ),
                }
            }
            DefKind::Const => {
                let cty = match pat_path(pat) {
                    Some(path) if self.resolve.parent_kind(def) == Some(DefKind::Trait) => {
                        self.check_trait_const_path(pat.id, pat.span, path, def)
                    }
                    Some(path) if !self.resolve.generics(def).is_empty() => {
                        self.check_generic_const_path(pat.id, pat.span, path, def)
                    }
                    _ => self.items.const_ty(def).clone(),
                };
                if self.try_coerce(&cty, ty).is_err() {
                    self.pat_type_mismatch(pat.span, ty, &cty, init_span);
                }
            }
            DefKind::Struct if self.items.adt(def).is_unit => {
                let args = match pat_path(pat) {
                    Some(path) => self.adt_args(def, path),
                    None => self.fresh_adt_args(def),
                };
                let inst = Ty::Adt(def, args.into());
                if self.try_coerce(&inst, ty).is_err() {
                    self.pat_type_mismatch(pat.span, ty, &inst, init_span);
                }
            }
            kind => {
                let name = info.name.clone();
                self.shape_error(
                    pat.span,
                    format!(
                        "expected unit struct, unit variant or constant, found {} `{name}`",
                        kind.describe()
                    ),
                    "not a unit struct, unit variant or constant",
                );
            }
        }
    }

    /// The enum and index of a variant definition.
    fn variant_of(&self, variant: DefId) -> (DefId, usize) {
        let parent = self
            .resolve
            .def(variant)
            .parent
            .expect("a variant has its enum as parent");
        (parent, self.resolve.variant_index(variant))
    }

    /// The value matched by a variant pattern must be of the variant's enum; returns the
    /// enum instance's generic arguments (see [`Self::adt_args`]).
    fn expect_enum_pat(
        &mut self,
        pat: &Pat,
        adt: DefId,
        ty: &Ty,
        init_span: Option<Span>,
    ) -> Vec<Ty> {
        let args = match pat_path(pat) {
            Some(path) => self.adt_args(adt, path),
            None => self.fresh_adt_args(adt),
        };
        let inst = Ty::Adt(adt, args.clone().into());
        if self.try_coerce(&inst, ty).is_err() {
            self.pat_type_mismatch(pat.span, ty, &inst, init_span);
        }
        args
    }

    /// The generic arguments of the struct or enum `adt` as `path` names it (in an
    /// expression or a pattern): the ones written after any segment (`Opt::<i32>::None`,
    /// `Opt::None::<i32>`), its own parameters for `Self` in its `impl` block, or else
    /// fresh inference variables. Empty for a type that is not generic.
    fn adt_args(&mut self, adt: DefId, path: &sabiko_ast::Path) -> Vec<Ty> {
        let generics = self.items.adt(adt).generics.clone();
        if generics.is_empty() {
            return Vec::new();
        }
        if path.segments[0].name == "Self" {
            // In a trait `impl` block, the arguments of its type.
            if let Some(Ty::Adt(d, args)) = &self.self_ty {
                if *d == adt && args.len() == generics.len() {
                    return args.to_vec();
                }
            }
            return generics.iter().map(|&p| Ty::Param(p)).collect();
        }
        // The resolver checked each list's length; one that does not fit was reported.
        let written = path
            .generic_args
            .iter()
            .find(|a| a.args.len() == generics.len());
        match written {
            Some(a) => a.args.iter().map(|t| self.lower_type(t)).collect(),
            None => self.fresh_adt_args(adt),
        }
    }

    /// Fresh inference variables for the generic arguments of `adt`.
    fn fresh_adt_args(&mut self, adt: DefId) -> Vec<Ty> {
        (0..self.items.adt(adt).generics.len())
            .map(|_| self.infer.new_var(VarKind::General))
            .collect()
    }

    /// The fields `fields` of `adt` at the instance with generic arguments `args`.
    fn instance_fields(&self, adt: DefId, fields: &[FieldDef], args: &[Ty]) -> Vec<FieldDef> {
        let def = self.items.adt(adt);
        fields
            .iter()
            .map(|f| FieldDef {
                ty: def.field_ty(&f.ty, args),
                ..f.clone()
            })
            .collect()
    }

    fn pat_type_mismatch(
        &mut self,
        span: Span,
        expected: &Ty,
        found: &Ty,
        init_span: Option<Span>,
    ) {
        let (e, f) = (self.ty_str(expected), self.ty_str(found));
        let mut d = Diagnostic::error(ErrorCode::TYPE_MISMATCH, "mismatched types")
            .with_label(span, format!("expected `{e}`, found `{f}`"));
        if let Some(init) = init_span {
            d = d.with_label(init, format!("this expression has type `{e}`"));
        }
        self.error(d);
    }

    fn shape_error(&mut self, span: Span, message: String, label: &str) {
        self.error(
            Diagnostic::error(ErrorCode::VARIANT_SHAPE_MISMATCH, message).with_label(span, label),
        );
    }

    /// A literal or a range bound in a pattern: an expression of the matched type.
    fn check_pat_expr(&mut self, e: &Expr, ty: &Ty) {
        self.check_expr(e, Some(ty));
    }

    /// The type a `match` (with arms) or `let` pattern is checked against for a scrutinee of
    /// type `ty`: rustc's `demand_scrutinee_type`. A diverging scrutinee coerces to a fresh
    /// variable the patterns decide (`match return 1 { E::A(b) => b, E::B => 2 }`), which
    /// falls back to `!` when they do not; with an explicit `ref`/`ref mut` binding in a
    /// pattern the scrutinee's own type is used, as rustc needs the precise type there
    /// (#99).
    fn scrutinee_ty<'p>(&mut self, ty: Ty, pats: impl IntoIterator<Item = &'p Pat>) -> Ty {
        let mut pats = pats.into_iter();
        if !self.shallow(&ty).is_never() || pats.any(has_explicit_ref_binding) {
            return ty;
        }
        let v = self.infer.new_var(VarKind::General);
        self.diverging_vars.push(v.clone());
        v
    }

    /// `match scrutinee { arms }`. The arms' bodies are checked against the expectation,
    /// or else against each other (the first that does not diverge decides); guards are
    /// `bool`. The match diverges when its scrutinee does or every arm's body does; a
    /// `match` without arms has type `!`.
    fn check_match(
        &mut self,
        expr: &Expr,
        scrutinee: &Expr,
        arms: &[Arm],
        expected: Option<&Ty>,
    ) -> Ty {
        let scrut_ty = self.check_expr(scrutinee, None);
        let scrut_ty = if arms.is_empty() {
            scrut_ty
        } else {
            self.scrutinee_ty(scrut_ty, arms.iter().map(|a| &a.pat))
        };
        let scrut_diverges = self.diverges;
        // The caller coerces the `match` to an expectation dropped here.
        let expected = self.expectation_for_branches(expected);
        let expected = expected.as_ref();
        let mut result: Option<Ty> = expected.cloned();
        let mut first_ty: Option<(Ty, Span)> = None;
        let mut all_diverge = true;
        for arm in arms {
            self.diverges = false;
            self.check_pat(&arm.pat, &scrut_ty, Some(scrutinee.span));
            if let Some(g) = &arm.guard {
                self.check_expr(g, Some(&Ty::BOOL));
            }
            self.diverges = false;
            let ty = match (&result, expected) {
                (_, Some(exp)) => self.check_expr(&arm.body, Some(exp)),
                (Some(r), None) => {
                    let r = r.clone();
                    let found = self.check_expr(&arm.body, None);
                    // Coerced to the first arm's type, which is known: a `&mut` place
                    // there is reborrowed (rustc's least-upper-bound coercion).
                    if !matches!(
                        strip_parens(&arm.body).kind,
                        ExprKind::Block(_) | ExprKind::If { .. }
                    ) {
                        self.note_reborrow(strip_parens(&arm.body), &found, &r);
                    }
                    if self.try_coerce(&found, &r).is_err() {
                        let (e, f) = (self.ty_str(&r), self.ty_str(&found));
                        let mut d = Diagnostic::error(
                            ErrorCode::TYPE_MISMATCH,
                            "`match` arms have incompatible types",
                        )
                        .with_label(arm.body.span, format!("expected `{e}`, found `{f}`"));
                        if let Some((_, span)) = &first_ty {
                            d = d.with_label(*span, format!("this is found to be of type `{e}`"));
                        }
                        self.error(d.with_label(expr.span, "`match` arms have incompatible types"));
                    }
                    found
                }
                (None, None) => {
                    let found = self.check_expr(&arm.body, None);
                    if !self.shallow(&found).is_never() {
                        result = Some(found.clone());
                        first_ty = Some((found.clone(), arm.body.span));
                    }
                    found
                }
            };
            let _ = ty;
            all_diverge &= self.diverges;
        }
        self.diverges = scrut_diverges || all_diverge;
        if arms.is_empty() {
            return Ty::Never;
        }
        match result {
            Some(t) if expected.is_none() => t,
            Some(t) => {
                // Every arm was coerced to the expectation; `!` arms only diverge.
                if all_diverge {
                    Ty::Never
                } else {
                    t
                }
            }
            None => Ty::Never,
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
                    DefKind::Fn => self.check_fn_path(expr, path, d, callee),
                    DefKind::TyParam | DefKind::Trait => {
                        unreachable!("the resolver rejects a type parameter or a trait as a value")
                    }
                    DefKind::Impl => {
                        self.error(
                            Diagnostic::error(
                                ErrorCode::WRONG_NAMESPACE,
                                "expected value, found `Self`",
                            )
                            .with_label(path.span, "`Self` is not a unit struct here"),
                        );
                        Ty::Error
                    }
                    DefKind::Const if self.resolve.parent_kind(d) == Some(DefKind::Trait) => {
                        self.check_trait_const_path(expr.id, expr.span, path, d)
                    }
                    DefKind::Const if !self.resolve.generics(d).is_empty() => {
                        self.check_generic_const_path(expr.id, expr.span, path, d)
                    }
                    DefKind::Const => self.items.const_ty(d).clone(),
                    DefKind::Variant => {
                        let (adt, index) = self.variant_of(d);
                        let kind = self.items.adt(adt).variant(index).kind;
                        let name = self.resolve.qualified_name(d);
                        match kind {
                            VariantKind::Unit => {
                                self.variant_exprs.insert(expr.id, (adt, index));
                                let args = self.adt_args(adt, path);
                                Ty::Adt(adt, args.into())
                            }
                            VariantKind::Tuple if callee => Ty::FnDef(d),
                            VariantKind::Tuple => {
                                self.error(
                                    Diagnostic::error(
                                        ErrorCode::FN_ITEM_AS_VALUE,
                                        format!("the constructor of variant `{name}` cannot be used as a value yet"),
                                    )
                                    .with_label(expr.span, "tuple-variant constructors are only callable")
                                    .with_note("function pointers and closures are planned for Phase 6; see docs/PLAN.md"),
                                );
                                Ty::Error
                            }
                            VariantKind::Named => {
                                self.shape_error(
                                    path.span,
                                    format!("expected value, found struct variant `{name}`"),
                                    "not a value",
                                );
                                Ty::Error
                            }
                        }
                    }
                    DefKind::Enum => {
                        let name = info.name.clone();
                        self.error(
                            Diagnostic::error(
                                ErrorCode::WRONG_NAMESPACE,
                                format!("expected value, found enum `{name}`"),
                            )
                            .with_label(path.span, "not a value")
                            .with_note("a variant is named through its enum: `Self::Variant`"),
                        );
                        Ty::Error
                    }
                    DefKind::Struct => {
                        let adt = self.items.adt(d);
                        if adt.is_unit {
                            let args = self.adt_args(d, path);
                            Ty::Adt(d, args.into())
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

    /// A path naming the function `def`: callable only (E0317), and for a trait's member
    /// the member the type implements, with its `Self`.
    fn check_fn_path(
        &mut self,
        expr: &Expr,
        path: &sabiko_ast::Path,
        def: DefId,
        callee: bool,
    ) -> Ty {
        if !callee {
            let name = self.resolve.def(def).name.clone();
            self.error(
                Diagnostic::error(
                    ErrorCode::FN_ITEM_AS_VALUE,
                    format!("function `{name}` cannot be used as a value yet"),
                )
                .with_label(expr.span, "function items are only callable")
                .with_note(
                    "function pointers and closures are planned for Phase 6; see docs/PLAN.md",
                ),
            );
            return Ty::Error;
        }
        let mut def = def;
        let mut self_ty = None;
        if self.resolve.parent_kind(def) == Some(DefKind::Trait) {
            let Some((chosen, ty)) = self.select_trait_item(expr.id, path, def) else {
                return Ty::Error;
            };
            if chosen != def {
                self.path_defs.insert(expr.id, chosen);
            }
            def = chosen;
            self_ty = Some(ty);
        }
        self.instantiate(expr.id, def, path, expr.span, self_ty);
        Ty::FnDef(def)
    }

    /// `T::N`, `Self::N` or `S::N` naming a trait's associated constant (#116): the
    /// constant of the trait among those with one of that name that the type implements,
    /// at that type, which must implement it. `Trait::N` names no type to take it from
    /// (E0347, rustc's E0790).
    fn check_trait_const_path(
        &mut self,
        node: NodeId,
        span: Span,
        path: &sabiko_ast::Path,
        def: DefId,
    ) -> Ty {
        let by_trait = matches!(
            self.resolve.assoc_self(node),
            Some(Res::Def(d)) if self.resolve.def(d).kind == DefKind::Trait
        );
        if by_trait {
            let name = self.resolve.def(def).name.clone();
            let tr = self.resolve.def(def).parent.expect("a trait member");
            let trait_name = self.resolve.def(tr).name.clone();
            self.error(
                Diagnostic::error(
                    ErrorCode::TRAIT_CONST_WITHOUT_TYPE,
                    "cannot refer to the associated constant on trait without specifying the corresponding `impl` type",
                )
                .with_label(path.span, "cannot refer to the associated constant of trait")
                .with_note(format!(
                    "name the type that implements `{trait_name}`: `Type::{name}`"
                )),
            );
            return Ty::Error;
        }
        let Some((chosen, self_ty)) = self.select_trait_item(node, path, def) else {
            return Ty::Error;
        };
        if chosen != def {
            self.path_defs.insert(node, chosen);
        }
        let tr = self.resolve.def(chosen).parent.expect("a trait member");
        self.obligations.push(Obligation {
            ty: self_ty.clone(),
            trait_: tr,
            span,
        });
        self.generic_args
            .insert(node, (vec![self_ty.clone()], chosen, span));
        self.items
            .const_ty(chosen)
            .subst(&[self.resolve.trait_self(tr)], &[self_ty])
    }

    /// `W::<i32>::N` or `Self::N` naming a constant of a generic struct's or enum's
    /// `impl` block (decision 85): at the type's generic arguments, which must satisfy
    /// the block's bounds.
    fn check_generic_const_path(
        &mut self,
        node: NodeId,
        span: Span,
        path: &sabiko_ast::Path,
        def: DefId,
    ) -> Ty {
        let adt = self
            .resolve
            .def(def)
            .parent
            .expect("an associated constant");
        let mut type_path = path.clone();
        type_path.segments.truncate(1);
        type_path.generic_args.retain(|a| a.segment == 0);
        let generics = self.resolve.generics(def).to_vec();
        let mut args = self.adt_args(adt, &type_path);
        if args.len() != generics.len() {
            args = vec![Ty::Error; generics.len()];
        }
        for pred in self.items.param_env(def) {
            self.obligations.push(Obligation {
                ty: Ty::Param(pred.param).subst(&generics, &args),
                trait_: pred.trait_,
                span,
            });
        }
        self.generic_args.insert(node, (args.clone(), def, span));
        self.items.const_ty(def).subst(&generics, &args)
    }

    /// `e?` (decision 93): `e` is an `Option<T>` or a `Result<T, E>` of the prelude and
    /// the function returns the same kind; the expression is the `T`. A `Result`'s error
    /// converts to the function's error type with `From` (judged in `finish`). Each misuse
    /// is E0350 with rustc's E0277 message.
    #[allow(clippy::too_many_lines)]
    fn check_try(&mut self, expr: &Expr, operand: &Expr) -> Ty {
        let ty = self.check_expr(operand, None);
        let question = Span::new(expr.span.file, operand.span.end, expr.span.end);
        let ty = self.shallow(&ty);
        let found = match &ty {
            Ty::Adt(def, args) => self.resolve.core_item_of(*def).map(|k| (k, args.clone())),
            _ => None,
        };
        let Some((kind, args)) = found else {
            match ty {
                Ty::Error => {}
                Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                    self.error(
                        Diagnostic::error(
                            ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                            "type annotations needed",
                        )
                        .with_label(operand.span, "cannot infer type")
                        .with_note("the type of the operand of `?` must be known at this point"),
                    );
                    self.infer.bind_error(&ty);
                }
                other => {
                    let shown = self.ty_str(&other);
                    self.error(
                        Diagnostic::error(
                            ErrorCode::INVALID_TRY,
                            "the `?` operator can only be applied to values that implement `Try`",
                        )
                        .with_label(
                            expr.span,
                            format!("the `?` operator cannot be applied to type `{shown}`"),
                        )
                        .with_note("`?` applies to an `Option` or a `Result`"),
                    );
                }
            }
            return Ty::Error;
        };
        let value = args[0].clone();
        let Some(fn_span) = self.fn_span else {
            self.error(
                Diagnostic::error(
                    ErrorCode::INVALID_TRY,
                    "the `?` operator can only be used in a function",
                )
                .with_label(
                    question,
                    "cannot use the `?` operator outside a function body",
                )
                .with_note(
                    "`?` returns early from the enclosing function, and a constant has none",
                ),
            );
            return Ty::Error;
        };
        let ret = self.ret_ty.clone();
        let ret = self.shallow(&ret);
        let ret_kind = match &ret {
            Ty::Adt(def, ret_args) => self
                .resolve
                .core_item_of(*def)
                .map(|k| (k, ret_args.clone())),
            _ => None,
        };
        match ret_kind {
            Some((ret_kind, ret_args)) if ret_kind == kind => {
                if kind == CoreItem::Result {
                    self.pending_tries.push(PendingTry {
                        from: args[1].clone(),
                        to: ret_args[1].clone(),
                        span: question,
                        operand_span: operand.span,
                    });
                }
                self.try_exprs.insert(expr.id, kind);
                value
            }
            Some(_) => {
                let (message, help, returns) = match kind {
                    CoreItem::Option => (
                        "the `?` operator can only be used on `Result`s, not `Option`s, in a function that returns `Result`",
                        format!(
                            "use `.ok_or(...)?` to provide an error compatible with `{}`",
                            self.ty_str(&ret)
                        ),
                        "this function returns a `Result`",
                    ),
                    CoreItem::Result => (
                        "the `?` operator can only be used on `Option`s, not `Result`s, in a function that returns `Option`",
                        "use `.ok()?` if you want to discard the error information".to_owned(),
                        "this function returns an `Option`",
                    ),
                };
                self.error(
                    Diagnostic::error(ErrorCode::INVALID_TRY, message)
                        .with_label(question, help)
                        .with_label(fn_span, returns),
                );
                Ty::Error
            }
            None if ret.is_error() => Ty::Error,
            None => {
                let shown = self.ty_str(&ret);
                self.error(
                    Diagnostic::error(
                        ErrorCode::INVALID_TRY,
                        "the `?` operator can only be used in a function that returns `Result` or `Option`",
                    )
                    .with_label(
                        question,
                        format!("cannot use the `?` operator in a function that returns `{shown}`"),
                    )
                    .with_label(
                        fn_span,
                        "this function should return `Result` or `Option` to accept `?`",
                    ),
                );
                Ty::Error
            }
        }
    }

    /// Lets the error conversions of `?` guide inference before literals default, as
    /// rustc's `To: From<?E>` obligation does: when the error type `?E` of the operand is
    /// not known yet and exactly one of the standard library's impls `From<S> for To`
    /// has a source `S` it could be, it is that source. `let x: i32 = Err(5)?;` in a
    /// function returning `Result<i32, u8>` makes the literal a `u8` (only the reflexive
    /// impl takes an integer); for an `i64` error, `From<i8>`, `From<i16>`, ... all could
    /// apply, so the literal defaults to `i32`, which `From<i32> for i64` converts.
    fn select_try_conversions(&mut self) {
        let pending: Vec<(Ty, Ty)> = self
            .pending_tries
            .iter()
            .map(|t| (t.from.clone(), t.to.clone()))
            .collect();
        for (from, to) in pending {
            let from = self.infer.resolve(&from);
            let to = self.infer.resolve(&to);
            if !from.has_infer() || to.has_infer() {
                continue;
            }
            let candidates: Vec<Ty> = self
                .conversion_sources(&to)
                .into_iter()
                .filter(|source| self.could_be(source, &[], &from))
                .collect();
            if let [source] = candidates.as_slice() {
                // The candidate was checked to fit.
                let _ = self.infer.unify(source, &from);
            }
        }
    }

    /// The error conversions of `?` on a `Result` (decision 93). rustc returns
    /// `Err(From::from(e))`; `From` is a generic trait, which Sabiko does not have yet
    /// (#117), so only the reflexive `impl<T> From<T> for T` is available: the same error
    /// type converts. A conversion the standard library has (`From<i32> for i64`,
    /// [`Self::conversion_sources`]) is not yet supported (E0107); any other is E0351 with
    /// rustc's message, since no program can implement `From` yet.
    fn check_pending_tries(&mut self) {
        for t in std::mem::take(&mut self.pending_tries) {
            let from = self.infer.resolve_with_defaults(&t.from);
            let to = self.infer.resolve_with_defaults(&t.to);
            if from == to {
                continue;
            }
            if has_error(&from) || has_error(&to) {
                if self.sink.error_count() == 0 {
                    // An error type with nothing reported: a type Sabiko could not lower
                    // (an array as a generic argument, #137). Never compile the `?`.
                    self.error(
                        Diagnostic::error(
                            ErrorCode::UNSUPPORTED_SYNTAX,
                            "not yet supported: the error type of this `?`",
                        )
                        .with_label(t.span, "the error type could not be lowered")
                        .with_note(
                            "an array as a generic argument in a type is not supported yet (#137)",
                        ),
                    );
                }
                continue;
            }
            if from.has_infer() || to.has_infer() {
                // An unknown error type (`Ok(1)?`), as rustc reports it (E0282).
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(
                        t.operand_span,
                        "cannot infer the error type of this `Result`",
                    ),
                );
                self.infer.bind_error(&from);
                continue;
            }
            let (from_s, to_s) = (self.ty_str(&from), self.ty_str(&to));
            if from.is_never() || self.conversion_sources(&to).contains(&from) {
                self.error(
                    Diagnostic::error(
                        ErrorCode::UNSUPPORTED_SYNTAX,
                        format!(
                            "not yet supported: `?` converting the error from `{from_s}` to `{to_s}`"
                        ),
                    )
                    .with_label(
                        t.span,
                        format!("Rust converts it with `From<{from_s}> for {to_s}`"),
                    )
                    .with_note("`?` returns an error of the function's own error type for now: `From` waits for generic traits (#117)"),
                );
            } else {
                self.error(
                    Diagnostic::error(
                        ErrorCode::TRY_ERROR_CONVERSION,
                        format!("`?` couldn't convert the error to `{to_s}`"),
                    )
                    .with_label(
                        t.span,
                        format!("the trait `From<{from_s}>` is not implemented for `{to_s}`"),
                    )
                    .with_label(
                        t.operand_span,
                        format!(
                            "this can't be annotated with `?` because it has type `Result<_, {from_s}>`"
                        ),
                    )
                    .with_note("the question mark operation (`?`) implicitly performs a conversion on the error value using the `From` trait"),
                );
            }
        }
    }

    /// The sources `S` of Rust's standard library's impls `From<S> for to`, among the
    /// types Sabiko has: `to` itself (the reflexive impl), the primitives that convert to
    /// a primitive losslessly, `T` for `Option<T>` and `&Option<T>` for `Option<&T>`
    /// (`&mut` likewise), and an array of `N` (up to 12) `T`s for a tuple of `N` `T`s and
    /// the other way round. `From<!> for T` is left out: rustc's inference does not
    /// consider it either. A program cannot implement `From` yet, so these are all the
    /// conversions there are (checked against rustc on every pair of a table of types,
    /// `crates/sabiko_driver/tests/try_conversions.rs`).
    fn conversion_sources(&self, to: &Ty) -> Vec<Ty> {
        let mut sources = vec![to.clone()];
        match to {
            Ty::Prim(p) => sources.extend(
                PrimTy::ALL
                    .into_iter()
                    .filter(|&q| q != *p && prim_from(q, *p))
                    .map(Ty::Prim),
            ),
            Ty::Adt(def, args) if self.resolve.core_item_of(*def) == Some(CoreItem::Option) => {
                sources.push(args[0].clone());
                if let Ty::Ref(m, inner) = &args[0] {
                    let option = Ty::Adt(*def, vec![(**inner).clone()].into());
                    sources.push(Ty::Ref(*m, Box::new(option)));
                }
            }
            Ty::Array(elem, n) if (1..=12).contains(n) => {
                let n = usize::try_from(*n).expect("at most 12");
                sources.push(Ty::Tuple(Box::new(vec![(**elem).clone(); n])));
            }
            Ty::Tuple(elems)
                if (1..=12).contains(&elems.len()) && elems.iter().all(|e| *e == elems[0]) =>
            {
                let n = u64::try_from(elems.len()).expect("at most 12");
                sources.push(Ty::Array(Box::new(elems[0].clone()), n));
            }
            _ => {}
        }
        sources
    }

    fn check_unary(&mut self, op: UnOp, operand: &Expr, expected: Option<&Ty>) -> Ty {
        if op == UnOp::Neg {
            let inner = std::mem::take(&mut self.double_negation);
            let operand = &strip_parens(operand).kind;
            self.negating = !inner && matches!(operand, ExprKind::Lit(Lit::Int { .. }));
            self.double_negation = matches!(operand, ExprKind::Unary(UnOp::Neg, _));
        }
        let (ty, never) = self.check_operand(operand, expected);
        self.negating = false;
        self.double_negation = false;
        let resolved = self.shallow(&ty);
        if never && matches!(resolved, Ty::Infer(_)) {
            // `let y = return; -y`: rustc needs `y`'s type here to pick the operator's
            // impl, even if a later use would decide it (E0282).
            self.error(
                Diagnostic::error(
                    ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                    "type annotations needed",
                )
                .with_label(operand.span, "cannot infer the type of this operand"),
            );
            return Ty::Error;
        }
        let ok = match (&resolved, op) {
            // `!(return)` is `!: Not`; there is no `!: Neg` (#87).
            (Ty::Error, _) | (Ty::Never, UnOp::Not) => true,
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
                let (l, l_never) = self.check_operand(lhs, None);
                if l_never {
                    // Only `!: PartialEq<!>` and `!: PartialOrd<!>` exist.
                    let (r, _) = self.check_operand(rhs, None);
                    self.never_operand(op, l, r, span);
                    return Ty::BOOL;
                }
                if let Ty::Ref(..) = self.shallow(&l) {
                    self.check_ref_comparison(op, &l, rhs, span);
                    return Ty::BOOL;
                }
                let errors_before = self.sink.error_count();
                let (r, r_never) = self.check_operand(rhs, Some(&l));
                if r_never {
                    if self.comparison_decided(op, &l) != Some(true) {
                        self.never_operand(op, l, r, span);
                        return Ty::BOOL;
                    }
                    self.coerce(rhs.span, &r, &l);
                }
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
                let (l, l_never) = self.check_operand(lhs, expected);
                let (r, r_never) = self.check_operand(rhs, None);
                if l_never || r_never {
                    return self.never_operand(op, l, r, span);
                }
                if self.check_shift_operands(op, &l, &r, span) {
                    l
                } else {
                    Ty::Error
                }
            }
            _ => {
                let (l, l_never) = self.check_operand(lhs, expected);
                let (r, r_never) = self.check_operand(rhs, (!l_never).then_some(&l));
                if l_never || r_never {
                    return self.never_operand(op, l, r, span);
                }
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
        let (rhs_ty, rhs_never) = self.check_operand(rhs, None);
        if rhs_never {
            if self.comparison_decided(op, lhs_ty) == Some(true) {
                self.coerce(rhs.span, &rhs_ty, lhs_ty);
            } else {
                self.never_operand(op, lhs_ty.clone(), rhs_ty, span);
            }
            return;
        }
        let r_res = self.shallow(&rhs_ty);
        match r_res {
            Ty::Error => return,
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

    /// Records an operation with an operand of type `!` (see [`NeverOperand`]) and
    /// returns its type for now.
    fn never_operand(&mut self, op: BinOp, lhs: Ty, rhs: Ty, span: Span) -> Ty {
        let out = (!op.is_comparison()).then(|| self.infer.new_var(VarKind::General));
        let ty = out.clone().unwrap_or(Ty::BOOL);
        self.never_operands.push(NeverOperand {
            op,
            compound: false,
            lhs,
            rhs,
            span,
            out,
        });
        ty
    }

    /// Whether a comparison with a `!` right operand and left operand type `lhs` has an
    /// impl to take: a scalar has one (`i32: PartialEq<i32>`), `&A` has one `PartialOrd`
    /// impl (`PartialOrd<&B>`) but two `PartialEq` ones (`PartialEq<&B>` and
    /// `PartialEq<&mut B>`). `None` while `lhs` is still a variable (`{integer}` has an
    /// impl per integer type).
    fn comparison_decided(&mut self, op: BinOp, lhs: &Ty) -> Option<bool> {
        match self.shallow(lhs) {
            Ty::Infer(_) => None,
            Ty::Ref(..) => Some(!matches!(op, BinOp::Eq | BinOp::Ne)),
            // Other types are judged by the comparison itself.
            _ => Some(true),
        }
    }

    /// Judges the operations [`Self::never_operand`] recorded, before the never-type
    /// fallback: an operand still `!` or still an open diverging variable is `!`.
    fn check_never_operands(&mut self) {
        for n in std::mem::take(&mut self.never_operands) {
            let l_never = self.is_never_like(&n.lhs);
            let r_never = self.is_never_like(&n.rhs);
            let ok = if !n.op.is_comparison() || n.compound {
                !l_never && !r_never
            } else if l_never {
                r_never
            } else if r_never {
                match self.comparison_decided(n.op, &n.lhs) {
                    Some(decided) => decided,
                    // An unknown type is reported with its local.
                    None => {
                        matches!(self.shallow(&n.lhs), Ty::Infer(v) if self.infer.kind(v) == VarKind::General)
                    }
                }
            } else {
                true
            };
            if ok {
                self.check_decided_operation(&n, r_never);
                continue;
            }
            if let Some(out) = &n.out {
                self.infer.bind_error(out);
            }
            let mut side = |never: bool, ty: &Ty| {
                if never {
                    return Some("!".to_owned());
                }
                let ty = self.infer.resolve_with_defaults(ty);
                // Reported already, or with its local.
                (ty != Ty::Error && !ty.has_infer()).then(|| self.ty_str(&ty))
            };
            let (Some(l), Some(r)) = (side(l_never, &n.lhs), side(r_never, &n.rhs)) else {
                continue;
            };
            let sym = format!("{}{}", n.op.as_str(), if n.compound { "=" } else { "" });
            let assign = if n.compound { "-assign" } else { "" };
            let message = match n.op {
                _ if n.op.is_comparison() => format!("can't compare `{l}` with `{r}`"),
                BinOp::Add => format!("cannot add{assign} `{r}` to `{l}`"),
                BinOp::Sub => format!("cannot subtract{assign} `{r}` from `{l}`"),
                BinOp::Mul => format!("cannot multiply{assign} `{l}` by `{r}`"),
                BinOp::Div => format!("cannot divide{assign} `{l}` by `{r}`"),
                BinOp::Rem if n.compound => {
                    format!("cannot calculate and assign the remainder of `{l}` divided by `{r}`")
                }
                BinOp::Rem => format!("cannot calculate the remainder of `{l}` divided by `{r}`"),
                _ => format!("no implementation for `{l} {sym} {r}`"),
            };
            self.error(
                Diagnostic::error(ErrorCode::INVALID_BINARY_OP, message)
                    .with_label(n.span, format!("no implementation for `{l} {sym} {r}`"))
                    .with_note(
                        "`!` does not coerce to the other operand's type here: the operator needs an implementation for `!` itself",
                    ),
            );
        }
    }

    /// A recorded operation whose operands were decided after all (`let y = return; let
    /// r = y + 1; let _: i32 = y;`): checked as an ordinary one now.
    fn check_decided_operation(&mut self, n: &NeverOperand, r_never: bool) {
        let shift = matches!(n.op, BinOp::Shl | BinOp::Shr);
        if n.op.is_comparison() && !n.compound {
            if !r_never {
                self.coerce(n.span, &n.rhs, &n.lhs);
            }
            return;
        }
        let ok = if shift {
            self.check_shift_operands(n.op, &n.lhs, &n.rhs, n.span)
        } else {
            self.coerce(n.span, &n.rhs, &n.lhs);
            self.check_arith_operands(n.op, &n.lhs, &n.rhs, n.span)
        };
        if let Some(out) = &n.out {
            let result = if ok { n.lhs.clone() } else { Ty::Error };
            if self.infer.unify(out, &result).is_err() {
                // The result was used as another type before the operands were decided
                // (`let r = y + 1; let _: i64 = y;` with `r` returned as an `i32`): rustc
                // reports it where the result is used, Sabiko at the operation (fuzz,
                // nightly run 23).
                let (out, result) = (self.infer.resolve(out), self.infer.resolve(&result));
                self.mismatch(n.span, &out, &result);
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
        if op.is_comparison() && matches!(referent, Ty::Array(..) | Ty::Slice(_)) {
            self.error(
                Diagnostic::error(
                    ErrorCode::UNSUPPORTED_SYNTAX,
                    "not yet supported: comparing arrays and slices",
                )
                .with_label(
                    span,
                    format!("`{}` on arrays and slices is not yet supported", op.as_str()),
                )
                .with_note(
                    "array comparison arrives with the comparison traits in Phase 6; see docs/PLAN.md. Compare the elements one by one instead",
                ),
            );
            return;
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

    /// `(a, b) = rhs`, checked as what rustc desugars it to, `{ let (x, y) = rhs; a = x;
    /// b = y; }`: the right side, then the pattern against its type (without default
    /// binding modes), then each assignment of a fresh local to its place, in order. A
    /// pointer coercion of such an assignment is recorded under the assignee's pattern.
    fn check_destructuring_assign(&mut self, pat: &Pat, rhs: &Expr) {
        let mut ty = self.check_expr(rhs, None);
        if self.shallow(&ty).is_never() {
            // `(a, b) = return;`: as for `let (x, y) = return;`, the pattern gets a fresh
            // variable, not `!`.
            let v = self.infer.new_var(VarKind::General);
            self.diverging_vars.push(v.clone());
            ty = v;
        }
        let saved = std::mem::replace(&mut self.in_assignee, true);
        self.check_pat(pat, &ty, Some(rhs.span));
        self.in_assignee = saved;
        let mut assignees = Vec::new();
        pat.walk_assignees(&mut |p, place| assignees.push((p, place)));
        for (p, place) in assignees {
            // The resolver gives every assignee a local: one naming a unit struct or a
            // variant compares, and is a path pattern by now.
            let local = self
                .resolve
                .local_of_pat(p.id)
                .expect("an assignee pattern has a local");
            let value_ty = self.local_types[&local].clone();
            let place_ty = self.check_place(place);
            self.coerce(place.span, &value_ty, &place_ty);
            self.note_pointer_coercion(p.id, &value_ty, &place_ty);
        }
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

    /// Records the generic arguments of a use of the function `def` at node `id`: the
    /// turbofish's (`_` a fresh variable), or a fresh inference variable per type parameter.
    /// Nothing for a function that is not generic.
    /// Records the generic arguments of a use of the function `def` at the path `path`
    /// (node `id`), and the bounds they must meet. A trait's member takes `self_ty` as its
    /// `Self` (decision 84).
    fn instantiate(
        &mut self,
        id: NodeId,
        def: DefId,
        path: &sabiko_ast::Path,
        span: Span,
        self_ty: Option<Ty>,
    ) {
        if self.items.builtin(def).is_some() {
            return;
        }
        let count = self.items.fn_sig(def).generics.len();
        if count == 0 {
            return;
        }
        // An associated function of a generic type takes the type's arguments first
        // (decision 83): `Self`'s own parameters, those after the type's segment
        // (`Pair::<i32, bool>::New`), or inference variables.
        let parent = self.resolve.parent_generic_count(def);
        let mut args: Vec<Ty> = Vec::new();
        if let Some(self_ty) = self_ty {
            args.push(self_ty);
        } else if parent > 0 {
            let adt = self
                .resolve
                .def(def)
                .parent
                .expect("an associated function");
            let mut type_path = path.clone();
            type_path.segments.truncate(1);
            type_path.generic_args.retain(|a| a.segment == 0);
            args = self.adt_args(adt, &type_path);
            if args.len() != parent {
                args = vec![Ty::Error; parent];
            }
        }
        // The resolver reported a turbofish of the wrong length; inference fills in.
        let own = count - parent;
        match path.args_of(path.segments.len() - 1) {
            Some(written) if written.args.len() == own => {
                args.extend(written.args.iter().map(|t| self.lower_type(t)));
            }
            _ => args.extend((0..own).map(|_| self.infer.new_var(VarKind::General))),
        }
        self.require_bounds(def, &args, span);
        self.generic_args.insert(id, (args, def, span));
    }

    /// The bounds of the function `def` at the generic arguments `args` become
    /// obligations of the body, judged when it is done.
    fn require_bounds(&mut self, def: DefId, args: &[Ty], span: Span) {
        let generics = &self.items.fn_sig(def).generics;
        for pred in self.items.param_env(def) {
            self.obligations.push(Obligation {
                ty: Ty::Param(pred.param).subst(generics, args),
                trait_: pred.trait_,
                span,
            });
        }
    }

    /// What `Self` is for the path expression `expr` naming a trait's member: the type
    /// its first segment names, or an inference variable for `Trait::Name`.
    fn assoc_self_ty(&mut self, node: NodeId, path: &sabiko_ast::Path) -> Ty {
        match self.resolve.assoc_self(node) {
            Some(Res::Def(d)) => match self.resolve.def(d).kind {
                DefKind::Trait => self.infer.new_var(VarKind::General),
                DefKind::TyParam => Ty::Param(d),
                DefKind::Impl => self.items.impl_self_ty(d),
                DefKind::Struct | DefKind::Enum => {
                    let mut type_path = path.clone();
                    type_path.segments.truncate(1);
                    type_path.generic_args.retain(|a| a.segment == 0);
                    let args = self.adt_args(d, &type_path);
                    Ty::Adt(d, args.into())
                }
                _ => Ty::Error,
            },
            Some(Res::Prim(p)) => Ty::Prim(p),
            _ => Ty::Error,
        }
    }

    /// The trait method a path `A::Name` calls, and its `Self`: for `Trait::Name` the
    /// resolver's, at a `Self` inference finds; for a type `A`, the one among the traits
    /// with a member `Name` that `A` implements (E0319 when none does, E0342, rustc's
    /// E0034, when several do).
    fn select_trait_item(
        &mut self,
        node: NodeId,
        path: &sabiko_ast::Path,
        def: DefId,
    ) -> Option<(DefId, Ty)> {
        let self_ty = self.assoc_self_ty(node, path);
        let by_trait = matches!(
            self.resolve.assoc_self(node),
            Some(Res::Def(d)) if self.resolve.def(d).kind == DefKind::Trait
        );
        if by_trait || self_ty.is_error() {
            return Some((def, self_ty));
        }
        let name = self.resolve.def(def).name.clone();
        let found: Vec<DefId> = self
            .resolve
            .trait_items_named(&name)
            .into_iter()
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|&m| {
                let tr = self.resolve.def(m).parent.expect("a trait member");
                self.may_implement(&self_ty, tr)
            })
            .collect();
        let shown = self.ty_str(&self_ty);
        match found.as_slice() {
            [one] => Some((*one, self_ty)),
            [] => {
                let tr = self.resolve.def(def).parent.expect("a trait member");
                let trait_name = self.resolve.def(tr).name.clone();
                self.error(
                    Diagnostic::error(
                        ErrorCode::UNKNOWN_ASSOC_ITEM,
                        format!("no function or associated item named `{name}` found for type `{shown}`"),
                    )
                    .with_label(path.last().span, format!("function or associated item not found in `{shown}`"))
                    .with_note(format!("the trait `{trait_name}` defines an item `{name}`, but `{shown}` does not implement it")),
                );
                None
            }
            many => {
                self.ambiguous(path.last().span, &name, many, &shown);
                None
            }
        }
    }

    /// Whether `ty` implements `tr` or may once inference knows more: an integer variable
    /// may be any integer type an `impl` of `tr` is for.
    fn may_implement(&mut self, ty: &Ty, tr: DefId) -> bool {
        let ty = self.infer.resolve(ty);
        match self.items.implements(&ty, tr, &self.env) {
            Solution::Yes => true,
            Solution::No => false,
            Solution::Ambiguous => {
                // The built-in `Copy` and `Clone` hold of every integer and float type,
                // and of tuples and arrays of them, never of `&mut T` (decision 86).
                let builtin = matches!(
                    self.items.lang_of(tr),
                    Some(LangItem::Copy | LangItem::Clone)
                );
                (builtin && self.may_be_copy_like(&ty, tr))
                    || self.impl_candidates(&ty, tr) > 0
                    || matches!(ty, Ty::Infer(v) if self.infer.kind(v) == VarKind::General)
            }
        }
    }

    /// Whether `ty`, with inference variables, may implement the built-in `Copy` or
    /// `Clone` `tr`: a variable may be any type, `&mut T`, `str` and slices never do, and
    /// a tuple or array does when its elements may.
    fn may_be_copy_like(&mut self, ty: &Ty, tr: DefId) -> bool {
        match ty {
            Ty::Infer(_) => true,
            Ty::Ref(Mutability::Mut, _) | Ty::Slice(_) | Ty::Prim(PrimTy::Str) => false,
            Ty::Tuple(elems) => elems.iter().all(|e| self.may_be_copy_like(e, tr)),
            Ty::Array(elem, _) => self.may_be_copy_like(elem, tr),
            _ => self.items.implements(ty, tr, &self.env) != Solution::No,
        }
    }

    /// How many `impl` blocks of `tr` have a type that `ty`, with inference variables,
    /// could be: an integer (float) variable can only be an integer (float) type, a
    /// general one anything.
    fn impl_candidates(&mut self, ty: &Ty, tr: DefId) -> usize {
        let impls: Vec<(Ty, Vec<DefId>)> = self
            .items
            .impls_of(tr)
            .map(|i| (i.self_ty.clone(), i.generics.clone()))
            .collect();
        impls
            .iter()
            .filter(|(pat, params)| self.could_be(pat, params, ty))
            .count()
    }

    fn could_be(&mut self, pat: &Ty, params: &[DefId], ty: &Ty) -> bool {
        if let Ty::Param(p) = pat {
            if params.contains(p) {
                return true;
            }
        }
        match (pat, ty) {
            (_, Ty::Infer(v)) => match self.infer.kind(*v) {
                VarKind::General => true,
                VarKind::Int => matches!(pat, Ty::Prim(p) if p.is_integer()),
                VarKind::Float => matches!(pat, Ty::Prim(p) if p.is_float()),
            },
            (_, Ty::Error) => true,
            // No block's type holds a slice (`check_impl_type`, E0107); a reference comes
            // from the sources of `?`'s conversions (`&Option<T>` for `Option<&T>`).
            (Ty::Ref(m1, a), Ty::Ref(m2, b)) => m1 == m2 && self.could_be(a, params, b),
            (Ty::Array(a, n), Ty::Array(b, m)) => n == m && self.could_be(a, params, b),
            (Ty::Tuple(a), Ty::Tuple(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| self.could_be(x, params, y))
            }
            (Ty::Adt(d1, a), Ty::Adt(d2, b)) => {
                d1 == d2
                    && a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| self.could_be(x, params, y))
            }
            (a, b) => a == b,
        }
    }

    /// Lets the pending bounds guide inference, as rustc's selection does: a bound on a
    /// type with inference variables that only one `impl` block's type could match makes
    /// it that type (`5.Show()` with only `impl Show for i64` is an `i64`). A bare
    /// general variable decides nothing. Repeats until nothing changes.
    fn select_obligations(&mut self) {
        loop {
            let mut progress = false;
            let pending: Vec<(Ty, DefId)> = self
                .obligations
                .iter()
                .map(|o| (o.ty.clone(), o.trait_))
                .collect();
            for (ty, tr) in pending {
                let ty = self.infer.resolve(&ty);
                if !ty.has_infer()
                    || matches!(ty, Ty::Infer(v) if self.infer.kind(v) == VarKind::General)
                {
                    continue;
                }
                if self.impl_candidates(&ty, tr) != 1 {
                    continue;
                }
                let all: Vec<(Ty, Vec<DefId>)> = self
                    .items
                    .impls_of(tr)
                    .map(|i| (i.self_ty.clone(), i.generics.clone()))
                    .collect();
                let Some((pat, params)) = all.into_iter().find(|(p, g)| self.could_be(p, g, &ty))
                else {
                    continue;
                };
                let fresh: Vec<Ty> = params
                    .iter()
                    .map(|_| self.infer.new_var(VarKind::General))
                    .collect();
                let pat = pat.subst(&params, &fresh);
                // Progress is a variable of `ty` learning something, not a fresh one
                // (a blanket block's `T`) being bound to it.
                if self.infer.unify(&pat, &ty).is_ok()
                    && known_nodes(&self.infer.resolve(&ty)) > known_nodes(&ty)
                {
                    progress = true;
                }
            }
            if !progress {
                return;
            }
        }
    }

    /// E0342 (rustc's E0034): several traits' methods named `name` apply to `shown`.
    fn ambiguous(&mut self, span: Span, name: &str, candidates: &[DefId], shown: &str) {
        let mut d = Diagnostic::error(
            ErrorCode::AMBIGUOUS_METHOD,
            "multiple applicable items in scope",
        )
        .with_label(span, format!("multiple `{name}` found"));
        for (i, &m) in candidates.iter().enumerate() {
            let tr = self.resolve.def(m).parent.expect("a trait member");
            let trait_name = &self.resolve.def(tr).name;
            d = d.with_label(
                self.resolve.def(m).span,
                format!(
                    "candidate #{} is defined in the trait `{trait_name}`",
                    i + 1
                ),
            );
            d = d.with_note(format!(
                "disambiguate it with the trait's path: `{trait_name}::{name}(..)` for `{shown}`"
            ));
        }
        self.error(d);
    }

    /// The generic arguments recorded for the use at node `id` (empty when the function is
    /// not generic).
    fn args_of_use(&self, id: NodeId) -> Vec<Ty> {
        self.generic_args
            .get(&id)
            .map(|(args, _, _)| args.clone())
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_lines)]
    fn check_call(
        &mut self,
        id: NodeId,
        callee: &Expr,
        args: &[Expr],
        expected: Option<&Ty>,
        span: Span,
    ) -> Ty {
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
            Ty::Adt(..)
                if strip_parens(callee).id == target.id
                    && self.variant_exprs.contains_key(&target.id) =>
            {
                // `E::Unit()`: a unit variant is a value, not a constructor.
                let ExprKind::Path(path) = &target.kind else {
                    unreachable!("recorded for a path")
                };
                let name = path
                    .segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                self.error(
                    Diagnostic::error(
                        ErrorCode::NOT_CALLABLE,
                        format!("expected function, found enum variant `{name}`"),
                    )
                    .with_label(callee.span, "a unit variant is a value: remove the `()`"),
                );
                for a in args {
                    self.check_expr(a, None);
                }
                return Ty::Error;
            }
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
        if self.resolve.def(def).kind == DefKind::Variant {
            // `E::V(a, b)`: a tuple variant's constructor.
            let (adt, index) = self.variant_of(def);
            let generic = match &target.kind {
                ExprKind::Path(path) => self.adt_args(adt, path),
                _ => self.fresh_adt_args(adt),
            };
            let mut variant = self.items.adt(adt).variant(index).clone();
            variant.fields = self.instance_fields(adt, &variant.fields, &generic);
            if variant.fields.len() != args.len() {
                let d =
                    Self::wrong_arg_count("enum variant", variant.fields.len(), args.len(), span)
                        .with_label(variant.span, "variant defined here");
                self.error(d);
            }
            let output = Ty::Adt(adt, generic.into());
            let formals: Vec<Ty> = variant.fields.iter().map(|f| f.ty.clone()).collect();
            let hints = self.expected_inputs(expected, &output, &formals);
            self.check_args(args, &formals, hints.as_deref(), callee.span);
            self.variant_exprs.insert(id, (adt, index));
            return output;
        }
        if self.is_destructor(def) {
            // `D::drop(&mut d)`, `Drop::drop(&mut d)`: nothing infers `Self` then.
            for arg in self.args_of_use(target.id) {
                self.infer.bind_error(&arg);
            }
            self.explicit_drop_call(callee.span);
            for a in args {
                self.check_expr(a, None);
            }
            return Ty::Error;
        }
        let mut sig = self.items.fn_sig(def).clone();
        let generic_args = self.args_of_use(target.id);
        sig.inputs = sig.inputs_with(&generic_args);
        sig.output = sig.output_with(&generic_args);
        if sig.inputs.len() != args.len() {
            let d = Self::wrong_arg_count("function", sig.inputs.len(), args.len(), span);
            let name = self.resolve.def(def).name.clone();
            // A builtin has no source to point at.
            let d = if sig.name_span.is_dummy() {
                d
            } else {
                d.with_label(sig.name_span, format!("function `{name}` defined here"))
            };
            self.error(d);
        }
        let hints = self.expected_inputs(expected, &sig.output, &sig.inputs);
        self.check_args(args, &sig.inputs, hints.as_deref(), callee.span);
        if sig.output.is_never() {
            self.diverges = true;
        }
        sig.output
    }

    /// `e.Name(args)`. The receiver is checked first, then the method is found by
    /// [`Self::probe_method`], then the arguments are checked against the parameters
    /// after the receiver, as in a call.
    fn check_method_call(
        &mut self,
        expr: &Expr,
        receiver: &Expr,
        method: &sabiko_ast::Ident,
        turbofish: Option<&sabiko_ast::GenericArgs>,
        args: &[Expr],
        expected: Option<&Ty>,
    ) -> Ty {
        // `(*s).len()` measures a slice place; any other method of a slice is reported by
        // `check_len_call`.
        self.unsized_place = true;
        let recv_ty = self.check_expr(receiver, None);
        if let Some(ty) = self.check_len_call(expr, &recv_ty, method, args) {
            return ty;
        }
        let Some((callee, parent_args)) = self.probe_method(&recv_ty, method) else {
            for a in args {
                self.check_expr(a, None);
            }
            return Ty::Error;
        };
        if self.resolve.intrinsic(callee.def) == Some(Intrinsic::SliceLen) {
            // `<[T]>::len`, found after the methods a user trait gives an array: a read of
            // the length, through the references the probe looked through.
            for a in args {
                self.check_expr(a, None);
            }
            if !args.is_empty() {
                let d = Self::wrong_arg_count("method", 0, args.len(), expr.span);
                self.error(d);
            }
            self.len_calls.insert(expr.id, callee.autoderefs);
            return Ty::Prim(PrimTy::Usize);
        }
        if self.is_destructor(callee.def) {
            // `d.drop()`.
            self.explicit_drop_call(method.span);
            for a in args {
                self.check_expr(a, None);
            }
            return Ty::Error;
        }
        let mut sig = self.items.fn_sig(callee.def).clone();
        // The arguments of the method's type (decision 83) are the receiver's own, a trait
        // method's is its `Self` (decision 84); a turbofish gives the method's.
        let parent = self.resolve.parent_generic_count(callee.def);
        let parent_args = if parent_args.len() == parent {
            parent_args
        } else {
            vec![Ty::Error; parent]
        };
        let count = sig.generics.len() - parent;
        let own_args: Vec<Ty> = match turbofish {
            Some(written) if written.args.len() == count => {
                written.args.iter().map(|t| self.lower_type(t)).collect()
            }
            written => {
                if let Some(written) = written {
                    let plural = |n: usize| if n == 1 { "argument" } else { "arguments" };
                    let got = written.args.len();
                    self.error(
                        Diagnostic::error(
                            ErrorCode::WRONG_GENERIC_ARG_COUNT,
                            format!(
                                "method takes {count} generic {} but {got} generic {} {} supplied",
                                plural(count),
                                plural(got),
                                if got == 1 { "was" } else { "were" }
                            ),
                        )
                        .with_label(
                            written.span,
                            format!("expected {count} generic {}", plural(count)),
                        )
                        .with_label(
                            sig.name_span,
                            format!("method `{}` defined here", method.name),
                        ),
                    );
                }
                (0..count)
                    .map(|_| self.infer.new_var(VarKind::General))
                    .collect()
            }
        };
        let mut generic_args = parent_args;
        generic_args.extend(own_args);
        if !generic_args.is_empty() {
            self.require_bounds(callee.def, &generic_args, method.span);
            self.generic_args
                .insert(expr.id, (generic_args.clone(), callee.def, method.span));
            sig.inputs = sig.inputs_with(&generic_args);
            sig.output = sig.output_with(&generic_args);
        }
        let params = &sig.inputs[1..];
        if params.len() != args.len() {
            let d = Self::wrong_arg_count("method", params.len(), args.len(), expr.span)
                .with_label(
                    sig.name_span,
                    format!("method `{}` defined here", method.name),
                );
            self.error(d);
        }
        let hints = self.expected_inputs(expected, &sig.output, params);
        self.check_args(args, params, hints.as_deref(), method.span);
        // No method returns `!`: the never type cannot be written in a signature.
        self.method_calls.insert(expr.id, callee);
        sig.output
    }

    /// Finds the method `name` for a receiver of type `recv_ty`, as rustc's probe does:
    /// the receiver is dereferenced step by step (`&&P`, `&P`, `P`), and at each step the
    /// step's type is tried by value, then as `&step`, then as `&mut step`, against the
    /// receiver type of each candidate: the inherent method of the innermost struct or
    /// enum first, then the methods named `name` of every trait the candidate's `Self`
    /// implements (a bound in scope for a type parameter), decision 84. Several traits at
    /// one step are E0342 (rustc's E0034). Returns the callee and the generic arguments
    /// of its block or trait: the receiver type's for an inherent method, `Self` for a
    /// trait's. Reports and returns `None` when nothing matches.
    #[allow(clippy::too_many_lines)]
    fn probe_method(
        &mut self,
        recv_ty: &Ty,
        name: &sabiko_ast::Ident,
    ) -> Option<(MethodCallee, Vec<Ty>)> {
        let mut steps = vec![self.infer.resolve(recv_ty)];
        while let Ty::Ref(_, inner) = steps.last().expect("one step").clone() {
            steps.push(*inner);
        }
        let base = steps.last().expect("one step").clone();
        let trait_methods: Vec<DefId> = self
            .resolve
            .trait_items_named(&name.name)
            .into_iter()
            .filter(|&m| self.items.fn_sig(m).self_kind.is_some())
            .collect();
        match base {
            Ty::Error => return None,
            Ty::Infer(v) if self.infer.kind(v) == VarKind::General => {
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(
                        name.span,
                        "the receiver's type must be known to call a method",
                    ),
                );
                // The local behind it is the same problem, not a new one.
                self.infer.bind_error(&Ty::Infer(v));
                return None;
            }
            _ => {}
        }
        let inherent = self.inherent_method(&base, &name.name);
        // An array's last step is its slice, reached by unsizing (rustc's probe does the
        // same), where the slice methods of the prelude's `impl<T> [T]` are inherent.
        let mut steps: Vec<(Ty, usize, bool)> = steps
            .into_iter()
            .enumerate()
            .map(|(autoderefs, step)| (step, autoderefs, false))
            .collect();
        if let Ty::Array(elem, _) = &base {
            steps.push((Ty::Slice(elem.clone()), steps.len() - 1, true));
        }
        for (step, autoderefs, unsize) in &steps {
            let (autoderefs, unsize) = (*autoderefs, *unsize);
            let inherent_here = if unsize {
                self.inherent_method(step, &name.name)
            } else {
                inherent.clone()
            };
            for autoref in [None, Some(Mutability::Not), Some(Mutability::Mut)] {
                let adjusted = match autoref {
                    None => step.clone(),
                    Some(m) => Ty::Ref(m, Box::new(step.clone())),
                };
                // A reference receiver taken by value is reborrowed, not copied or moved:
                // `&*r`, `&mut *m`.
                let callee = |def: DefId| match (autoref, step) {
                    (None, Ty::Ref(m, _)) => MethodCallee {
                        def,
                        autoderefs: autoderefs + 1,
                        autoref: Some(*m),
                        unsize,
                    },
                    _ => MethodCallee {
                        def,
                        autoderefs,
                        autoref,
                        unsize,
                    },
                };
                if let Some((def, self_ty, args)) = &inherent_here {
                    if *self_ty == adjusted {
                        return Some((callee(*def), args.clone()));
                    }
                }
                if unsize {
                    // Only the slice's inherent methods: a trait implemented for slices
                    // needs a block for an unsized type, which is not supported yet.
                    continue;
                }
                let mut found: Vec<(DefId, Ty)> = Vec::new();
                for &m in &trait_methods {
                    let tr = self.resolve.def(m).parent.expect("a trait member");
                    let self_ty = match self.items.fn_sig(m).self_kind {
                        Some(sabiko_ast::SelfKind::Ref { mutable }) => match &adjusted {
                            Ty::Ref(mm, inner) if *mm == Mutability::from_bool(mutable) => {
                                (**inner).clone()
                            }
                            _ => continue,
                        },
                        _ => adjusted.clone(),
                    };
                    if self.may_implement(&self_ty, tr) {
                        found.push((m, self_ty));
                    }
                }
                match found.as_slice() {
                    [] => {}
                    [(def, self_ty)] => return Some((callee(*def), vec![self_ty.clone()])),
                    many => {
                        let defs: Vec<DefId> = many.iter().map(|(d, _)| *d).collect();
                        let shown = self.ty_str(&many[0].1);
                        self.ambiguous(name.span, &name.name, &defs, &shown);
                        return None;
                    }
                }
            }
        }
        self.method_not_found(recv_ty, &base, name, &trait_methods);
        None
    }

    /// The inherent method `name` of `base` taking a receiver (`self`, `&self`, ...), with
    /// its receiver type and the generic arguments of its block: a struct's or enum's at
    /// the type's arguments, or for a slice the prelude's `impl<T> [T]` at its element
    /// type (decision 93).
    fn inherent_method(&self, base: &Ty, name: &str) -> Option<(DefId, Ty, Vec<Ty>)> {
        let (def, args) = match base {
            Ty::Adt(adt, base_args) => (self.resolve.assoc_fn(*adt, name)?, base_args.to_vec()),
            Ty::Slice(elem) => (self.resolve.slice_method(name)?, vec![(**elem).clone()]),
            _ => return None,
        };
        let sig = self.items.fn_sig(def);
        sig.self_kind?;
        let parent = &sig.generics[..self.resolve.parent_generic_count(def)];
        Some((def, sig.inputs[0].subst(parent, &args), args))
    }

    /// Reports a method call nothing matched (E0319, rustc's E0599), with what may explain
    /// it: a field of that name, an associated function, a destructor, a trait that has
    /// the method but is not implemented or not a bound.
    fn method_not_found(
        &mut self,
        recv_ty: &Ty,
        base: &Ty,
        name: &sabiko_ast::Ident,
        trait_methods: &[DefId],
    ) {
        if let Ty::Adt(adt, _) = base {
            let adt = *adt;
            if name.name == "drop"
                && self.items.drop_impl(adt).is_some()
                && self.resolve.assoc_fn(adt, &name.name).is_none()
            {
                // `d.drop()`: the destructor of `impl Drop` is not callable.
                self.explicit_drop_call(name.span);
                return;
            }
            let label = if self.resolve.assoc_fn(adt, &name.name).is_some() {
                "this is an associated function, not a method"
            } else if self.items.adt(adt).field_index(&name.name).is_some() {
                "field, not a method"
            } else {
                "method not found"
            };
            self.unknown_method(recv_ty, name, Some(adt), label, trait_methods);
        } else {
            self.unknown_method(recv_ty, name, None, "method not found", trait_methods);
        }
    }

    /// Whether `def` is `Drop::drop` or an `impl Drop` block's `drop`, which no code may
    /// call (E0323).
    fn is_destructor(&self, def: DefId) -> bool {
        self.resolve.is_drop_fn(def) || self.resolve.is_lang_method(def, LangItem::Drop)
    }

    /// E0323: `d.drop()` or `D::drop(&mut d)` (rustc's E0040). Sabiko has no
    /// `std::mem::drop`; a value is dropped early by passing it to a by-value parameter.
    fn explicit_drop_call(&mut self, span: Span) {
        self.error(
            Diagnostic::error(
                ErrorCode::EXPLICIT_DROP_CALL,
                "explicit use of destructor method",
            )
            .with_label(span, "explicit destructor calls not allowed")
            .with_note("a value is dropped at the end of its scope; to drop it earlier, pass it by value to a function"),
        );
    }

    /// Whether `ty` is a type of the prelude the user's file has an `impl` block for that
    /// the orphan rule rejects.
    fn orphan_adt(&self, ty: &Ty) -> bool {
        matches!(ty, Ty::Adt(adt, _) if self.resolve.has_orphan_impl(*adt))
    }

    /// E0319: no method `name` for a receiver of type `recv_ty`.
    fn unknown_method(
        &mut self,
        recv_ty: &Ty,
        name: &sabiko_ast::Ident,
        adt: Option<DefId>,
        label: &str,
        trait_methods: &[DefId],
    ) {
        if adt.is_some_and(|a| self.resolve.has_orphan_impl(a)) {
            // The method may be one of the block the orphan rule rejected (E0211, E0214),
            // which was reported.
            return;
        }
        let shown = self.ty_str(recv_ty);
        let mut base = self.infer.resolve(recv_ty);
        while let Ty::Ref(_, inner) = base {
            base = *inner;
        }
        let what = match (self.shallow(recv_ty), adt, &base) {
            (Ty::Ref(..), _, _) => format!("reference `{shown}`"),
            (_, Some(adt), _) if self.items.adt(adt).is_enum() => format!("enum `{shown}`"),
            (_, Some(_), _) => format!("struct `{shown}`"),
            (_, None, Ty::Param(_)) => format!("type parameter `{shown}`"),
            _ => format!("type `{shown}`"),
        };
        let mut d = Diagnostic::error(
            ErrorCode::UNKNOWN_METHOD,
            format!("no method named `{}` found for {what}", name.name),
        )
        .with_label(name.span, label);
        if let Some(adt) = adt {
            let owner = self.items.adt(adt).name.clone();
            if self.resolve.assoc_fn(adt, &name.name).is_some() {
                d = d.with_note(format!(
                    "call it as an associated function: `{owner}::{}(..)`",
                    name.name
                ));
            }
        }
        if let Some(&m) = trait_methods.first() {
            let tr = self.resolve.def(m).parent.expect("a trait member");
            let trait_name = self.resolve.def(tr).name.clone();
            let base_shown = self.ty_str(&base);
            d = d.with_note(match base {
                Ty::Param(_) => format!(
                    "the trait `{trait_name}` defines an item `{}`: restrict type parameter `{base_shown}` with it, `{base_shown}: {trait_name}`",
                    name.name
                ),
                _ => format!(
                    "the trait `{trait_name}` defines an item `{}`, but `{base_shown}` does not implement it",
                    name.name
                ),
            });
        } else if adt.is_none() {
            d = d.with_note(
                "methods come from inherent `impl` blocks of structs and enums, and from traits",
            );
        }
        self.error(d);
    }

    fn wrong_arg_count(what: &str, expected: usize, found: usize, span: Span) -> Diagnostic {
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
                "this {what} takes {expected} {} but {found} {} supplied",
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

    /// `Print(x)` / `PrintLine(x)`: one argument of any printable type, or a reference to
    /// one (see [`collect::print_derefs`]); an unsuffixed literal takes its default later. Both
    /// return `()`.
    fn check_print_call(&mut self, builtin: Builtin, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            let d = Self::wrong_arg_count("function", 1, args.len(), span);
            self.error(d);
        }
        for arg in args {
            let ty = self.check_expr(arg, None);
            let ty = self.infer.resolve(&ty);
            if let Ty::Infer(v) = ty {
                if self.is_diverging_var(v) {
                    // Decided by a later use or by the never-type fallback: checked then.
                    self.pending_prints.push((builtin, arg.span, ty));
                    continue;
                }
            }
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
            self.check_printable(builtin, arg.span, &ty);
        }
        Ty::Unit
    }

    /// Whether the variable `v` is one of a diverging initialiser or element.
    fn is_diverging_var(&mut self, v: TyVid) -> bool {
        let vars = self.diverging_vars.clone();
        vars.iter().any(|d| self.infer.resolve(d) == Ty::Infer(v))
    }

    /// Reports a `Print`/`PrintLine` argument of known type `ty` that cannot be printed.
    fn check_printable(&mut self, builtin: Builtin, span: Span, ty: &Ty) {
        // A reference prints what it points to (`&5`, `&&s`; rustc's `Display` for `&T`).
        let mut base = ty;
        while let (false, Ty::Ref(_, inner)) = (collect::is_printable(base), base) {
            base = inner;
        }
        // `&loop {}` prints like `loop {}` (rustc: `!: Display`); neither runs. A literal
        // variable takes its default later.
        if matches!(base, Ty::Infer(_) | Ty::Error)
            || base.is_uninhabited()
            || collect::is_printable(base)
        {
            return;
        }
        let s = self.ty_str(ty);
        self.error(
            Diagnostic::error(
                ErrorCode::TYPE_MISMATCH,
                format!("`{}` cannot print a value of type `{s}`", builtin.name()),
            )
            .with_label(span, format!("expected a printable value, found `{s}`"))
            .with_note("printable types are integers, floats, `bool`, `char` and `&str`, and references to them"),
        );
    }

    /// Whether `ty` (resolved) holds no general inference variable: literal variables
    /// (`{integer}`, `{float}`) count as known.
    fn has_no_general_var(&mut self, ty: &Ty) -> bool {
        match ty {
            Ty::Infer(v) => self.infer.kind(*v) != VarKind::General,
            Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => {
                self.has_no_general_var(inner)
            }
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
            Ty::Adt(d, args) => {
                let adt = self.items.adt(d);
                if let Some(index) = adt.field_index(name) {
                    self.field_accesses
                        .insert(expr.id, FieldAccess { index, autoderefs });
                    return adt.field_ty(&adt.fields[index].ty, &args);
                }
                let available: Vec<String> =
                    adt.fields.iter().map(|f| format!("`{}`", f.name)).collect();
                // The type as written, references included (`&P`), as rustc reports it.
                let shown = self.ty_str(base_ty);
                let is_method = self
                    .resolve
                    .assoc_fn(d, name)
                    .is_some_and(|m| self.items.fn_sig(m).self_kind.is_some());
                if is_method {
                    self.error(
                        Diagnostic::error(
                            ErrorCode::METHOD_AS_FIELD,
                            format!("attempted to take value of method `{name}` on type `{shown}`"),
                        )
                        .with_label(name_span, "method, not a field")
                        .with_note(format!("use parentheses to call the method: `{name}(..)`")),
                    );
                    return Ty::Error;
                }
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
            Ty::Array(..) | Ty::Slice(_) if name == "len" => {
                let shown = self.ty_str(base_ty);
                self.error(
                    Diagnostic::error(
                        ErrorCode::METHOD_AS_FIELD,
                        format!("attempted to take value of method `len` on type `{shown}`"),
                    )
                    .with_label(name_span, "method, not a field")
                    .with_note("use parentheses to call the method: `len()`"),
                );
                Ty::Error
            }
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
        let adjusted = self.expectation_for_branches(expected);
        let ty = self.check_branches(then, els, adjusted.as_ref(), cond_diverges, span);
        if let (Some(var), None) = (expected, &adjusted) {
            // The expectation was a variable, which the branches were not checked against
            // (see `expectation_for_branches`): the `if` as a whole is coerced to it.
            self.coerce(span, &ty, var);
        }
        ty
    }

    /// The branches of `if cond { then } else { els }`, checked against `expected`.
    fn check_branches(
        &mut self,
        then: &Block,
        els: &Expr,
        expected: Option<&Ty>,
        cond_diverges: bool,
        span: Span,
    ) -> Ty {
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

    fn check_struct_literal(
        &mut self,
        expr: &Expr,
        fields: &[sabiko_ast::FieldInit],
        expected: Option<&Ty>,
    ) -> Ty {
        let ExprKind::Struct { path, .. } = &expr.kind else {
            unreachable!()
        };
        let Res::Def(def) = self.resolve.res(expr.id) else {
            for f in fields {
                self.check_expr(&f.value, None);
            }
            return Ty::Error;
        };
        let (def, adt) = if self.resolve.def(def).kind == DefKind::Variant {
            // `E::V { .. }`: the variant's fields; the value is an `E`.
            let (enum_def, index) = self.variant_of(def);
            self.variant_exprs.insert(expr.id, (enum_def, index));
            let e = self.items.adt(enum_def);
            let v = e.variant(index);
            let adt = collect::AdtDef {
                name: e.name.clone(),
                generics: e.generics.clone(),
                fields: v.fields.clone(),
                is_unit: false,
                derives_copy: false,
                derives_clone: false,
                span: v.span,
                variants: None,
            };
            (enum_def, adt)
        } else {
            (def, self.items.adt(def).clone())
        };
        let generic = self.adt_args(def, path);
        self.unify_with_expected_adt(&Ty::Adt(def, generic.clone().into()), expected);
        let mut adt = adt;
        adt.fields = self.instance_fields(def, &adt.fields, &generic);
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
        Ty::Adt(def, generic.into())
    }

    // ----- finalisation -----

    /// rustc's E0282 for a value no local holds (`let _ = [];`, `(_, _) = (x, [])`, #78):
    /// the innermost expression whose type is still unknown is reported, and every unknown
    /// variable becomes an error, as the expressions around it are the same problem. Only
    /// in a body without other errors, whose unknown types may be their consequence.
    fn report_unknown_exprs(&mut self) {
        if self.sink.error_count() > self.errors_before {
            return;
        }
        let mut unknown: Vec<(Span, Ty)> = Vec::new();
        for (id, ty) in &self.expr_types {
            if self.infer.resolve_with_defaults(ty).has_infer() {
                unknown.push((self.expr_spans[id], ty.clone()));
            }
        }
        // Innermost first: a sub-expression's span is no longer than its parent's.
        unknown.sort_by_key(|(span, _)| (span.end - span.start, span.start));
        // Like rustc, one report per body: the others are made errors silently.
        let mut reported = false;
        for (span, ty) in unknown {
            let t = self.infer.resolve_with_defaults(&ty);
            if t.has_infer() && !std::mem::replace(&mut reported, true) {
                self.error(
                    Diagnostic::error(
                        ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                        "type annotations needed",
                    )
                    .with_label(span, "cannot infer the type of this expression"),
                );
            }
            self.infer.bind_error(&t);
        }
    }

    /// The expressions' final types, after reporting those still unknown.
    fn resolve_expr_types(&mut self) -> HashMap<NodeId, Ty> {
        self.report_unknown_exprs();
        let mut expr_types = HashMap::new();
        for (id, ty) in std::mem::take(&mut self.expr_types) {
            let mut t = self.infer.resolve_with_defaults(&ty);
            // Anything still unknown was reported (with a local, or above).
            if t.has_infer() {
                t = Ty::Error;
            }
            expr_types.insert(id, t);
        }
        expr_types
    }

    /// The generic arguments of every use of a generic function, resolved; an argument
    /// inference left unknown is E0313 (rustc's E0282) at the use, and an error type.
    fn resolve_generic_args(&mut self) -> HashMap<NodeId, Vec<Ty>> {
        let mut uses: Vec<(NodeId, GenericUse)> =
            std::mem::take(&mut self.generic_args).into_iter().collect();
        uses.sort_by_key(|(_, (_, _, span))| (span.start, span.end));
        let mut out = HashMap::new();
        for (id, (args, def, span)) in uses {
            let mut resolved = Vec::new();
            for (i, arg) in args.iter().enumerate() {
                let mut t = self.infer.resolve_with_defaults(arg);
                if t.has_infer() {
                    let param = self.resolve.generics(def)[i];
                    let param_name = self.resolve.def(param).name.clone();
                    let fn_name = self.resolve.def(def).name.clone();
                    self.error(
                        Diagnostic::error(
                            ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                            "type annotations needed",
                        )
                        .with_label(
                            span,
                            format!("cannot infer type of the type parameter `{param_name}` declared on the function `{fn_name}`"),
                        )
                        .with_note(format!("specify it with a turbofish: `{fn_name}::<..>`")),
                    );
                    self.infer.bind_error(&t);
                    t = Ty::Error;
                }
                // Every type parameter but a trait's `Self` is `Sized` (rustc's E0277).
                let trait_self = i == 0 && self.resolve.parent_kind(def) == Some(DefKind::Trait);
                if crate::traits::is_unsized(&t) && !trait_self {
                    let param = self.resolve.generics(def)[i];
                    let param_name = self.resolve.def(param).name.clone();
                    let shown = self.ty_str(&t);
                    self.error(
                        Diagnostic::error(
                            ErrorCode::UNSIZED_TYPE,
                            format!("the size for values of type `{shown}` cannot be known at compilation time"),
                        )
                        .with_label(span, "doesn't have a size known at compile-time")
                        .with_note(format!("the type parameter `{param_name}` stands for sized types only; pass a reference instead")),
                    );
                    t = Ty::Error;
                }
                resolved.push(t);
            }
            out.insert(id, resolved);
        }
        out
    }

    /// Judges the bounds the body's uses require (E0341, rustc's E0277), once inference is
    /// done. A type inference left unknown is E0313 (rustc's E0283), unless an error about
    /// it was reported.
    fn check_obligations(&mut self) {
        let mut seen: HashSet<(Ty, DefId, Span)> = HashSet::new();
        for ob in std::mem::take(&mut self.obligations) {
            let ty = self.infer.resolve_with_defaults(&ob.ty);
            if !seen.insert((ty.clone(), ob.trait_, ob.span)) {
                continue;
            }
            let trait_name = self.resolve.def(ob.trait_).name.clone();
            match self.items.implements(&ty, ob.trait_, &self.env) {
                Solution::Yes => {}
                Solution::Ambiguous => {
                    let shown = self.ty_str(&ty);
                    self.error(
                        Diagnostic::error(
                            ErrorCode::TYPE_ANNOTATIONS_NEEDED,
                            "type annotations needed",
                        )
                        .with_label(ob.span, "cannot infer type")
                        .with_note(format!("cannot satisfy `{shown}: {trait_name}`")),
                    );
                    self.infer.bind_error(&ty);
                }
                Solution::No if self.orphan_adt(&ty) => {
                    // The bound may be the one of a block the orphan rule rejected
                    // (E0214), which was reported.
                }
                Solution::No => {
                    let shown = self.ty_str(&ty);
                    let mut d = Diagnostic::error(
                        ErrorCode::UNSATISFIED_BOUND,
                        format!("the trait bound `{shown}: {trait_name}` is not satisfied"),
                    )
                    .with_label(
                        ob.span,
                        format!("the trait `{trait_name}` is not implemented for `{shown}`"),
                    );
                    let prelude = self.resolve.is_prelude(ob.trait_);
                    if let (Ty::Param(_), false) = (&ty, prelude) {
                        d = d.with_note(format!(
                            "consider restricting type parameter `{shown}`: `{shown}: {trait_name}`"
                        ));
                    } else if let Ty::Param(_) = ty {
                        // The prelude's traits are its own (Rust's `std::fmt::Debug` is
                        // outside its prelude), and a path cannot name one yet.
                        d = d.with_note(format!(
                            "a bound cannot name `{trait_name}` yet: it is not in the prelude, and paths to other modules arrive with Phase 9"
                        ));
                    } else if prelude {
                        // rustc's standard library implements its traits for more types
                        // than Sabiko's `core` prelude does so far (decision 90).
                        d = d.with_note(format!(
                            "Sabiko's `core` library implements `{trait_name}` only for some types so far (see `library/core`)"
                        ));
                    }
                    self.error(d);
                }
            }
        }
    }

    /// In a trait's provided method `Self` may be unsized, as in rustc: a parameter, a
    /// return type or a local holding a `Self` by value is E0208 (rustc's E0277). rustc
    /// accepts them under `where Self: Sized`, which is not supported yet.
    fn check_self_sized(&mut self, def: DefId, f: &FnDecl) {
        let tr = self.resolve.def(def).parent.expect("a trait member");
        // `Clone` (and so `Copy`) is `Sized`, as in rustc: a subtrait's `Self` has a size.
        let sized = self
            .items
            .lang_item(LangItem::Clone)
            .is_some_and(|clone| self.items.trait_closure(tr).contains(&clone));
        if sized {
            return;
        }
        let self_param = self.resolve.trait_self(tr);
        let sig = self.items.fn_sig(def).clone();
        let mut spans: Vec<Span> = Vec::new();
        for (ty, span) in sig.inputs.iter().zip(&sig.param_spans) {
            if holds_by_value(ty, self_param) {
                spans.push(*span);
            }
        }
        if holds_by_value(&sig.output, self_param) {
            spans.push(f.ret.as_ref().map_or(sig.name_span, |t| t.span));
        }
        let mut locals: Vec<(LocalId, Ty)> = self
            .local_types
            .iter()
            .map(|(l, t)| (*l, t.clone()))
            .collect();
        locals.sort_by_key(|(l, _)| *l);
        for (local, ty) in locals {
            let info = self.resolve.local(local);
            if info.is_param {
                continue;
            }
            if holds_by_value(&self.infer.resolve(&ty), self_param) {
                spans.push(info.span);
            }
        }
        for span in spans {
            self.error(
                Diagnostic::error(
                    ErrorCode::UNSIZED_TYPE,
                    "the size for values of type `Self` cannot be known at compilation time",
                )
                .with_label(span, "doesn't have a size known at compile-time")
                .with_note("in a trait's provided method, `Self` is taken and returned behind a reference; `where Self: Sized` is not supported yet"),
            );
        }
    }

    /// `-e` where `e` was still `{integer}` and became unsigned (E0307).
    fn check_pending_negs(&mut self) {
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
    }

    fn finish(mut self) -> BodyTypes {
        self.select_obligations();
        self.select_try_conversions();
        self.check_never_operands();
        // Never-type fallback (edition 2024, #85): `let x = return;` leaves `x`
        // unconstrained; it becomes `!` (`()` in edition 2021).
        for v in std::mem::take(&mut self.diverging_vars) {
            self.infer.fall_back_to_never(&v);
        }
        for (builtin, span, ty) in std::mem::take(&mut self.pending_prints) {
            let ty = self.infer.resolve_with_defaults(&ty);
            self.check_printable(builtin, span, &ty);
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
        let generic_args = self.resolve_generic_args();
        self.check_obligations();
        self.check_pending_tries();
        let mut pat_types = HashMap::new();
        for (id, ty) in std::mem::take(&mut self.pat_types) {
            let mut t = self.infer.resolve_with_defaults(&ty);
            if t.has_infer() {
                t = Ty::Error;
            }
            pat_types.insert(id, t);
        }
        let expr_types = self.resolve_expr_types();
        self.check_pending_negs();

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

        self.check_pending_repeats();

        let has_errors = self.sink.error_count() > self.errors_before;
        BodyTypes {
            expr_types,
            local_types,
            field_accesses: self.field_accesses,
            pat_derefs: self.pat_derefs,
            ref_bindings: self.ref_bindings,
            reborrows: self.reborrows,
            method_calls: self.method_calls,
            generic_args,
            pat_types,
            pat_variants: self.pat_variants,
            variant_exprs: self.variant_exprs,
            pat_consts: HashMap::new(),
            index_derefs: self.index_derefs,
            len_calls: self.len_calls,
            coercions: Self::resolved_coercions(&mut self.infer, self.coercions),
            repeat_counts: self.repeat_counts,
            path_defs: self.path_defs,
            try_exprs: self.try_exprs,
            has_errors,
        }
    }

    /// The pointer coercions with their targets resolved: the target of `let s: &[_] = &a;`
    /// is only known once `_` is. Anything still unknown was reported with its local.
    fn resolved_coercions(
        infer: &mut InferCtxt,
        coercions: HashMap<NodeId, Ty>,
    ) -> HashMap<NodeId, Ty> {
        coercions
            .into_iter()
            .map(|(id, ty)| {
                let t = infer.resolve_with_defaults(&ty);
                (id, if t.has_infer() { Ty::Error } else { t })
            })
            .collect()
    }

    /// `[x; N]` with `N` other than 0 or 1 copies `x`: its type must be `Copy`, unless it
    /// names a `const` item (rustc's E0277, Sabiko's E0334).
    fn check_pending_repeats(&mut self) {
        for (span, ty) in std::mem::take(&mut self.pending_repeats) {
            let t = self.infer.resolve_with_defaults(&ty);
            if !t.has_infer() && !self.items.is_copy_in(&t, &self.env) {
                let s = self.ty_str(&t);
                self.error(
                    Diagnostic::error(
                        ErrorCode::REPEAT_NEEDS_COPY,
                        format!("the trait bound `{s}: Copy` is not satisfied"),
                    )
                    .with_label(span, format!("the trait `Copy` is not implemented for `{s}`"))
                    .with_note("the operand of a repeat expression `[x; N]` with `N` other than 0 or 1 is copied, so it must be `Copy` (or name a `const` item)"),
                );
            }
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
        let fieldless = self
            .items
            .as_enum(from)
            .is_some_and(crate::collect::AdtDef::is_fieldless_enum);
        // rustc rejects casting an enum that implements `Drop` (formerly the
        // `cenum_impl_drop_cast` lint): the cast would read a value whose drop then runs.
        let drop_impl = matches!(from, Ty::Adt(d, _) if self.items.drop_impl(*d).is_some());
        let ok = match (from, to) {
            (Ty::Prim(f), Ty::Prim(t)) => {
                (f.is_numeric() && t.is_numeric())
                    || (*f == PrimTy::Bool && t.is_integer())
                    || (*f == PrimTy::Char && t.is_integer())
                    || (*f == PrimTy::U8 && *t == PrimTy::Char)
            }
            // A fieldless enum casts to its discriminant.
            (Ty::Adt(..), Ty::Prim(t)) if fieldless => t.is_integer() && !drop_impl,
            _ => false,
        };
        if ok {
            return;
        }
        let (f, t) = (self.ty_str(from), self.ty_str(to));
        let d = match (from, to) {
            (Ty::Adt(..), Ty::Prim(p)) if fieldless && drop_impl && p.is_integer() => {
                Diagnostic::error(
                    ErrorCode::INVALID_CAST,
                    format!(
                        "cannot cast enum `{f}` into integer `{t}` because it implements `Drop`"
                    ),
                )
                .with_label(span, "invalid cast")
            }
            (Ty::Adt(..), Ty::Prim(PrimTy::Bool)) if fieldless => Diagnostic::error(
                ErrorCode::INVALID_CAST,
                format!("cannot cast `{f}` as `bool`"),
            )
            .with_label(span, "invalid cast")
            .with_note("compare the discriminant instead: `e as i32 != 0`"),
            (Ty::Adt(..), Ty::Prim(_)) if fieldless => Diagnostic::error(
                ErrorCode::INVALID_CAST,
                format!("casting `{f}` as `{t}` is invalid"),
            )
            .with_label(span, "invalid cast")
            .with_note("an enum casts to an integer type only"),
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
    // `str` is compared behind its reference (#10): `s == t` for `s, t: &str`.
    matches!(
        ty,
        Ty::Error | Ty::Never | Ty::Infer(_) | Ty::Unit | Ty::Prim(_)
    )
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

/// The path a path, tuple-struct or struct pattern names.
fn pat_path(pat: &Pat) -> Option<&sabiko_ast::Path> {
    match &pat.kind {
        PatKind::Path(path) | PatKind::TupleStruct(path, _) | PatKind::Struct { path, .. } => {
            Some(path)
        }
        _ => None,
    }
}

fn strip_parens(mut e: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    e
}

/// `1 element`, `3 elements`.
/// For each of `pats` (at most one `..` among them), the index of the element or field
/// it matches among `len`; `None` for the `..` itself.
/// Whether `ty` holds a value of the type parameter `param` itself (not behind a
/// reference): what needs it to be sized.
fn holds_by_value(ty: &Ty, param: DefId) -> bool {
    match ty {
        Ty::Param(p) => *p == param,
        Ty::Array(inner, _) | Ty::Slice(inner) => holds_by_value(inner, param),
        Ty::Tuple(elems) => elems.iter().any(|e| holds_by_value(e, param)),
        Ty::Adt(_, args) => args.iter().any(|a| holds_by_value(a, param)),
        _ => false,
    }
}

/// How many nodes of `ty` are not inference variables: what selection learning a type
/// increases.
fn known_nodes(ty: &Ty) -> usize {
    match ty {
        Ty::Infer(_) => 0,
        Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => 1 + known_nodes(inner),
        Ty::Tuple(elems) => 1 + elems.iter().map(known_nodes).sum::<usize>(),
        Ty::Adt(_, args) => 1 + args.iter().map(known_nodes).sum::<usize>(),
        _ => 1,
    }
}

pub fn rest_positions(pats: &[Pat], len: usize) -> Vec<Option<usize>> {
    let rest = pats.iter().position(|p| matches!(p.kind, PatKind::Rest));
    let after = rest.map_or(0, |r| pats.len() - r - 1);
    pats.iter()
        .enumerate()
        .map(|(i, _)| match rest {
            Some(r) if i == r => None,
            Some(r) if i > r => Some(len.saturating_sub(after) + (i - r - 1)),
            _ => Some(i),
        })
        .collect()
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        n => format!("{} and {}", items[..n - 1].join(", "), items[n - 1]),
    }
}

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

/// Whether `pat` binds a name with an explicit `ref` or `ref mut` (rustc's
/// `contains_explicit_ref_binding`; not a by-reference default binding mode).
fn has_explicit_ref_binding(pat: &Pat) -> bool {
    let mut found = false;
    pat.walk_idents(&mut |p| {
        if let PatKind::Ident { by_ref: true, .. } = p.kind {
            found = true;
        }
    });
    found
}

/// Whether Rust's standard library implements `From<from> for to` for two primitive types
/// (`from != to`): the lossless conversions between integers, floats, `bool` and `char`.
fn prim_from(from: PrimTy, to: PrimTy) -> bool {
    use PrimTy::{
        Bool, Char, Isize, Usize, F32, F64, I128, I16, I32, I64, I8, U128, U16, U32, U64, U8,
    };
    match from {
        Bool => to.is_integer() || to.is_float(),
        Char => matches!(to, U32 | U64 | U128),
        U8 => matches!(
            to,
            Char | U16 | U32 | U64 | U128 | Usize | I16 | I32 | I64 | I128 | Isize | F32 | F64
        ),
        U16 => matches!(to, U32 | U64 | U128 | Usize | I32 | I64 | I128 | F32 | F64),
        U32 => matches!(to, U64 | U128 | I64 | I128 | F64),
        U64 => matches!(to, U128 | I128),
        I8 => matches!(to, I16 | I32 | I64 | I128 | Isize | F32 | F64),
        I16 => matches!(to, I32 | I64 | I128 | Isize | F32 | F64),
        I32 => matches!(to, I64 | I128 | F64),
        I64 => matches!(to, I128),
        F32 => matches!(to, F64),
        _ => false,
    }
}
