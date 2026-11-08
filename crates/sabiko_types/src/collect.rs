//! Collects item signatures: function types, struct shapes and constant types. Runs
//! before any body is checked so bodies may refer to items in any order.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{ItemKind, NodeId, SelfKind, SourceFile, StructKind, Type, TypeKind, VariantData};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{Builtin, DefId, DefKind, PrimTy, Res, ResolveResults};
use sabiko_span::Span;

use crate::ty::{Mutability, Ty};

/// The evaluated lengths of the array types in item signatures, keyed by the length
/// expression's node. A length missing here failed to evaluate (and was reported).
pub(crate) type ArrayLens = HashMap<NodeId, u64>;

/// Whether `Print`/`PrintLine` accept a value of type `ty`: every integer and float
/// primitive, `bool`, `char` and `&str` (either mutability). The runtime library has one
/// overload per printable type.
#[must_use]
pub fn is_printable(ty: &Ty) -> bool {
    match ty {
        Ty::Prim(p) => *p != PrimTy::Str,
        Ty::Ref(_, inner) => **inner == Ty::Prim(PrimTy::Str),
        _ => false,
    }
}

/// How many references a `Print`/`PrintLine` argument of type `ty` is printed through:
/// `Some(0)` for a printable value, `Some(n)` for `n` references to one (`&i32`, `&&str`,
/// `&mut f64`: rustc's `impl<T: Display> Display for &T` and `&mut T`, #26), `None` when
/// nothing printable is behind it. The argument is dereferenced that many times when the
/// call is lowered.
#[must_use]
pub fn print_derefs(ty: &Ty) -> Option<usize> {
    let mut ty = ty;
    let mut n = 0;
    loop {
        if is_printable(ty) {
            return Some(n);
        }
        let Ty::Ref(_, inner) = ty else {
            return None;
        };
        ty = inner;
        n += 1;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnSig {
    /// The type parameters, in declaration order; empty unless the function is generic.
    /// `inputs` and `output` mention them as `Ty::Param`.
    pub generics: Vec<DefId>,
    /// The parameter types, the receiver's (`P`, `&P` or `&mut P`) first for a method.
    pub inputs: Vec<Ty>,
    pub output: Ty,
    /// How a method takes its receiver, `inputs[0]`; `None` for a function without a
    /// `self` parameter.
    pub self_kind: Option<SelfKind>,
    /// Span of the whole `fn` header's name, for "function defined here" labels.
    pub name_span: Span,
    /// Spans of the parameters, for arity diagnostics.
    pub param_spans: Vec<Span>,
}

impl FnSig {
    /// The parameter types with the type parameters replaced by `args`, the generic
    /// arguments of a call (one per parameter in `generics`).
    #[must_use]
    pub fn inputs_with(&self, args: &[Ty]) -> Vec<Ty> {
        self.inputs
            .iter()
            .map(|t| t.subst(&self.generics, args))
            .collect()
    }

    /// The return type with the type parameters replaced by `args`.
    #[must_use]
    pub fn output_with(&self, args: &[Ty]) -> Ty {
        self.output.subst(&self.generics, args)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    pub span: Span,
}

/// A struct, or an enum (`variants` is `Some`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdtDef {
    pub name: String,
    /// The type parameters, in order; empty unless the type is generic. Field types
    /// mention them as `Ty::Param`: [`AdtDef::field_ty`] substitutes an instance's
    /// arguments.
    pub generics: Vec<DefId>,
    /// A struct's fields; empty for an enum, whose fields are its variants'.
    pub fields: Vec<FieldDef>,
    /// `struct S;`
    pub is_unit: bool,
    /// `#[derive(Copy)]`: values are copied, not moved.
    pub is_copy: bool,
    pub span: Span,
    /// The variants of an enum, in declaration order; `None` for a struct.
    pub variants: Option<Vec<VariantDef>>,
}

impl AdtDef {
    /// The type `ty` of one of this type's fields, in the instance with generic arguments
    /// `args` (empty for a type that is not generic).
    #[must_use]
    pub fn field_ty(&self, ty: &Ty, args: &[Ty]) -> Ty {
        if self.generics.len() == args.len() {
            ty.subst(&self.generics, args)
        } else {
            // An error type's arguments (or a generic type named without them, already
            // reported).
            ty.subst(&self.generics, &vec![Ty::Error; self.generics.len()])
        }
    }

    #[must_use]
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    #[must_use]
    pub fn is_enum(&self) -> bool {
        self.variants.is_some()
    }

    /// The variants of an enum; empty for a struct.
    #[must_use]
    pub fn variants(&self) -> &[VariantDef] {
        self.variants.as_deref().unwrap_or(&[])
    }

    /// The variant at `index` of an enum.
    #[must_use]
    pub fn variant(&self, index: usize) -> &VariantDef {
        &self.variants()[index]
    }

    /// Every field type of the value: a struct's fields, or every variant's fields.
    pub fn all_field_tys(&self) -> impl Iterator<Item = &Ty> {
        self.fields
            .iter()
            .chain(self.variants().iter().flat_map(|v| v.fields.iter()))
            .map(|f| &f.ty)
    }

    /// Whether every variant is a unit variant (and there is at least one): `as` casts the
    /// value to its discriminant.
    #[must_use]
    pub fn is_fieldless_enum(&self) -> bool {
        self.variants
            .as_ref()
            .is_some_and(|vs| vs.iter().all(|v| v.kind == VariantKind::Unit))
    }
}

/// How a variant is written: `A`, `B(T)` or `C { F: T }` (or a struct: unit or named).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VariantKind {
    Unit,
    Tuple,
    Named,
}

/// One variant of an enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantDef {
    /// The variant's own definition (`DefKind::Variant`).
    pub def: DefId,
    pub name: String,
    pub kind: VariantKind,
    /// Its fields; a tuple variant's are named `0`, `1`, ...
    pub fields: Vec<FieldDef>,
    /// The discriminant value, filled in after constant evaluation (decision 15): explicit,
    /// or one more than the previous variant's (0 for the first).
    pub discr: i128,
    /// Whether the discriminant derives from an explicit one (this variant's or an earlier
    /// variant's `= value`): rustc then computes a cast's value with a checked addition,
    /// which its constant-propagation lints see (decision 73).
    pub explicit_base: bool,
    pub span: Span,
}

/// Signatures of every item in the file, plus the compiler builtins.
#[derive(Clone, Debug, Default)]
pub struct ItemTypes {
    fn_sigs: HashMap<DefId, FnSig>,
    adts: HashMap<DefId, AdtDef>,
    const_tys: HashMap<DefId, Ty>,
    /// The builtins that were not shadowed by a user item (see
    /// [`sabiko_resolve::ResolveResults::builtin`]).
    builtins: HashMap<DefId, Builtin>,
    /// The `drop` function of each struct with an `impl Drop` (decision 46).
    drop_impls: HashMap<DefId, DefId>,
    /// The structs with drop glue: an `impl Drop` of their own or a field that has drop
    /// glue (decision 55).
    needs_drop: HashSet<DefId>,
}

impl ItemTypes {
    /// The signature of a user function or of `Panic`. `Print` and `PrintLine` are
    /// overloaded over every printable type and have no signature: check
    /// [`ItemTypes::builtin`] first and use [`is_printable`] for their argument.
    #[must_use]
    pub fn fn_sig(&self, def: DefId) -> &FnSig {
        self.fn_sigs
            .get(&def)
            .unwrap_or_else(|| match self.builtins.get(&def) {
                Some(b) => panic!(
                "builtin `{}` is overloaded and has no `FnSig`; check `ItemTypes::builtin` first",
                b.name()
            ),
                None => panic!("no signature recorded for {def:?}"),
            })
    }

    /// `Some` when `def` is a compiler builtin. Same answer as
    /// `ResolveResults::builtin`, available to the phases that only hold the type tables.
    #[must_use]
    pub fn builtin(&self, def: DefId) -> Option<Builtin> {
        self.builtins.get(&def).copied()
    }

    #[must_use]
    pub fn adt(&self, def: DefId) -> &AdtDef {
        &self.adts[&def]
    }

    /// The field types of the struct `ty`, or of its variant `variant` when it is an enum,
    /// at `ty`'s generic arguments. Empty for any other type.
    #[must_use]
    pub fn field_tys(&self, ty: &Ty, variant: Option<usize>) -> Vec<Ty> {
        let Ty::Adt(def, args) = ty else {
            return Vec::new();
        };
        let adt = self.adt(*def);
        let fields = match variant {
            Some(v) => &adt.variant(v).fields,
            None => &adt.fields,
        };
        fields.iter().map(|f| adt.field_ty(&f.ty, args)).collect()
    }

    /// The enum a type names, if it is one.
    #[must_use]
    pub fn as_enum(&self, ty: &Ty) -> Option<&AdtDef> {
        match ty {
            Ty::Adt(def, _) => self.adts.get(def).filter(|a| a.is_enum()),
            _ => None,
        }
    }

    pub(crate) fn set_discriminants(&mut self, adt: DefId, discrs: &[i128]) {
        if let Some(variants) = self.adts.get_mut(&adt).and_then(|a| a.variants.as_mut()) {
            for (v, &d) in variants.iter_mut().zip(discrs) {
                v.discr = d;
            }
        }
    }

    #[must_use]
    pub fn const_ty(&self, def: DefId) -> &Ty {
        &self.const_tys[&def]
    }

    /// The `drop` function of the struct `adt`'s `impl Drop`, if it has one.
    #[must_use]
    pub fn drop_impl(&self, adt: DefId) -> Option<DefId> {
        self.drop_impls.get(&adt).copied()
    }

    /// Whether a value of `ty` has drop glue: a struct or enum with an `impl Drop`, or a
    /// struct, enum or tuple with a field (element) that has drop glue. References,
    /// primitives, `()` and `Copy` types never do.
    #[must_use]
    pub fn needs_drop(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Adt(def, args) if args.is_empty() => self.needs_drop.contains(def),
            // An instance of a generic type: its own `impl Drop`, or a field that needs drop
            // at these arguments.
            Ty::Adt(def, args) => {
                self.drop_impls.contains_key(def)
                    || self.adts.get(def).is_some_and(|a| {
                        a.all_field_tys()
                            .any(|t| self.needs_drop(&a.field_ty(t, args)))
                    })
            }
            Ty::Tuple(elems) => elems.iter().any(|e| self.needs_drop(e)),
            // An empty array holds nothing to drop, as in rustc.
            Ty::Array(elem, n) => *n > 0 && self.needs_drop(elem),
            // It may be instantiated with a type that has drop glue, so a generic body
            // drops it, as rustc's does; monomorphisation removes the drops an instance
            // does not need (decision 82).
            Ty::Param(_) => true,
            _ => false,
        }
    }

    /// Whether values of `ty` are copied rather than moved. Like [`Ty::is_copy`] but
    /// aware of `#[derive(Copy)]` on structs and enums.
    #[must_use]
    pub fn is_copy(&self, ty: &Ty) -> bool {
        match ty {
            // A derived `Copy` of a generic type requires its arguments to be `Copy`, as
            // rustc's derive does.
            Ty::Adt(def, args) => {
                self.adts.get(def).is_some_and(|a| a.is_copy)
                    && args.iter().all(|a| self.is_copy(a))
            }
            Ty::Tuple(elems) => elems.iter().all(|e| self.is_copy(e)),
            Ty::Array(elem, _) => self.is_copy(elem),
            _ => ty.is_copy(),
        }
    }
}

/// Reads `#[derive(...)]` attributes. `Copy` is understood, `Clone` is accepted as its
/// companion (every `Copy` type clones trivially); everything else waits for traits.
fn derives_copy(item: &sabiko_ast::Item, sink: &mut DiagnosticSink) -> bool {
    let mut is_copy = false;
    for attr in &item.attrs {
        if !attr.path.is_single() || attr.path.segments[0].name != "derive" {
            continue;
        }
        let list = attr
            .args
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')');
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "Copy" => is_copy = true,
                "Clone" => {}
                other => sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNSUPPORTED_SYNTAX,
                        format!("not yet supported: `#[derive({other})]`"),
                    )
                    .with_label(attr.span, "only `Copy` and `Clone` can be derived for now")
                    .with_note("derived traits arrive with traits in Phase 5; see docs/PLAN.md"),
                ),
            }
        }
    }
    is_copy
}

/// References inside struct fields (also inside a tuple-typed field) need lifetime
/// parameters, which arrive in Phase 9.
fn check_no_reference(ty: &Ty, span: Span, sink: &mut DiagnosticSink) {
    if ty.has_ref() {
        sink.emit(
            Diagnostic::error(
                ErrorCode::UNSUPPORTED_SYNTAX,
                "not yet supported: references in struct fields",
            )
            .with_label(
                span,
                "a struct holding a reference needs a lifetime parameter",
            )
            .with_note("planned for Phase 9; see docs/PLAN.md"),
        );
    }
}

/// The fields of a struct or a struct variant; the resolver reported duplicate names, and
/// the first of each is kept.
fn lower_fields(
    fields: &[sabiko_ast::FieldDef],
    resolve: &ResolveResults,
    lens: &ArrayLens,
    sink: &mut DiagnosticSink,
) -> Vec<FieldDef> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for f in fields {
        if !seen.insert(f.name.name.as_str()) {
            continue;
        }
        out.push(field_def(
            f.name.name.clone(),
            &f.ty,
            f.span,
            resolve,
            lens,
            sink,
        ));
    }
    out
}

fn field_def(
    name: String,
    ty: &Type,
    span: Span,
    resolve: &ResolveResults,
    lens: &ArrayLens,
    sink: &mut DiagnosticSink,
) -> FieldDef {
    let lowered = lower_sig_type(ty, resolve, lens);
    check_sized(&lowered, ty.span, sink);
    check_no_reference(&lowered, ty.span, sink);
    FieldDef {
        name,
        ty: lowered,
        span,
    }
}

/// Lowers a syntactic type to a semantic one. `Infer` becomes `Error` here: this is only
/// used for signatures, where the resolver already rejected `_`. An array length comes
/// from `lens`; one that failed to evaluate makes the array an error type.
pub(crate) fn lower_sig_type(ty: &Type, resolve: &ResolveResults, lens: &ArrayLens) -> Ty {
    match &ty.kind {
        TypeKind::Path(path) => match resolve.res(ty.id) {
            Res::Prim(p) => Ty::Prim(p),
            Res::Def(d) if resolve.def(d).kind == DefKind::TyParam => Ty::Param(d),
            Res::Def(d) => adt_type(d, path, resolve, &mut |t| lower_sig_type(t, resolve, lens)),
            Res::Local(_) => unreachable!("locals never resolve in type position"),
            Res::Err => Ty::Error,
        },
        TypeKind::Ref { mutable, inner } => Ty::Ref(
            Mutability::from_bool(*mutable),
            Box::new(lower_sig_type(inner, resolve, lens)),
        ),
        TypeKind::Unit => Ty::Unit,
        TypeKind::Tuple(elems) => Ty::Tuple(
            elems
                .iter()
                .map(|t| lower_sig_type(t, resolve, lens))
                .collect::<Vec<_>>()
                .into(),
        ),
        TypeKind::Array(elem, len) => {
            let elem = lower_sig_type(elem, resolve, lens);
            match lens.get(&len.id) {
                Some(&n) if !elem.is_error() => Ty::Array(Box::new(elem), n),
                _ => Ty::Error,
            }
        }
        TypeKind::Slice(elem) => Ty::Slice(Box::new(lower_sig_type(elem, resolve, lens))),
        TypeKind::Infer => Ty::Error,
    }
}

/// The struct or enum `adt` named by `path` in a type: with the path's generic arguments,
/// lowered by `lower`, or with its own parameters for `Self` inside its `impl` block. An
/// error type when the arguments are missing or of the wrong number (the resolver
/// reported it).
pub(crate) fn adt_type(
    adt: DefId,
    path: &sabiko_ast::Path,
    resolve: &ResolveResults,
    lower: &mut dyn FnMut(&Type) -> Ty,
) -> Ty {
    let generics = resolve.generics(adt);
    if generics.is_empty() {
        return Ty::adt(adt);
    }
    if path.last().name == "Self" {
        return Ty::Adt(adt, generics.iter().map(|&p| Ty::Param(p)).collect());
    }
    match path.args_of(path.segments.len() - 1) {
        Some(written) if written.args.len() == generics.len() => {
            Ty::Adt(adt, written.args.iter().map(lower).collect())
        }
        _ => Ty::Error,
    }
}

/// `str` and `[T]` are only valid behind a reference, not by value nor as a tuple or
/// array element; a slice's element must be sized. Reports and returns `false` otherwise.
pub(crate) fn check_sized(ty: &Ty, span: Span, sink: &mut DiagnosticSink) -> bool {
    match ty {
        Ty::Tuple(elems) => elems.iter().all(|e| check_sized(e, span, sink)),
        Ty::Array(elem, _) => check_sized(elem, span, sink),
        Ty::Ref(_, inner) => match &**inner {
            Ty::Slice(elem) => check_sized(elem, span, sink),
            _ => true,
        },
        Ty::Prim(PrimTy::Str) => {
            sink.emit(
                Diagnostic::error(
                    ErrorCode::UNSIZED_TYPE,
                    "the size for values of type `str` cannot be known at compilation time",
                )
                .with_label(span, "doesn't have a size known at compile-time")
                .with_note("string values are used behind a reference: `&str`"),
            );
            false
        }
        Ty::Slice(_) => {
            sink.emit(
                Diagnostic::error(
                    ErrorCode::UNSIZED_TYPE,
                    format!(
                        "the size for values of type `{}` cannot be known at compilation time",
                        slice_display(ty)
                    ),
                )
                .with_label(span, "doesn't have a size known at compile-time")
                .with_note("slices are used behind a reference: `&[T]` or `&mut [T]`"),
            );
            false
        }
        _ => true,
    }
}

/// A slice type for a message, without item names (`[i32]`, `[[u8; 2]]`, `[_]`).
fn slice_display(ty: &Ty) -> String {
    match ty {
        Ty::Prim(p) => p.name().to_owned(),
        Ty::Slice(e) => format!("[{}]", slice_display(e)),
        Ty::Array(e, n) => format!("[{}; {n}]", slice_display(e)),
        Ty::Ref(m, e) => format!("&{}{}", m.prefix_str(), slice_display(e)),
        Ty::Unit => "()".to_owned(),
        _ => "_".to_owned(),
    }
}

/// The signature of a function; a method's receiver type is its first input.
fn fn_sig(
    def: DefId,
    f: &sabiko_ast::FnDecl,
    resolve: &ResolveResults,
    lens: &ArrayLens,
    sink: &mut DiagnosticSink,
) -> FnSig {
    let mut inputs = Vec::new();
    let mut param_spans = Vec::new();
    if let Some(sp) = &f.self_param {
        // `Self` is the struct of the `impl` block; an error type when that failed to
        // resolve (reported with the block).
        let self_ty = resolve.def(def).parent.map_or(Ty::Error, |adt| {
            Ty::Adt(
                adt,
                resolve
                    .generics(adt)
                    .iter()
                    .map(|&p| Ty::Param(p))
                    .collect(),
            )
        });
        inputs.push(match sp.kind {
            SelfKind::Value { .. } => self_ty,
            SelfKind::Ref { mutable } => Ty::Ref(Mutability::from_bool(mutable), Box::new(self_ty)),
        });
        param_spans.push(sp.span);
    }
    for p in &f.params {
        let ty = lower_sig_type(&p.ty, resolve, lens);
        check_sized(&ty, p.ty.span, sink);
        inputs.push(ty);
        param_spans.push(p.span);
    }
    let output = match &f.ret {
        Some(t) => {
            let ty = lower_sig_type(t, resolve, lens);
            check_sized(&ty, t.span, sink);
            ty
        }
        None => Ty::Unit,
    };
    FnSig {
        generics: resolve.generics(def).to_vec(),
        inputs,
        output,
        self_kind: f.self_param.as_ref().map(|sp| sp.kind),
        name_span: f.name.span,
        param_spans,
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn collect(
    file: &SourceFile,
    resolve: &ResolveResults,
    lens: &ArrayLens,
    sink: &mut DiagnosticSink,
) -> ItemTypes {
    let mut items = ItemTypes::default();
    for item in file.all_items() {
        if let ItemKind::Impl(_) = item.kind {
            continue;
        }
        let def = resolve.def_of_item(item.id);
        match &item.kind {
            ItemKind::Impl(_) => unreachable!("skipped above"),
            ItemKind::Fn(f) => {
                items
                    .fn_sigs
                    .insert(def, fn_sig(def, f, resolve, lens, sink));
            }
            ItemKind::Struct(s) => {
                let (fields, is_unit) = match &s.kind {
                    StructKind::Named(fields) => (lower_fields(fields, resolve, lens, sink), false),
                    StructKind::Unit => (Vec::new(), true),
                };
                let is_copy = derives_copy(item, sink);
                items.adts.insert(
                    def,
                    AdtDef {
                        name: s.name.name.clone(),
                        generics: resolve.generics(def).to_vec(),
                        fields,
                        is_unit,
                        is_copy,
                        span: s.name.span,
                        variants: None,
                    },
                );
            }
            ItemKind::Const(c) => {
                let mut ty = lower_sig_type(&c.ty, resolve, lens);
                check_sized(&ty, c.ty.span, sink);
                if has_array(&ty) {
                    sink.emit(
                        Diagnostic::error(
                            ErrorCode::NON_CONSTANT_EXPRESSION,
                            "arrays cannot be used in constants yet",
                        )
                        .with_label(c.ty.span, "a constant of array type")
                        .with_note("constants are scalars for now; see docs/PLAN.md"),
                    );
                    ty = Ty::Error;
                }
                items.const_tys.insert(def, ty);
            }
            ItemKind::Enum(e) => {
                let mut variants = Vec::new();
                let mut seen = HashSet::new();
                let mut explicit_base = false;
                for v in &e.variants {
                    explicit_base |= v.discriminant.is_some();
                    // The resolver reported duplicates and defined the first only.
                    if !seen.insert(v.name.name.as_str()) {
                        continue;
                    }
                    let (kind, fields) = match &v.data {
                        VariantData::Unit => (VariantKind::Unit, Vec::new()),
                        VariantData::Tuple(tys) => (
                            VariantKind::Tuple,
                            tys.iter()
                                .enumerate()
                                .map(|(i, t)| {
                                    field_def(i.to_string(), t, t.span, resolve, lens, sink)
                                })
                                .collect(),
                        ),
                        VariantData::Named(fields) => (
                            VariantKind::Named,
                            lower_fields(fields, resolve, lens, sink),
                        ),
                    };
                    variants.push(VariantDef {
                        def: resolve.def_of_item(v.id),
                        name: v.name.name.clone(),
                        kind,
                        fields,
                        discr: 0,
                        explicit_base,
                        span: v.name.span,
                    });
                }
                let is_copy = derives_copy(item, sink);
                items.adts.insert(
                    def,
                    AdtDef {
                        name: e.name.name.clone(),
                        generics: resolve.generics(def).to_vec(),
                        fields: Vec::new(),
                        is_unit: false,
                        is_copy,
                        span: e.name.span,
                        variants: Some(variants),
                    },
                );
            }
        }
    }
    for def in resolve.defs_of_kind(DefKind::Fn) {
        let Some(builtin) = resolve.builtin(def) else {
            continue;
        };
        items.builtins.insert(def, builtin);
        match builtin {
            // Overloaded over every printable type; `check_call` handles them without a
            // signature.
            Builtin::Print | Builtin::PrintLine => {}
            Builtin::Panic => {
                items.fn_sigs.insert(
                    def,
                    FnSig {
                        generics: Vec::new(),
                        inputs: vec![Ty::shared_ref(Ty::Prim(PrimTy::Str))],
                        output: Ty::Never,
                        self_kind: None,
                        name_span: Span::dummy(),
                        param_spans: vec![Span::dummy()],
                    },
                );
            }
        }
    }
    for adt in adt_defs(resolve) {
        if let Some(def) = resolve.drop_impl(adt) {
            items.drop_impls.insert(adt, def);
            if let Some(f) = file.all_items().find_map(|item| match &item.kind {
                ItemKind::Fn(f) if resolve.def_of_item(item.id) == def => Some(f),
                _ => None,
            }) {
                check_drop_signature(resolve, adt, f, lens, sink);
            }
        }
    }
    for (def, c) in resolve.prim_consts() {
        items.const_tys.insert(def, crate::consts::prim_const(c).0);
    }
    check_recursive_structs(resolve, &items, sink);
    items.needs_drop = structs_needing_drop(resolve, &items);
    check_unused_params(resolve, &items, sink);
    check_copy_derives(resolve, &items, sink);
    items
}

/// `drop` must be declared exactly `fn drop(&mut self)`: E0321 for another receiver or a
/// return type (rustc's E0053, E0186), E0322 for parameters after `&mut self` (E0050).
fn check_drop_signature(
    resolve: &ResolveResults,
    adt: DefId,
    f: &sabiko_ast::FnDecl,
    lens: &ArrayLens,
    sink: &mut DiagnosticSink,
) {
    let name = &resolve.def(adt).name;
    let incompatible = |span: Span, label: String| {
        Diagnostic::error(
            ErrorCode::DROP_SIGNATURE_MISMATCH,
            "method `drop` has an incompatible type for trait `Drop`",
        )
        .with_label(span, label)
        .with_note("`Drop::drop` is declared as `fn drop(&mut self)`")
    };
    match f.self_param.as_ref().map(|sp| (sp.kind, sp.span)) {
        Some((SelfKind::Ref { mutable: true }, _)) => {}
        Some((SelfKind::Ref { mutable: false }, span)) => sink.emit(incompatible(
            span,
            format!("expected `&mut {name}`, found `&{name}`: types differ in mutability"),
        )),
        Some((SelfKind::Value { .. }, span)) => sink.emit(incompatible(
            span,
            format!("expected `&mut {name}`, found `{name}`"),
        )),
        None => sink.emit(
            Diagnostic::error(
                ErrorCode::DROP_SIGNATURE_MISMATCH,
                "method `drop` has a `&mut self` declaration in the trait, but not in the impl",
            )
            .with_label(f.name.span, "expected `&mut self` in impl"),
        ),
    }
    if let Some(ret) = &f.ret {
        let ty = lower_sig_type(ret, resolve, lens);
        if !ty.is_unit() {
            sink.emit(incompatible(
                ret.span,
                format!("expected `()`, found `{}`", ty.display(resolve)),
            ));
        }
    }
    if !f.params.is_empty() {
        let found = f.params.len() + 1;
        let start = f.params[0].span;
        let end = f.params[f.params.len() - 1].span;
        sink.emit(
            Diagnostic::error(
                ErrorCode::DROP_PARAM_COUNT,
                format!(
                    "method `drop` has {found} parameters but the declaration in trait `Drop::drop` has 1"
                ),
            )
            .with_label(
                Span::new(start.file, start.start, end.end),
                format!("expected 1 parameter, found {found}"),
            ),
        );
    }
}

/// The structs and enums of the file, in source order.
fn adt_defs(resolve: &ResolveResults) -> impl Iterator<Item = DefId> + '_ {
    resolve
        .defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind.is_type())
        .map(|(i, _)| DefId(u32::try_from(i).expect("fewer than 2^32 items")))
}

/// The structs whose values have drop glue (see [`ItemTypes::needs_drop`]), to a fixpoint:
/// a struct needs drop when it has an `impl Drop` or a field type that needs drop.
fn structs_needing_drop(resolve: &ResolveResults, items: &ItemTypes) -> HashSet<DefId> {
    let mut set: HashSet<DefId> = items.drop_impls.keys().copied().collect();
    let structs: Vec<DefId> = adt_defs(resolve).collect();
    loop {
        let before = set.len();
        let probe = ItemTypes {
            needs_drop: set.clone(),
            adts: items.adts.clone(),
            drop_impls: items.drop_impls.clone(),
            ..ItemTypes::default()
        };
        for &def in &structs {
            if items.adt(def).all_field_tys().any(|t| probe.needs_drop(t)) {
                set.insert(def);
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

/// Every type parameter of a struct or enum must appear in a field (E0340, rustc's
/// E0392): an instance's references, drop glue and layout come from its fields.
fn check_unused_params(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    for def in adt_defs(resolve) {
        let adt = items.adt(def);
        for &param in &adt.generics {
            if adt.all_field_tys().any(|t| t.mentions(param)) {
                continue;
            }
            let info = resolve.def(param);
            sink.emit(
                Diagnostic::error(
                    ErrorCode::UNUSED_TYPE_PARAMETER,
                    format!("type parameter `{}` is never used", info.name),
                )
                .with_label(info.span, "unused type parameter")
                .with_note(format!(
                    "use `{}` in a field of `{}`, or remove it",
                    info.name, adt.name
                )),
            );
        }
    }
}

/// `#[derive(Copy)]` requires every field to be `Copy` (E0417) and no `impl Drop` (E0324):
/// a copy would be dropped twice.
fn check_copy_derives(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    for def in adt_defs(resolve) {
        let adt = items.adt(def);
        if !adt.is_copy {
            continue;
        }
        if items.drop_impl(def).is_some() {
            sink.emit(
                Diagnostic::error(
                    ErrorCode::COPY_WITH_DESTRUCTOR,
                    "the trait `Copy` cannot be implemented for this type; the type has a destructor",
                )
                .with_label(adt.span, "`Copy` not allowed on types with destructors")
                .with_note(format!(
                    "`{}` implements `Drop`; a copy would be dropped twice",
                    adt.name
                )),
            );
        }
        let variant_fields = adt.variants().iter().flat_map(|v| v.fields.iter());
        for field in adt.fields.iter().chain(variant_fields) {
            // The derive requires `T: Copy` of each parameter, so a parameter is `Copy`
            // here (any `Copy` type stands for it).
            let assumed = field.ty.map_params(&mut |_| Some(Ty::Unit));
            if !items.is_copy(&assumed) {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::COPY_NOT_IMPLEMENTABLE,
                        format!(
                            "the trait `Copy` cannot be implemented for type `{}`",
                            adt.name
                        ),
                    )
                    .with_label(
                        field.span,
                        format!(
                            "this field does not implement `Copy`: `{}`",
                            field.ty.display(resolve)
                        ),
                    )
                    .with_label(adt.span, "`Copy` derived here"),
                );
            }
        }
    }
}

/// A struct that contains itself by value has infinite size. Mutually recursive structs
/// are reported once, together.
fn check_recursive_structs(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    let mut reported: HashSet<DefId> = HashSet::new();
    let mut done = HashSet::new();
    for def in adt_defs(resolve) {
        if reported.contains(&def) || done.contains(&def) {
            continue;
        }
        let mut stack = vec![def];
        if let Some(cycle) = find_cycle(def, items, &mut stack, &mut done) {
            reported.extend(cycle.iter().copied());
            let names: Vec<String> = cycle
                .iter()
                .map(|d| format!("`{}`", items.adt(*d).name))
                .collect();
            let (what, list) = if names.len() == 1 {
                ("type", names[0].clone())
            } else {
                (
                    "types",
                    format!(
                        "{} and {}",
                        names[..names.len() - 1].join(", "),
                        names[names.len() - 1]
                    ),
                )
            };
            let mut d = Diagnostic::error(
                ErrorCode::RECURSIVE_TYPE,
                format!("recursive {what} {list} have infinite size"),
            );
            if names.len() == 1 {
                d.message = format!("recursive type {list} has infinite size");
            }
            for member in &cycle {
                d = d.with_label(items.adt(*member).span, "recursive type has infinite size");
            }
            sink.emit(d.with_note(
                "a struct or enum cannot contain itself by value; indirection (`Box<T>`) arrives in Phase 7",
            ));
        }
    }
}

/// Whether `ty` holds an array anywhere (by value or behind a reference).
fn has_array(ty: &Ty) -> bool {
    match ty {
        Ty::Array(..) | Ty::Slice(_) => true,
        Ty::Ref(_, inner) => has_array(inner),
        Ty::Tuple(elems) => elems.iter().any(has_array),
        _ => false,
    }
}

/// The structs `ty` holds by value: itself, or the elements of a tuple or an array,
/// recursively.
fn structs_by_value(ty: &Ty, out: &mut Vec<DefId>) {
    match ty {
        // Each argument of a generic type is held by value too: fields hold no references.
        Ty::Adt(def, args) => {
            out.push(*def);
            for a in args {
                structs_by_value(a, out);
            }
        }
        Ty::Array(elem, _) => structs_by_value(elem, out),
        Ty::Tuple(elems) => {
            for e in elems.iter() {
                structs_by_value(e, out);
            }
        }
        _ => {}
    }
}

/// Depth-first search for a by-value cycle starting at `current` (already on `stack`).
/// Returns the members of the cycle, in source order.
fn find_cycle(
    current: DefId,
    items: &ItemTypes,
    stack: &mut Vec<DefId>,
    done: &mut HashSet<DefId>,
) -> Option<Vec<DefId>> {
    for field_ty in items.adt(current).all_field_tys() {
        let mut contained = Vec::new();
        structs_by_value(field_ty, &mut contained);
        for next in &contained {
            if let Some(pos) = stack.iter().position(|d| d == next) {
                let mut members = stack[pos..].to_vec();
                members.sort();
                return Some(members);
            }
            if done.contains(next) {
                continue;
            }
            stack.push(*next);
            let found = find_cycle(*next, items, stack, done);
            stack.pop();
            if found.is_some() {
                return found;
            }
        }
    }
    done.insert(current);
    None
}
