//! Collects item signatures: function types, struct shapes and constant types. Runs
//! before any body is checked so bodies may refer to items in any order.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{ItemKind, NodeId, SelfKind, SourceFile, StructKind, Type, TypeKind, VariantData};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{Builtin, DefId, DefKind, LangItem, Predicate, PrimTy, Res, ResolveResults};
use sabiko_span::Span;

use crate::ty::{Mutability, Ty};

/// What lowering a type written in a signature needs besides the resolver's tables: the
/// evaluated lengths of its array types, keyed by the length expression's node (a length
/// missing here failed to evaluate, and was reported), and the type each trait `impl`
/// block is for, which `Self` names in it.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArrayLens {
    lens: HashMap<NodeId, u64>,
    impl_self: HashMap<DefId, Ty>,
}

impl ArrayLens {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, len: NodeId) -> Option<&u64> {
        self.lens.get(&len)
    }

    pub(crate) fn insert(&mut self, len: NodeId, n: u64) {
        self.lens.insert(len, n);
    }
}

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
    /// `#[derive(Copy)]`: values are copied, not moved, when the type arguments are
    /// `Copy` (decision 86).
    pub derives_copy: bool,
    /// `#[derive(Clone)]`: the type is `Clone` when its type arguments are.
    pub derives_clone: bool,
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

/// A trait of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitDef {
    pub name: String,
    /// Its `Self` type parameter, which its members' signatures mention.
    pub self_param: DefId,
    /// The supertraits, as written.
    pub supertraits: Vec<DefId>,
    pub span: Span,
}

/// A trait `impl` block whose trait resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplDef {
    /// The block's definition (`DefKind::Impl`), its members' parent.
    pub def: DefId,
    pub trait_: DefId,
    /// Its own type parameters, which `self_ty` and `preds` mention.
    pub generics: Vec<DefId>,
    /// The type it implements the trait for.
    pub self_ty: Ty,
    /// Its bounds.
    pub preds: Vec<Predicate>,
    /// From `impl` to the end of its type.
    pub span: Span,
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
    /// The `drop` method of the `impl Drop` block of each struct or enum (decision 86).
    drop_impls: HashMap<DefId, DefId>,
    /// The structs with drop glue: an `impl Drop` of their own or a field that has drop
    /// glue (decision 55).
    needs_drop: HashSet<DefId>,
    traits: HashMap<DefId, TraitDef>,
    /// The trait `impl` blocks, in source order.
    impls: Vec<ImplDef>,
    /// The language traits (decision 86).
    lang: HashMap<LangItem, DefId>,
    /// The bounds that hold in each function's signature and body: its own, its block's,
    /// and for a trait's member `Self: Trait`. Absent when there are none.
    param_envs: HashMap<DefId, Vec<Predicate>>,
    /// The lengths and `impl` types to lower the types written in bodies with.
    pub(crate) sig_cx: ArrayLens,
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

    /// The trait `def`, if it is one.
    #[must_use]
    pub fn trait_def(&self, def: DefId) -> Option<&TraitDef> {
        self.traits.get(&def)
    }

    /// The trait `impl` blocks of the trait `tr`, in source order.
    pub fn impls_of(&self, tr: DefId) -> impl Iterator<Item = &ImplDef> {
        self.impls.iter().filter(move |i| i.trait_ == tr)
    }

    /// The type `Self` names in the trait `impl` block `def` (an error type when its trait
    /// did not resolve, or it is not one).
    #[must_use]
    pub fn impl_self_ty(&self, def: DefId) -> Ty {
        self.sig_cx
            .impl_self
            .get(&def)
            .cloned()
            .unwrap_or(Ty::Error)
    }

    /// The bounds that hold in the signature and body of the function `def`.
    #[must_use]
    pub fn param_env(&self, def: DefId) -> &[Predicate] {
        self.param_envs.get(&def).map_or(&[], Vec::as_slice)
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

    /// The struct or enum `def`, if it is one.
    pub(crate) fn adt_def(&self, def: DefId) -> Option<&AdtDef> {
        self.adts.get(&def)
    }

    /// The definition of a language trait (decision 86).
    #[must_use]
    pub fn lang_item(&self, item: LangItem) -> Option<DefId> {
        self.lang.get(&item).copied()
    }

    /// Which language trait `def` is, if any.
    #[must_use]
    pub fn lang_of(&self, def: DefId) -> Option<LangItem> {
        self.lang
            .iter()
            .find_map(|(&item, &d)| (d == def).then_some(item))
    }
}

/// Reads `#[derive(...)]` attributes: whether the item derives `Copy` and `Clone`
/// (decision 86). Every other trait waits for #117 (E0107).
fn derives(item: &sabiko_ast::Item, sink: &mut DiagnosticSink) -> (bool, bool) {
    let (mut copy, mut clone) = (false, false);
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
                "Copy" => copy = true,
                "Clone" => clone = true,
                other => sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNSUPPORTED_SYNTAX,
                        format!("not yet supported: `#[derive({other})]`"),
                    )
                    .with_label(attr.span, "only `Copy` and `Clone` can be derived for now")
                    .with_note("the comparison traits and their derives arrive with #117; see docs/PLAN.md"),
                ),
            }
        }
    }
    (copy, clone)
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
        TypeKind::Path(path) => lower_path_type(ty, path, resolve, lens, &mut |t| {
            lower_sig_type(t, resolve, lens)
        }),
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
            match lens.get(len.id) {
                Some(&n) if !elem.is_error() => Ty::Array(Box::new(elem), n),
                _ => Ty::Error,
            }
        }
        TypeKind::Slice(elem) => Ty::Slice(Box::new(lower_sig_type(elem, resolve, lens))),
        TypeKind::Infer => Ty::Error,
        TypeKind::Never => Ty::Never,
    }
}

/// The type the path type `ty` (`path`) names, its generic arguments lowered by
/// `lower_arg`: the signature's lowering or a body's (#137).
pub(crate) fn lower_path_type(
    ty: &Type,
    path: &sabiko_ast::Path,
    resolve: &ResolveResults,
    lens: &ArrayLens,
    lower_arg: &mut dyn FnMut(&Type) -> Ty,
) -> Ty {
    match resolve.res(ty.id) {
        Res::Prim(p) => Ty::Prim(p),
        Res::Def(d) if resolve.def(d).kind == DefKind::TyParam => Ty::Param(d),
        // `Self` in a trait `impl` block.
        Res::Def(d) if resolve.def(d).kind == DefKind::Impl => {
            lens.impl_self.get(&d).cloned().unwrap_or(Ty::Error)
        }
        Res::Def(d) => adt_type(d, path, resolve, lower_arg),
        Res::Local(_) => unreachable!("locals never resolve in type position"),
        Res::Err => Ty::Error,
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

/// A value of type `ty` must be sized, and so must every type `ty` contains where
/// `Sized` is required (see [`check_well_formed`]): `str` and `[T]` are only valid behind
/// a reference. Reports the first unsized type and returns `false` if there is one.
pub(crate) fn check_sized(ty: &Ty, span: Span, sink: &mut DiagnosticSink) -> bool {
    match ty {
        Ty::Prim(PrimTy::Str) | Ty::Slice(_) => report_unsized(ty, span, sink),
        _ => check_well_formed(ty, span, sink),
    }
}

/// Every type inside `ty` that must be sized is: a tuple's elements, an array's or a
/// slice's element, and every generic argument of a struct or enum (each type parameter
/// is `Sized`; there is no `?Sized`), at any depth, behind references too (`&W<[i32]>`,
/// #137). `ty` itself may be unsized (the referent of a reference, an `impl` block's
/// type). One structural walk for every position a type is written in. Reports the first
/// unsized type and returns `false` if there is one.
pub(crate) fn check_well_formed(ty: &Ty, span: Span, sink: &mut DiagnosticSink) -> bool {
    match ty {
        Ty::Tuple(elems) => elems.iter().all(|e| check_sized(e, span, sink)),
        Ty::Array(elem, _) | Ty::Slice(elem) => check_sized(elem, span, sink),
        Ty::Adt(_, args) => args.iter().all(|a| check_sized(a, span, sink)),
        Ty::Ref(_, inner) => check_well_formed(inner, span, sink),
        _ => true,
    }
}

/// E0208 for the unsized `ty` (`str` or a slice), as rustc's E0277.
fn report_unsized(ty: &Ty, span: Span, sink: &mut DiagnosticSink) -> bool {
    match ty {
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
        _ => unreachable!("only `str` and slices are unsized"),
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
        // `Self` is the struct of an inherent `impl` block, the type of a trait `impl`
        // block, a trait's `Self` parameter; an error type when that failed to resolve
        // (reported with the block).
        let self_ty =
            resolve
                .def(def)
                .parent
                .map_or(Ty::Error, |parent| match resolve.def(parent).kind {
                    DefKind::Trait => Ty::Param(resolve.trait_self(parent)),
                    DefKind::Impl => lens.impl_self.get(&parent).cloned().unwrap_or(Ty::Error),
                    _ => Ty::Adt(
                        parent,
                        resolve
                            .generics(parent)
                            .iter()
                            .map(|&p| Ty::Param(p))
                            .collect(),
                    ),
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
    // The types of the trait `impl` blocks first: `Self` names them in signatures.
    let mut lens = lens.clone();
    for item in &file.items {
        if let ItemKind::Impl(imp) = &item.kind {
            let ty = lower_sig_type(&imp.self_ty, resolve, &lens);
            // The block's type may be unsized itself, not its arguments: `W<[i32]>`.
            check_well_formed(&ty, imp.self_ty.span, sink);
            if imp.trait_ref.is_some() {
                if let Some(def) = resolve.item_def(item.id) {
                    lens.impl_self.insert(def, ty);
                }
            }
        }
    }
    let lens = &lens;
    for item in file.all_items() {
        if let ItemKind::Impl(_) | ItemKind::Trait(_) | ItemKind::Use(_) = item.kind {
            continue;
        }
        let def = resolve.def_of_item(item.id);
        match &item.kind {
            ItemKind::Impl(_) | ItemKind::Trait(_) | ItemKind::Use(_) => {
                unreachable!("skipped above")
            }
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
                let (derives_copy, derives_clone) = derives(item, sink);
                items.adts.insert(
                    def,
                    AdtDef {
                        name: s.name.name.clone(),
                        generics: resolve.generics(def).to_vec(),
                        fields,
                        is_unit,
                        derives_copy,
                        derives_clone,
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
                let (derives_copy, derives_clone) = derives(item, sink);
                items.adts.insert(
                    def,
                    AdtDef {
                        name: e.name.name.clone(),
                        generics: resolve.generics(def).to_vec(),
                        fields: Vec::new(),
                        is_unit: false,
                        derives_copy,
                        derives_clone,
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
    for item in LangItem::ALL {
        let Some(tr) = resolve.lang_item(item) else {
            continue;
        };
        items.lang.insert(item, tr);
        let Some((_, mutable)) = item.method() else {
            continue;
        };
        // `fn clone(&self) -> Self`, `fn drop(&mut self)`.
        let self_param = resolve.trait_self(tr);
        let method = resolve.trait_members(tr)[0];
        let self_ty = Ty::Param(self_param);
        let (output, receiver) = if mutable {
            (Ty::Unit, Ty::mut_ref(self_ty))
        } else {
            (self_ty.clone(), Ty::shared_ref(self_ty))
        };
        items.fn_sigs.insert(
            method,
            FnSig {
                generics: vec![self_param],
                inputs: vec![receiver],
                output,
                self_kind: Some(SelfKind::Ref { mutable }),
                name_span: Span::dummy(),
                param_spans: vec![Span::dummy()],
            },
        );
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
    collect_traits(file, resolve, lens, &mut items, sink);
    items.sig_cx = ArrayLens {
        lens: HashMap::new(),
        impl_self: lens.impl_self.clone(),
    };
    check_recursive_structs(resolve, &items, sink);
    items.needs_drop = structs_needing_drop(resolve, &items);
    check_unused_params(resolve, &items, sink);
    check_lang_impls(resolve, &items, sink);
    items
}

/// The traits, the trait `impl` blocks and the bounds of each function, then the checks
/// that need them all: each block's type (E0107 for one with references or unsized), its
/// methods' signatures against its trait's (E0053, E0050, E0049, E0185/E0186, E0276), its
/// supertraits (E0277), and that no two blocks of one trait overlap (E0215, E0119).
fn collect_traits(
    file: &SourceFile,
    resolve: &ResolveResults,
    lens: &ArrayLens,
    items: &mut ItemTypes,
    sink: &mut DiagnosticSink,
) {
    for tr in resolve.defs_of_kind(DefKind::Trait) {
        let info = resolve.def(tr);
        items.traits.insert(
            tr,
            TraitDef {
                name: info.name.clone(),
                self_param: resolve.trait_self(tr),
                supertraits: resolve.supertraits(tr).to_vec(),
                span: info.span,
            },
        );
    }
    for item in &file.items {
        let ItemKind::Impl(imp) = &item.kind else {
            continue;
        };
        let Some(def) = resolve.item_def(item.id) else {
            continue;
        };
        let Some(trait_) = resolve.impl_trait(def) else {
            continue;
        };
        let self_ty = lens.impl_self.get(&def).cloned().unwrap_or(Ty::Error);
        if resolve.lang_of(trait_).is_some() && !matches!(self_ty, Ty::Adt(..)) {
            // The resolver reported a language trait's block for another type (E0214).
            continue;
        }
        if !check_impl_type(&self_ty, imp.self_ty.span, resolve, sink) {
            continue;
        }
        items.impls.push(ImplDef {
            def,
            trait_,
            generics: resolve.generics(def).to_vec(),
            self_ty,
            preds: resolve.predicates(def).to_vec(),
            span: resolve.def(def).span,
        });
    }
    let consts = resolve
        .defs_of_kind(DefKind::Const)
        .filter(|&d| crate::consts::is_assoc_const(resolve, d));
    for def in resolve.defs_of_kind(DefKind::Fn).chain(consts) {
        if resolve.builtin(def).is_some() {
            continue;
        }
        let mut env = resolve.predicates(def).to_vec();
        if let Some(parent) = resolve.def(def).parent {
            match resolve.def(parent).kind {
                DefKind::Impl => env.extend_from_slice(resolve.predicates(parent)),
                DefKind::Trait => env.push(Predicate {
                    param: resolve.trait_self(parent),
                    trait_: parent,
                    span: resolve.def(parent).span,
                }),
                _ => {}
            }
        }
        if !env.is_empty() {
            items.param_envs.insert(def, env);
        }
    }
    check_overlap(resolve, items, sink);
    check_supertraits(resolve, items, sink);
    check_impl_methods(resolve, items, sink);
}

/// A trait `impl` block's type may not hold a reference or be unsized, for now (E0107).
fn check_impl_type(
    ty: &Ty,
    span: Span,
    resolve: &ResolveResults,
    sink: &mut DiagnosticSink,
) -> bool {
    let what = if ty.has_ref() {
        "a type that holds a reference"
    } else if matches!(ty, Ty::Prim(PrimTy::Str) | Ty::Slice(_)) {
        "an unsized type"
    } else {
        return !ty.is_error();
    };
    sink.emit(
        Diagnostic::error(
            ErrorCode::UNSUPPORTED_SYNTAX,
            format!("not yet supported: implementing a trait for {what}"),
        )
        .with_label(span, format!("`{}`", ty.display(resolve)))
        .with_note("see docs/PLAN.md"),
    );
    false
}

/// Two `impl` blocks of one trait whose types some type matches both (E0215, rustc's
/// E0119), reported at the later one.
fn check_overlap(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    for (j, later) in items.impls.iter().enumerate() {
        for earlier in &items.impls[..j] {
            if earlier.trait_ != later.trait_ {
                continue;
            }
            let mut vars = earlier.generics.clone();
            vars.extend(later.generics.iter().copied());
            if !crate::traits::unify_patterns(&earlier.self_ty, &later.self_ty, &vars) {
                continue;
            }
            let name = &items.traits[&later.trait_].name;
            sink.emit(
                Diagnostic::error(
                    ErrorCode::CONFLICTING_IMPLS,
                    format!(
                        "conflicting implementations of trait `{name}` for type `{}`",
                        later.self_ty.display(resolve)
                    ),
                )
                .with_label(
                    later.span,
                    format!(
                        "conflicting implementation for `{}`",
                        later.self_ty.display(resolve)
                    ),
                )
                .with_label(earlier.span, "first implementation here"),
            );
            break;
        }
    }
}

/// The type of an `impl` block of a trait with supertraits must implement them (E0341,
/// rustc's E0277), where the block's bounds hold.
fn check_supertraits(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    for imp in &items.impls {
        let name = &items.traits[&imp.trait_].name;
        for &sup in &items.traits[&imp.trait_].supertraits {
            if items.implements(&imp.self_ty, sup, &imp.preds) == crate::traits::Solution::Yes {
                continue;
            }
            let ty = imp.self_ty.display(resolve);
            let sup_name = &resolve.def(sup).name;
            sink.emit(
                Diagnostic::error(
                    ErrorCode::UNSATISFIED_BOUND,
                    format!("the trait bound `{ty}: {sup_name}` is not satisfied"),
                )
                .with_label(
                    imp.span,
                    format!("the trait `{sup_name}` is not implemented for `{ty}`"),
                )
                .with_note(format!(
                    "required by a bound in `{name}`: `trait {name}: {sup_name}`"
                )),
            );
        }
    }
}

/// Each method of a trait `impl` block must have its trait's signature, with `Self` the
/// block's type and the trait method's type parameters the block method's: the same
/// receiver (E0321, rustc's E0185/E0186), parameter count (E0322, E0050), number of type
/// parameters (E0343, E0049), types (E0321, E0053), and no bound on its type parameters
/// the trait's do not imply (E0344, E0276).
fn check_impl_methods(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    for imp in &items.impls {
        let trait_name = items.traits[&imp.trait_].name.clone();
        for (i, info) in resolve.defs.iter().enumerate() {
            if info.parent != Some(imp.def) {
                continue;
            }
            let def = DefId(u32::try_from(i).expect("fewer than 2^32 items"));
            let Some(decl) = resolve.trait_item(imp.trait_, &info.name) else {
                continue;
            };
            match (info.kind, resolve.def(decl).kind) {
                // `check_drop_signature` judged it, with `Drop`'s own messages.
                (DefKind::Fn, DefKind::Fn) if resolve.is_lang_method(decl, LangItem::Drop) => {}
                (DefKind::Fn, DefKind::Fn) => {
                    check_impl_method(resolve, items, imp, &trait_name, decl, def, sink);
                }
                (DefKind::Const, DefKind::Const) => {
                    check_impl_const(resolve, items, imp, &trait_name, decl, def, sink);
                }
                // The resolver reported the mismatch.
                _ => {}
            }
        }
    }
}

/// A trait `impl` block's constant `def` has the type of its trait's `decl`, at the
/// block's type (E0321, rustc's E0326).
fn check_impl_const(
    resolve: &ResolveResults,
    items: &ItemTypes,
    imp: &ImplDef,
    trait_name: &str,
    decl: DefId,
    def: DefId,
    sink: &mut DiagnosticSink,
) {
    let want = items.const_ty(decl).subst(
        &[items.traits[&imp.trait_].self_param],
        std::slice::from_ref(&imp.self_ty),
    );
    let have = items.const_ty(def);
    if want == *have || want.is_error() || have.is_error() {
        return;
    }
    let name = &resolve.def(def).name;
    sink.emit(
        Diagnostic::error(
            ErrorCode::IMPL_SIGNATURE_MISMATCH,
            format!("implemented const `{name}` has an incompatible type for trait"),
        )
        .with_label(
            resolve.def(def).span,
            format!(
                "expected `{}`, found `{}`",
                want.display(resolve),
                have.display(resolve)
            ),
        )
        .with_label(
            resolve.def(decl).span,
            format!("type in trait `{trait_name}`"),
        ),
    );
}

fn check_impl_method(
    resolve: &ResolveResults,
    items: &ItemTypes,
    imp: &ImplDef,
    trait_name: &str,
    decl: DefId,
    def: DefId,
    sink: &mut DiagnosticSink,
) {
    if check_impl_shape(resolve, trait_name, decl, def, items, sink) {
        check_impl_types(resolve, items, imp, decl, def, sink);
    }
}

/// The receiver, parameter count and number of type parameters of a trait block's method
/// `def` against its trait's `decl`; `false` after a mismatch, which was reported.
fn check_impl_shape(
    resolve: &ResolveResults,
    trait_name: &str,
    decl: DefId,
    def: DefId,
    items: &ItemTypes,
    sink: &mut DiagnosticSink,
) -> bool {
    let name = &resolve.def(def).name;
    let (want, have) = (items.fn_sig(decl), items.fn_sig(def));
    match (want.self_kind, have.self_kind) {
        (Some(k), None) => {
            sink.emit(
                Diagnostic::error(
                    ErrorCode::IMPL_SIGNATURE_MISMATCH,
                    format!(
                        "method `{name}` has a `{}` declaration in the trait, but not in the impl",
                        k.as_str()
                    ),
                )
                .with_label(have.name_span, format!("expected `{}` in impl", k.as_str()))
                .with_label(want.name_span, format!("`{}` used in trait", k.as_str())),
            );
            return false;
        }
        (None, Some(k)) => {
            sink.emit(
                Diagnostic::error(
                    ErrorCode::IMPL_SIGNATURE_MISMATCH,
                    format!(
                        "method `{name}` has a `{}` declaration in the impl, but not in the trait",
                        k.as_str()
                    ),
                )
                .with_label(
                    have.param_spans[0],
                    format!("`{}` used in impl", k.as_str()),
                )
                .with_label(want.name_span, "trait method declared without `self`"),
            );
            return false;
        }
        _ => {}
    }
    if want.inputs.len() != have.inputs.len() {
        let (w, h) = (want.inputs.len(), have.inputs.len());
        let plural = |n: usize| if n == 1 { "parameter" } else { "parameters" };
        sink.emit(
            Diagnostic::error(
                ErrorCode::IMPL_PARAM_COUNT,
                format!(
                    "method `{name}` has {h} {} but the declaration in trait `{trait_name}::{name}` has {w}",
                    plural(h)
                ),
            )
            .with_label(have.name_span, format!("expected {w} {}, found {h}", plural(w)))
            .with_label(want.name_span, format!("trait requires {w} {}", plural(w))),
        );
        return false;
    }
    let (w, h) = (
        resolve.own_generics(decl).len(),
        resolve.own_generics(def).len(),
    );
    if w != h {
        let plural = |n: usize| {
            if n == 1 {
                "type parameter"
            } else {
                "type parameters"
            }
        };
        sink.emit(
            Diagnostic::error(
                ErrorCode::IMPL_GENERIC_COUNT,
                format!(
                    "method `{name}` has {h} {} but its trait declaration has {w} {}",
                    plural(h),
                    plural(w)
                ),
            )
            .with_label(have.name_span, format!("found {h} {}", plural(h)))
            .with_label(want.name_span, format!("expected {w} {}", plural(w))),
        );
        return false;
    }
    true
}

/// The parameter and return types of a trait block's method `def` against its trait's
/// `decl` at `Self` the block's type, and its own type parameters' bounds.
fn check_impl_types(
    resolve: &ResolveResults,
    items: &ItemTypes,
    imp: &ImplDef,
    decl: DefId,
    def: DefId,
    sink: &mut DiagnosticSink,
) {
    let name = &resolve.def(def).name;
    let (want, have) = (items.fn_sig(decl), items.fn_sig(def));
    let want_own = resolve.own_generics(decl);
    let have_own = resolve.own_generics(def);
    let mut args = vec![imp.self_ty.clone()];
    args.extend(have_own.iter().map(|&p| Ty::Param(p)));
    let mismatch = |span: Span, label: String, want_span: Span| {
        Diagnostic::error(
            ErrorCode::IMPL_SIGNATURE_MISMATCH,
            format!("method `{name}` has an incompatible type for trait"),
        )
        .with_label(span, label)
        .with_label(want_span, "type in trait")
    };
    for (k, (e, h)) in want.inputs_with(&args).iter().zip(&have.inputs).enumerate() {
        if e != h && !e.is_error() && !h.is_error() {
            let label = format!(
                "expected `{}`, found `{}`",
                e.display(resolve),
                h.display(resolve)
            );
            sink.emit(mismatch(have.param_spans[k], label, want.param_spans[k]));
            return;
        }
    }
    let out = want.output_with(&args);
    if out != have.output && !out.is_error() && !have.output.is_error() {
        let label = format!(
            "expected return type `{}`, found `{}`",
            out.display(resolve),
            have.output.display(resolve)
        );
        sink.emit(mismatch(have.name_span, label, want.name_span));
        return;
    }
    // A bound of the method's own parameters must follow from the trait method's.
    for pred in resolve.predicates(def) {
        let Some(k) = have_own.iter().position(|&p| p == pred.param) else {
            continue;
        };
        let implied = resolve.predicates(decl).iter().any(|w| {
            w.param == want_own[k] && items.trait_closure(w.trait_).contains(&pred.trait_)
        });
        if !implied {
            let bound = &resolve.def(pred.trait_).name;
            let param = &resolve.def(pred.param).name;
            sink.emit(
                Diagnostic::error(
                    ErrorCode::IMPL_STRICTER_BOUNDS,
                    "impl has stricter requirements than trait",
                )
                .with_label(
                    pred.span,
                    format!("impl has extra requirement `{param}: {bound}`"),
                )
                .with_label(want.name_span, "definition of the method in the trait"),
            );
        }
    }
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
            ErrorCode::IMPL_SIGNATURE_MISMATCH,
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
                ErrorCode::IMPL_SIGNATURE_MISMATCH,
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
                ErrorCode::IMPL_PARAM_COUNT,
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

/// The blocks and derives of the language traits (decision 86). A `Copy` type, derived or
/// implemented, needs every field `Copy` (E0417, rustc's E0204) and no `impl Drop`
/// (E0324, E0184): a copy would be dropped twice. A derived `Copy` needs `Clone` (E0341,
/// rustc's E0277), as a block's does (`check_supertraits`). A derive and a block of one
/// trait for one type conflict (E0215, E0119). An `impl Drop` block is for its type's
/// own parameters, in order (E0348, rustc's E0366), with no bounds of its own (E0349,
/// E0367).
fn check_lang_impls(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    let lang = |item| items.lang_item(item);
    for def in adt_defs(resolve) {
        let adt = items.adt(def);
        let own = Ty::Adt(def, adt.generics.iter().map(|&p| Ty::Param(p)).collect());
        let block = |item| {
            lang(item).and_then(|tr| {
                items
                    .impls_of(tr)
                    .find(|i| matches!(&i.self_ty, Ty::Adt(d, _) if *d == def))
            })
        };
        for (item, derived) in [
            (LangItem::Copy, adt.derives_copy),
            (LangItem::Clone, adt.derives_clone),
        ] {
            if let (true, Some(imp)) = (derived, block(item)) {
                let name = item.name();
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::CONFLICTING_IMPLS,
                        format!(
                            "conflicting implementations of trait `{name}` for type `{}`",
                            adt.name
                        ),
                    )
                    .with_label(
                        imp.span,
                        format!("conflicting implementation for `{}`", adt.name),
                    )
                    .with_label(
                        adt.span,
                        format!("first implementation here, by `#[derive({name})]`"),
                    ),
                );
            }
        }
        if !adt.derives_copy {
            continue;
        }
        // The derive requires `T: Copy` of each parameter.
        let env: Vec<Predicate> = lang(LangItem::Copy)
            .map(|copy| {
                adt.generics
                    .iter()
                    .map(|&param| Predicate {
                        param,
                        trait_: copy,
                        span: adt.span,
                    })
                    .collect()
            })
            .unwrap_or_default();
        check_copy_type(
            resolve,
            items,
            def,
            &own,
            &env,
            adt.span,
            "`Copy` derived here",
            sink,
        );
        if let Some(clone) = lang(LangItem::Clone) {
            if items.implements(&own, clone, &env) == crate::traits::Solution::No {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNSATISFIED_BOUND,
                        format!(
                            "the trait bound `{}: Clone` is not satisfied",
                            own.display(resolve)
                        ),
                    )
                    .with_label(adt.span, "`Copy` derived here")
                    .with_note(
                        "`Clone` is a supertrait of `Copy`: derive both, `#[derive(Clone, Copy)]`",
                    ),
                );
            }
        }
    }
    for imp in &items.impls {
        let Ty::Adt(def, args) = &imp.self_ty else {
            continue;
        };
        match items.lang_of(imp.trait_) {
            Some(LangItem::Copy) => check_copy_type(
                resolve,
                items,
                *def,
                &imp.self_ty,
                &imp.preds,
                imp.span,
                "`Copy` implemented here",
                sink,
            ),
            Some(LangItem::Drop) => check_drop_impl(resolve, imp, args, sink),
            _ => {}
        }
    }
}

/// The struct or enum `def` at the type `ty` (its own parameters, or a block's type) is
/// `Copy` where `env` holds: no `impl Drop` (E0324) and only `Copy` fields (E0417).
#[allow(clippy::too_many_arguments)]
fn check_copy_type(
    resolve: &ResolveResults,
    items: &ItemTypes,
    def: DefId,
    ty: &Ty,
    env: &[Predicate],
    span: Span,
    what: &str,
    sink: &mut DiagnosticSink,
) {
    let adt = items.adt(def);
    if items.drop_impl(def).is_some() {
        sink.emit(
            Diagnostic::error(
                ErrorCode::COPY_WITH_DESTRUCTOR,
                "the trait `Copy` cannot be implemented for this type; the type has a destructor",
            )
            .with_label(span, "`Copy` not allowed on types with destructors")
            .with_note(format!(
                "`{}` implements `Drop`; a copy would be dropped twice",
                adt.name
            )),
        );
    }
    let Ty::Adt(_, args) = ty else {
        return;
    };
    let variant_fields = adt.variants().iter().flat_map(|v| v.fields.iter());
    for field in adt.fields.iter().chain(variant_fields) {
        let field_ty = adt.field_ty(&field.ty, args);
        if !items.is_copy_in(&field_ty, env) {
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
                        field_ty.display(resolve)
                    ),
                )
                .with_label(span, what),
            );
        }
    }
}

/// An `impl Drop` block is for its type's own parameters, each once and in order (E0348,
/// rustc's E0366, which allows any order), and has no bounds (E0349, E0367): drop glue
/// runs it for every instance.
fn check_drop_impl(
    resolve: &ResolveResults,
    imp: &ImplDef,
    args: &[Ty],
    sink: &mut DiagnosticSink,
) {
    let own = args.len() == imp.generics.len()
        && args
            .iter()
            .zip(&imp.generics)
            .all(|(a, p)| *a == Ty::Param(*p));
    if !own {
        sink.emit(
            Diagnostic::error(
                ErrorCode::DROP_IMPL_SPECIALIZED,
                "`Drop` impls cannot be specialized",
            )
            .with_label(
                imp.span,
                format!("`{}` is not the type's own parameters", imp.self_ty.display(resolve)),
            )
            .with_note("an `impl Drop` is for every instance: `impl<A, B> Drop for Pair<A, B>`, its parameters in order"),
        );
    } else if let Some(pred) = imp.preds.first() {
        sink.emit(
            Diagnostic::error(
                ErrorCode::DROP_IMPL_BOUNDS,
                "`Drop` impl requires bounds the type does not require",
            )
            .with_label(pred.span, "a bound the struct or enum does not have")
            .with_note("an `impl Drop` is for every instance; bounds on struct and enum parameters are not supported yet"),
        );
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
