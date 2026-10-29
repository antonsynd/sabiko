//! Collects item signatures: function types, struct shapes and constant types. Runs
//! before any body is checked so bodies may refer to items in any order.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{ItemKind, SelfKind, SourceFile, StructKind, Type, TypeKind};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{Builtin, DefId, DefKind, PrimTy, Res, ResolveResults};
use sabiko_span::Span;

use crate::ty::{Mutability, Ty};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnSig {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdtDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// `struct S;`
    pub is_unit: bool,
    /// `#[derive(Copy)]`: values are copied, not moved.
    pub is_copy: bool,
    pub span: Span,
}

impl AdtDef {
    #[must_use]
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
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

    #[must_use]
    pub fn const_ty(&self, def: DefId) -> &Ty {
        &self.const_tys[&def]
    }

    /// The `drop` function of the struct `adt`'s `impl Drop`, if it has one.
    #[must_use]
    pub fn drop_impl(&self, adt: DefId) -> Option<DefId> {
        self.drop_impls.get(&adt).copied()
    }

    /// Whether a value of `ty` has drop glue: a struct with an `impl Drop`, or a struct or
    /// tuple with a field (element) that has drop glue. References, primitives, `()` and
    /// `Copy` types never do.
    #[must_use]
    pub fn needs_drop(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Adt(def) => self.needs_drop.contains(def),
            Ty::Tuple(elems) => elems.iter().any(|e| self.needs_drop(e)),
            _ => false,
        }
    }

    /// Whether values of `ty` are copied rather than moved. Like [`Ty::is_copy`] but
    /// aware of `#[derive(Copy)]` on structs.
    #[must_use]
    pub fn is_copy(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Adt(def) => self.adts.get(def).is_some_and(|a| a.is_copy),
            Ty::Tuple(elems) => elems.iter().all(|e| self.is_copy(e)),
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

/// Lowers a syntactic type to a semantic one. `Infer` becomes `Error` here: this is only
/// used for signatures, where the resolver already rejected `_`.
pub(crate) fn lower_sig_type(ty: &Type, resolve: &ResolveResults) -> Ty {
    match &ty.kind {
        TypeKind::Path(_) => match resolve.res(ty.id) {
            Res::Prim(p) => Ty::Prim(p),
            Res::Def(d) => Ty::Adt(d),
            Res::Local(_) => unreachable!("locals never resolve in type position"),
            Res::Err => Ty::Error,
        },
        TypeKind::Ref { mutable, inner } => Ty::Ref(
            Mutability::from_bool(*mutable),
            Box::new(lower_sig_type(inner, resolve)),
        ),
        TypeKind::Unit => Ty::Unit,
        TypeKind::Tuple(elems) => Ty::Tuple(
            elems
                .iter()
                .map(|t| lower_sig_type(t, resolve))
                .collect::<Vec<_>>()
                .into(),
        ),
        TypeKind::Infer => Ty::Error,
    }
}

/// `str` is only valid behind a reference, not by value nor as a tuple element. Reports
/// and returns `false` otherwise.
pub(crate) fn check_sized(ty: &Ty, span: Span, sink: &mut DiagnosticSink) -> bool {
    if let Ty::Tuple(elems) = ty {
        return elems.iter().all(|e| check_sized(e, span, sink));
    }
    if *ty == Ty::Prim(PrimTy::Str) {
        sink.emit(
            Diagnostic::error(
                ErrorCode::UNSIZED_TYPE,
                "the size for values of type `str` cannot be known at compilation time",
            )
            .with_label(span, "doesn't have a size known at compile-time")
            .with_note("string values are used behind a reference: `&str`"),
        );
        return false;
    }
    true
}

/// The signature of a function; a method's receiver type is its first input.
fn fn_sig(
    def: DefId,
    f: &sabiko_ast::FnDecl,
    resolve: &ResolveResults,
    sink: &mut DiagnosticSink,
) -> FnSig {
    let mut inputs = Vec::new();
    let mut param_spans = Vec::new();
    if let Some(sp) = &f.self_param {
        // `Self` is the struct of the `impl` block; an error type when that failed to
        // resolve (reported with the block).
        let self_ty = resolve.def(def).parent.map_or(Ty::Error, Ty::Adt);
        inputs.push(match sp.kind {
            SelfKind::Value { .. } => self_ty,
            SelfKind::Ref { mutable } => Ty::Ref(Mutability::from_bool(mutable), Box::new(self_ty)),
        });
        param_spans.push(sp.span);
    }
    for p in &f.params {
        let ty = lower_sig_type(&p.ty, resolve);
        check_sized(&ty, p.ty.span, sink);
        inputs.push(ty);
        param_spans.push(p.span);
    }
    let output = match &f.ret {
        Some(t) => {
            let ty = lower_sig_type(t, resolve);
            check_sized(&ty, t.span, sink);
            ty
        }
        None => Ty::Unit,
    };
    FnSig {
        inputs,
        output,
        self_kind: f.self_param.as_ref().map(|sp| sp.kind),
        name_span: f.name.span,
        param_spans,
    }
}

pub(crate) fn collect(
    file: &SourceFile,
    resolve: &ResolveResults,
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
                items.fn_sigs.insert(def, fn_sig(def, f, resolve, sink));
            }
            ItemKind::Struct(s) => {
                let (fields, is_unit) = match &s.kind {
                    StructKind::Named(fields) => {
                        let mut out = Vec::new();
                        let mut seen = HashSet::new();
                        for f in fields {
                            // The resolver reported duplicates; keep the first.
                            if !seen.insert(f.name.name.as_str()) {
                                continue;
                            }
                            let ty = lower_sig_type(&f.ty, resolve);
                            check_sized(&ty, f.ty.span, sink);
                            check_no_reference(&ty, f.ty.span, sink);
                            out.push(FieldDef {
                                name: f.name.name.clone(),
                                ty,
                                span: f.span,
                            });
                        }
                        (out, false)
                    }
                    StructKind::Unit => (Vec::new(), true),
                };
                let is_copy = derives_copy(item, sink);
                items.adts.insert(
                    def,
                    AdtDef {
                        name: s.name.name.clone(),
                        fields,
                        is_unit,
                        is_copy,
                        span: s.name.span,
                    },
                );
            }
            ItemKind::Const(c) => {
                let ty = lower_sig_type(&c.ty, resolve);
                check_sized(&ty, c.ty.span, sink);
                items.const_tys.insert(def, ty);
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
    for adt in resolve.defs_of_kind(DefKind::Struct) {
        if let Some(def) = resolve.drop_impl(adt) {
            items.drop_impls.insert(adt, def);
            if let Some(f) = file.all_items().find_map(|item| match &item.kind {
                ItemKind::Fn(f) if resolve.def_of_item(item.id) == def => Some(f),
                _ => None,
            }) {
                check_drop_signature(resolve, adt, f, sink);
            }
        }
    }
    check_recursive_structs(resolve, &items, sink);
    items.needs_drop = structs_needing_drop(resolve, &items);
    check_copy_derives(resolve, &items, sink);
    items
}

/// `drop` must be declared exactly `fn drop(&mut self)`: E0321 for another receiver or a
/// return type (rustc's E0053, E0186), E0322 for parameters after `&mut self` (E0050).
fn check_drop_signature(
    resolve: &ResolveResults,
    adt: DefId,
    f: &sabiko_ast::FnDecl,
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
        let ty = lower_sig_type(ret, resolve);
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

/// The structs whose values have drop glue (see [`ItemTypes::needs_drop`]), to a fixpoint:
/// a struct needs drop when it has an `impl Drop` or a field type that needs drop.
fn structs_needing_drop(resolve: &ResolveResults, items: &ItemTypes) -> HashSet<DefId> {
    let mut set: HashSet<DefId> = items.drop_impls.keys().copied().collect();
    let structs: Vec<DefId> = resolve.defs_of_kind(DefKind::Struct).collect();
    loop {
        let before = set.len();
        let probe = ItemTypes {
            needs_drop: set.clone(),
            ..ItemTypes::default()
        };
        for &def in &structs {
            if items
                .adt(def)
                .fields
                .iter()
                .any(|f| probe.needs_drop(&f.ty))
            {
                set.insert(def);
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

/// `#[derive(Copy)]` requires every field to be `Copy` (E0417) and no `impl Drop` (E0324):
/// a copy would be dropped twice.
fn check_copy_derives(resolve: &ResolveResults, items: &ItemTypes, sink: &mut DiagnosticSink) {
    for def in resolve.defs_of_kind(DefKind::Struct) {
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
        for field in &adt.fields {
            if !items.is_copy(&field.ty) {
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
    for def in resolve.defs_of_kind(DefKind::Struct) {
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
                "a struct cannot contain itself by value; indirection (`Box<T>`) arrives in Phase 5",
            ));
        }
    }
}

/// The structs `ty` holds by value: itself, or the elements of a tuple, recursively.
fn structs_by_value(ty: &Ty, out: &mut Vec<DefId>) {
    match ty {
        Ty::Adt(def) => out.push(*def),
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
    for field in &items.adt(current).fields {
        let mut contained = Vec::new();
        structs_by_value(&field.ty, &mut contained);
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
