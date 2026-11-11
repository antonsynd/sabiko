//! Monomorphisation (decision 82): every generic function is type-checked, built and
//! borrow-checked once, generically; this pass then makes one copy of its MIR for each
//! list of generic arguments the program calls it with, with the type parameters replaced
//! by the arguments, so drop elaboration and code generation only ever see concrete types.
//!
//! The instances are collected from the roots, every function that is not generic: each
//! call of a generic function there names an instance, and each instance's own calls name
//! more, with their arguments substituted. A generic function no root reaches gets no
//! instance and no code, as in rustc.

use std::collections::{HashMap, HashSet};

use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{DefId, DefKind, Intrinsic, ResolveResults};
use sabiko_span::Span;
use sabiko_types::{ConstValue, ItemTypes, Ty, TypeckResults};

use crate::{
    AggregateKind, Body, CleanupScope, Operand, Place, PlaceElem, Rvalue, StatementKind,
    TerminatorKind,
};

/// How deep a chain of instances may get, each instantiated from the one before at a
/// type that keeps growing (`F::<T>` calling `F::<(T,)>`), before monomorphisation gives
/// up; rustc's `recursion_limit` is 128.
pub const RECURSION_LIMIT: usize = 128;

/// How many type nodes an instance's generic arguments may have in all before
/// monomorphisation gives up (`F::<T>` calling `F::<(T, T)>` doubles them at each step);
/// lower than rustc's `type_length_limit` (about a million), so failing stays fast.
pub const TYPE_LENGTH_LIMIT: usize = 1 << 16;

/// The number of type nodes in `ty`.
fn type_length(ty: &Ty) -> usize {
    1 + match ty {
        Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => type_length(inner),
        Ty::Tuple(elems) => elems.iter().map(type_length).sum(),
        _ => 0,
    }
}

/// The name of an instance's method: the function's name, with the generic arguments
/// (`Id<i32>`, `Swap<&str, (i32, bool)>`) when there are any. A function of a generic
/// type's `impl` block lives in that instance's class, so only its own arguments are
/// named (decision 83). A method of a trait `impl` block, or a trait's provided method at
/// a `Self`, is named after the type and the trait, as rustc's paths name them:
/// `<i32 as Show>::Show` (decision 84); a provided method's generic body is `Show::Name`.
#[must_use]
pub fn instance_name(
    resolve: &ResolveResults,
    items: &ItemTypes,
    def: DefId,
    args: &[Ty],
) -> String {
    instance_name_as(resolve, items, def, args, false)
}

/// [`instance_name`] as generated code names the method: with the `core` prelude's items
/// qualified (`Id<core::Option<i32>>`, decision 90), so a user item of the same name
/// never collides. Diagnostics and the MIR use [`instance_name`].
#[must_use]
pub fn instance_symbol(
    resolve: &ResolveResults,
    items: &ItemTypes,
    def: DefId,
    args: &[Ty],
) -> String {
    instance_name_as(resolve, items, def, args, true)
}

fn instance_name_as(
    resolve: &ResolveResults,
    items: &ItemTypes,
    def: DefId,
    args: &[Ty],
    symbol: bool,
) -> String {
    let ty_name = |t: &Ty| {
        if symbol {
            t.symbol(resolve)
        } else {
            t.display(resolve)
        }
    };
    let def_name = |d: DefId| {
        if symbol {
            resolve.symbol_name(d)
        } else {
            resolve.def(d).name.clone()
        }
    };
    let info = resolve.def(def);
    let parent_count = resolve.parent_generic_count(def).min(args.len());
    let own = &args[parent_count..];
    // A trait at its arguments: `From<u8>` (decision 95), so the blocks of one trait at
    // different arguments for one type never collide.
    let trait_name = |tr: DefId, trait_args: &[Ty]| {
        if trait_args.is_empty() {
            def_name(tr)
        } else {
            let shown: Vec<String> = trait_args.iter().map(ty_name).collect();
            format!("{}<{}>", def_name(tr), shown.join(", "))
        }
    };
    let mut name = match info.parent.map(|p| (p, resolve.def(p).kind)) {
        Some((imp, DefKind::Impl)) => {
            // A generic block's body before monomorphisation names its parameters.
            let block = items.impl_def(imp);
            let subst = |ty: &Ty| {
                if parent_count == resolve.generics(imp).len() {
                    ty.subst(resolve.generics(imp), &args[..parent_count])
                } else {
                    ty.clone()
                }
            };
            let self_ty = subst(&items.impl_self_ty(imp));
            let trait_args: Vec<Ty> = block
                .map(|b| b.trait_args.iter().map(subst).collect())
                .unwrap_or_default();
            // The prelude's `impl<T> [T]` has no trait: `<[i32]>::get`, as rustc names it.
            match resolve.impl_trait(imp) {
                Some(tr) => format!(
                    "<{} as {}>::{}",
                    ty_name(&self_ty),
                    trait_name(tr, &trait_args),
                    info.name
                ),
                None => format!("<{}>::{}", ty_name(&self_ty), info.name),
            }
        }
        Some((tr, DefKind::Trait)) => match args.split_first() {
            Some((self_ty, rest)) => {
                let n = items.trait_param_count(tr).min(rest.len());
                format!(
                    "<{} as {}>::{}",
                    ty_name(self_ty),
                    trait_name(tr, &rest[..n]),
                    info.name
                )
            }
            None => format!("{}::{}", def_name(tr), info.name),
        },
        // A function of a struct's or enum's `impl` block lives in its class, which names
        // the type; a free function of the prelude lives in the module class.
        _ if info.parent.is_some() => info.name.clone(),
        _ => def_name(def),
    };
    if !own.is_empty() {
        let own: Vec<String> = own.iter().map(ty_name).collect();
        name = format!("{name}<{}>", own.join(", "));
    }
    name
}

/// Replaces the generic functions' bodies with their instances. Bodies of functions that
/// are not generic are kept as they are, in their order, followed by the instances in the
/// order they were first reached. An instance chain deeper than [`RECURSION_LIMIT`], or
/// with generic arguments longer than [`TYPE_LENGTH_LIMIT`], is E0502 (rustc's "reached
/// the recursion limit while instantiating"), and the bodies returned are then
/// incomplete.
pub fn monomorphize(
    bodies: Vec<Body>,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) -> Vec<Body> {
    let is_generic = |def: DefId| made_on_demand(def, resolve, typeck);
    let mut generic: HashMap<DefId, Body> = HashMap::new();
    let mut out = Vec::new();
    for mut body in bodies {
        if is_generic(body.def) {
            generic.insert(body.def, body);
        } else {
            resolve_trait_calls(&mut body, resolve, typeck, sink);
            out.push(body);
        }
    }
    // Instances to make, with how deep in a chain of instances each was reached.
    let mut queue: Vec<Pending> = Vec::new();
    let mut seen: HashSet<(DefId, Vec<Ty>)> = HashSet::new();
    // Where each body of `out` was reached from (aligned with `out`).
    let mut origins: Vec<Origin> = Vec::new();
    for body in &out {
        let origin = Origin::root(body.span);
        enqueue(body, 0, origin, resolve, &mut queue, &mut seen);
        origins.push(origin);
    }
    let mut limits = LimitReports::default();
    let mut drop_seen: HashSet<Ty> = HashSet::new();
    let mut scanned = 0;
    let mut next = 0;
    loop {
        while next < queue.len() {
            let Pending {
                def,
                args,
                depth,
                origin,
            } = queue[next].clone();
            next += 1;
            let length: usize = args.iter().map(type_length).sum();
            if depth > RECURSION_LIMIT || length > TYPE_LENGTH_LIMIT {
                let (limit, shown) = if depth > RECURSION_LIMIT {
                    (
                        "recursion",
                        instance_name(resolve, &typeck.items, def, &args),
                    )
                } else {
                    // Too long to print.
                    ("type-length", resolve.def(def).name.clone())
                };
                let diagnostic = Diagnostic::error(
                    ErrorCode::INSTANTIATION_RECURSION_LIMIT,
                    format!("reached the {limit} limit while instantiating `{shown}`"),
                )
                .with_label(origin.user_call, "instantiated here")
                .with_note("each call instantiates the function at a larger type, without end");
                limits.hit(origin.root, resolve.is_prelude(def), diagnostic, sink);
                // The chain stops here; other instances are still made.
                continue;
            }
            // A prelude intrinsic has no instances: code generation lowers its calls
            // (decision 90).
            if resolve.intrinsic(def) == Some(Intrinsic::ArrayIntoTuple) {
                array_into_tuple(origin, &args, resolve, sink);
                continue;
            }
            if resolve.intrinsic(def).is_some() {
                continue;
            }
            // A generic function whose body has errors was not built; nothing calls it
            // then, since the caller's errors stop compilation too.
            let Some(template) = generic.get(&def) else {
                continue;
            };
            let params = &typeck.items.fn_sig(def).generics;
            let mut own = DiagnosticSink::new();
            let instance = instantiate(template, params, &args, resolve, typeck, &mut own);
            for mut d in own.into_vec() {
                // An error found in a prelude body is the user's call's: point at it.
                if resolve.is_prelude(def) {
                    d = d.with_label(origin.user_call, "instantiated here");
                }
                sink.emit(d);
            }
            enqueue(&instance, depth, origin, resolve, &mut queue, &mut seen);
            out.push(instance);
            origins.push(origin);
        }
        // Every instance of a generic type with its own `impl Drop` needs that `drop` at
        // its arguments, which no call names: its drop glue calls it.
        let before = queue.len();
        while scanned < out.len() {
            let body = &out[scanned];
            let origin = origins[scanned];
            scanned += 1;
            let depth = 1;
            for decl in &body.locals {
                drop_instances(&decl.ty, typeck, &mut drop_seen, &mut |drop_fn, args| {
                    if seen.insert((drop_fn, args.clone())) {
                        queue.push(Pending {
                            def: drop_fn,
                            args,
                            depth,
                            origin,
                        });
                    }
                });
            }
        }
        if queue.len() == before {
            limits.finish(sink);
            return out;
        }
    }
}

/// Whether the function `def` is made only where something reaches it: a generic
/// function, or any function of the `core` prelude (whose non-generic functions, such as
/// `impl From<u8> for u16`'s `from`, are made only where a program reaches them, as
/// rustc links only what is used).
fn made_on_demand(def: DefId, resolve: &ResolveResults, typeck: &TypeckResults) -> bool {
    resolve.builtin(def).is_none()
        && (resolve.is_prelude(def) || !typeck.items.fn_sig(def).generics.is_empty())
}

/// E0500 at the program's call that reached `From<[T; N]> for (T, .., T)`, whose body
/// needs array patterns (decision 95).
/// The instance is named, as E0502 names one: the call may be in a generic helper.
fn array_into_tuple(
    origin: Origin,
    args: &[Ty],
    resolve: &ResolveResults,
    sink: &mut DiagnosticSink,
) {
    let mut d = Diagnostic::error(
        ErrorCode::UNSUPPORTED_CODEGEN,
        "converting an array into a tuple is not yet supported by code generation",
    )
    .with_label(origin.user_call, "cannot be compiled yet");
    if let [array, tuple] = args {
        d = d.with_note(format!(
            "instantiated as `<{} as From<{}>>::from`",
            tuple.display(resolve),
            array.display(resolve)
        ));
    }
    sink.emit(d.with_note("`From<[T; N]> for (T, .., T)` needs array patterns; see docs/PLAN.md"));
}

/// An instance to make.
#[derive(Clone)]
struct Pending {
    def: DefId,
    args: Vec<Ty>,
    depth: usize,
    origin: Origin,
}

/// Where a chain of instances started, and the call in the user's program nearest to an
/// instance along it: an error found while instantiating a body of the `core` prelude
/// (decision 90) points there, never only into the prelude's source.
#[derive(Clone, Copy)]
struct Origin {
    /// The first call of the chain, made from a function that is not generic.
    root: Span,
    /// The last call on the way made from a body of the user's program.
    user_call: Span,
}

impl Origin {
    fn root(span: Span) -> Self {
        Origin {
            root: span,
            user_call: span,
        }
    }
}

/// The instantiation limit errors (E0502), one per chain, as rustc stops at the first. A
/// chain through the prelude's helpers reaches the limit at a helper (`Option::is_none`)
/// in the same step as at the user's function that keeps growing (`Grow`), which rustc,
/// counting the depth per function, names; so a hit at a prelude function waits for the
/// end, and is reported only if no function of the user's program hit the limit in that
/// chain.
#[derive(Default)]
struct LimitReports {
    reported: HashSet<Span>,
    waiting: Vec<(Span, Diagnostic)>,
}

impl LimitReports {
    fn hit(&mut self, root: Span, in_prelude: bool, d: Diagnostic, sink: &mut DiagnosticSink) {
        if self.reported.contains(&root) {
            return;
        }
        if in_prelude {
            if !self.waiting.iter().any(|(r, _)| *r == root) {
                self.waiting.push((root, d));
            }
        } else {
            self.reported.insert(root);
            sink.emit(d);
        }
    }

    fn finish(&mut self, sink: &mut DiagnosticSink) {
        for (root, d) in std::mem::take(&mut self.waiting) {
            if self.reported.insert(root) {
                sink.emit(d);
            }
        }
    }
}

/// Points each call of a trait's method in `body`, whose generic arguments are concrete,
/// at the method that runs: the implementing block's, or the trait's provided one
/// (decision 84). A call of a block's method that is not generic then names no instance.
fn resolve_trait_calls(
    body: &mut Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    for data in &mut body.blocks {
        let term = data.terminator.as_mut().expect("block has a terminator");
        if let TerminatorKind::Call {
            func, generic_args, ..
        } = &mut term.kind
        {
            if generic_args.is_empty() || generic_args.iter().any(Ty::has_param) {
                continue;
            }
            let (f, args) = typeck
                .items
                .resolve_trait_call(resolve, *func, generic_args);
            *func = f;
            *generic_args = args;
        }
        if let Some(copy) = builtin_clone(term, resolve, typeck, sink) {
            data.statements.push(copy.0);
            term.kind = TerminatorKind::Goto { target: copy.1 };
        }
    }
}

/// `Clone::clone(r)` at a `Self` that no `impl Clone` block implements (decision 86):
/// rustc's built-in or derived `clone`, which for a `Copy` type is a copy of `*r`, the
/// statement returned with the block to go to. A derived or tuple's `clone` that has to
/// clone fields of a type that is not `Copy` needs a generated body (#117): E0500.
fn builtin_clone(
    term: &crate::Terminator,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) -> Option<(crate::Statement, crate::BasicBlock)> {
    let TerminatorKind::Call {
        func,
        generic_args,
        args,
        destination,
        target,
        ..
    } = &term.kind
    else {
        return None;
    };
    if !resolve.is_lang_method(*func, sabiko_resolve::LangItem::Clone) {
        return None;
    }
    let self_ty = generic_args.first()?;
    if self_ty.has_param() {
        return None;
    }
    if !typeck.items.is_copy(self_ty) {
        sink.emit(
            Diagnostic::error(
                ErrorCode::UNSUPPORTED_CODEGEN,
                format!(
                    "not yet supported: the built-in or derived `clone` of `{}`, which is not `Copy`",
                    self_ty.display(resolve)
                ),
            )
            .with_label(term.span, "cloned here")
            .with_note("cloning a derived type or a tuple or array field by field arrives with derives (#117); implement `Clone` by hand for now"),
        );
        return None;
    }
    let (Some(target), Some(Operand::Copy(r) | Operand::Move(r))) = (target, args.first()) else {
        return None;
    };
    let copy = crate::Statement {
        kind: StatementKind::Assign(
            destination.clone(),
            Rvalue::Use(Operand::Copy(r.clone().deref())),
        ),
        span: term.span,
    };
    Some((copy, *target))
}

/// Queues the instances of generic functions `body` calls that are not made yet, `body`
/// being reached along `origin` (a chain starts at each call of a body at `depth` 0).
fn enqueue(
    body: &Body,
    depth: usize,
    origin: Origin,
    resolve: &ResolveResults,
    queue: &mut Vec<Pending>,
    seen: &mut HashSet<(DefId, Vec<Ty>)>,
) {
    let user_body = !resolve.is_prelude(body.def);
    for (def, args, span) in generic_calls(body, resolve) {
        if seen.insert((def, args.clone())) {
            queue.push(Pending {
                def,
                args,
                depth: depth + 1,
                origin: Origin {
                    root: if depth == 0 { span } else { origin.root },
                    user_call: if user_body { span } else { origin.user_call },
                },
            });
        }
    }
}

/// Calls `f` with the `drop` function and generic arguments of every instance of a
/// generic type with its own `impl Drop` that `ty` holds by value, through fields too.
fn drop_instances(
    ty: &Ty,
    typeck: &TypeckResults,
    seen: &mut HashSet<Ty>,
    f: &mut impl FnMut(DefId, Vec<Ty>),
) {
    match ty {
        Ty::Tuple(elems) => elems
            .iter()
            .for_each(|e| drop_instances(e, typeck, seen, f)),
        Ty::Array(elem, _) => drop_instances(elem, typeck, seen, f),
        Ty::Adt(def, args) if !args.is_empty() => {
            if !seen.insert(ty.clone()) {
                return;
            }
            if let Some(drop_fn) = typeck.items.drop_impl(*def) {
                f(drop_fn, args.to_vec());
            }
            let adt = typeck.items.adt(*def);
            for field in adt.all_field_tys() {
                drop_instances(&adt.field_ty(field, args), typeck, seen, f);
            }
        }
        _ => {}
    }
}

/// The generic functions and the functions of the `core` prelude `body` calls, with their
/// (concrete) arguments and the call's span.
fn generic_calls(body: &Body, resolve: &ResolveResults) -> Vec<(DefId, Vec<Ty>, Span)> {
    let mut calls = Vec::new();
    for data in &body.blocks {
        let term = data.terminator();
        if let TerminatorKind::Call {
            func, generic_args, ..
        } = &term.kind
        {
            if !generic_args.is_empty() || resolve.is_prelude(*func) {
                calls.push((*func, generic_args.clone(), term.span));
            }
        }
    }
    calls
}

/// A copy of the generic `template` with `params` replaced by `args` everywhere. A drop of
/// a place whose type turns out to have no drop glue (`T` instantiated as `i32`) becomes a
/// jump to its target.
fn instantiate(
    template: &Body,
    params: &[DefId],
    args: &[Ty],
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) -> Body {
    let mut body = template.clone();
    let s = |ty: &mut Ty| *ty = ty.subst(params, args);
    for decl in &mut body.locals {
        s(&mut decl.ty);
    }
    for data in &mut body.blocks {
        for stmt in &mut data.statements {
            match &mut stmt.kind {
                StatementKind::Assign(place, rvalue) => {
                    subst_place(place, &s);
                    subst_rvalue(rvalue, &s);
                }
                StatementKind::PlaceMention(place) => subst_place(place, &s),
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let term = data.terminator.as_mut().expect("block has a terminator");
        match &mut term.kind {
            TerminatorKind::SwitchInt { discr, .. } => subst_operand(discr, &s),
            TerminatorKind::Call {
                generic_args,
                args: call_args,
                destination,
                ..
            } => {
                for t in generic_args.iter_mut() {
                    s(t);
                }
                for a in call_args {
                    subst_operand(a, &s);
                }
                subst_place(destination, &s);
            }
            TerminatorKind::Drop { place, .. } => subst_place(place, &s),
            TerminatorKind::Goto { .. }
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::FalseEdge { .. } => {}
        }
    }
    for cleanup in &mut body.cleanups {
        for drop in &mut cleanup.drops {
            for place in drop.places_mut() {
                subst_place(place, &s);
            }
        }
    }
    // Drops of places without drop glue go: elaboration and code generation only expect
    // drops of types that have it.
    let glueless: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, data)| {
            matches!(&data.terminator().kind,
                TerminatorKind::Drop { place, .. } if !typeck.items.needs_drop(&place.ty(&body)))
        })
        .map(|(i, _)| i)
        .collect();
    for i in glueless {
        let term = body.blocks[i]
            .terminator
            .as_mut()
            .expect("block has a terminator");
        if let TerminatorKind::Drop { target, .. } = term.kind {
            term.kind = TerminatorKind::Goto { target };
        }
    }
    prune_cleanups(&mut body, typeck);
    copy_copy_operands(&mut body, typeck);
    resolve_trait_calls(&mut body, resolve, typeck, sink);
    eval_assoc_consts(&mut body, resolve, typeck, true, sink);
    body.name = instance_name(resolve, &typeck.items, template.def, args);
    body.instance = args.to_vec();
    body
}

/// Removes the cleanup scopes of locals whose type turns out to have no drop glue: their
/// blocks belong to the nearest enclosing scope that is kept, and so do their children.
fn prune_cleanups(body: &mut Body, typeck: &TypeckResults) {
    let keep: Vec<bool> = body
        .cleanups
        .iter()
        .map(|c| typeck.items.needs_drop(&body.locals[c.local.index()].ty))
        .collect();
    if keep.iter().all(|&k| k) {
        return;
    }
    // The nearest kept scope at or above each scope (parents precede children).
    let mut kept_at: Vec<Option<CleanupScope>> = Vec::with_capacity(keep.len());
    for (i, c) in body.cleanups.iter().enumerate() {
        let above = c.parent.and_then(|p| kept_at[p.index()]);
        kept_at.push(if keep[i] {
            Some(CleanupScope(
                u32::try_from(i).expect("fewer than 2^32 scopes"),
            ))
        } else {
            above
        });
    }
    // New indices of the kept scopes, in order.
    let mut renumber = vec![None; keep.len()];
    let mut next = 0u32;
    for (i, &k) in keep.iter().enumerate() {
        if k {
            renumber[i] = Some(CleanupScope(next));
            next += 1;
        }
    }
    let map = |scope: Option<CleanupScope>| {
        scope
            .and_then(|s| kept_at[s.index()])
            .and_then(|s| renumber[s.index()])
    };
    let old = std::mem::take(&mut body.cleanups);
    for (i, mut c) in old.into_iter().enumerate() {
        if keep[i] {
            c.parent = map(c.parent);
            body.cleanups.push(c);
        }
    }
    for data in &mut body.blocks {
        data.cleanup = map(data.cleanup);
    }
}

/// Turns each move of a value whose type turns out to be `Copy` into a copy, as MIR
/// building would have made it.
fn copy_copy_operands(body: &mut Body, typeck: &TypeckResults) {
    let locals = body.locals.clone();
    let fix = |op: &mut Operand| {
        if let Operand::Move(place) = op {
            if typeck.items.is_copy(&place_ty(place, &locals)) {
                *op = Operand::Copy(place.clone());
            }
        }
    };
    for data in &mut body.blocks {
        for stmt in &mut data.statements {
            if let StatementKind::Assign(_, rvalue) = &mut stmt.kind {
                rvalue_operands_mut(rvalue, &fix);
            }
        }
        let term = data.terminator.as_mut().expect("block has a terminator");
        match &mut term.kind {
            TerminatorKind::SwitchInt { discr, .. } => fix(discr),
            TerminatorKind::Call { args, .. } => args.iter_mut().for_each(&fix),
            _ => {}
        }
    }
}

fn rvalue_operands_mut(rvalue: &mut Rvalue, mut f: impl FnMut(&mut Operand)) {
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp(_, op)
        | Rvalue::Repeat(op, _)
        | Rvalue::Cast(op, _)
        | Rvalue::PointerCoercion(op, _) => f(op),
        Rvalue::BinaryOp(_, a, b) => {
            f(a);
            f(b);
        }
        Rvalue::Aggregate(_, ops) => ops.iter_mut().for_each(f),
        Rvalue::Subslice(op, bounds) => {
            f(op);
            bounds.operands_mut().for_each(f);
        }
        Rvalue::Ref(..) | Rvalue::Discriminant(_) | Rvalue::Len(_) => {}
    }
}

/// The type of `place` given the body's locals.
fn place_ty(place: &Place, locals: &[crate::LocalDecl]) -> Ty {
    let mut ty = locals[place.local.index()].ty.clone();
    for elem in &place.projection {
        ty = match (elem, ty) {
            (PlaceElem::Deref, Ty::Ref(_, inner)) => *inner,
            (PlaceElem::Field(_, field_ty), _) => field_ty.clone(),
            (PlaceElem::Index(_), Ty::Array(elem, _) | Ty::Slice(elem)) => *elem,
            (_, other) => other,
        };
    }
    ty
}

fn subst_place(place: &mut Place, s: &impl Fn(&mut Ty)) {
    for elem in &mut place.projection {
        if let PlaceElem::Field(_, ty) = elem {
            s(ty);
        }
    }
}

fn subst_operand(op: &mut Operand, s: &impl Fn(&mut Ty)) {
    match op {
        Operand::Copy(place) | Operand::Move(place) => subst_place(place, s),
        Operand::Constant(c) => {
            s(&mut c.ty);
            if let ConstValue::Unevaluated(_, args) = &mut c.value {
                args.iter_mut().for_each(s);
            }
        }
    }
}

/// Evaluates each trait constant `body` names at generic arguments without a type
/// parameter (#116): every one of an instance, or of a function that is not generic. A
/// failure is reported (once per constant and type) and leaves the constant unevaluated;
/// in an instance (`instance`), as E0503, since only code generation finds it.
pub fn eval_assoc_consts(
    body: &mut Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    instance: bool,
    sink: &mut DiagnosticSink,
) {
    let mut own = DiagnosticSink::new();
    eval_body_consts(body, resolve, typeck, &mut own);
    for mut d in own.into_vec() {
        if instance && d.code == ErrorCode::CONST_EVAL_FAILED {
            d.code = ErrorCode::INSTANCE_CONST_EVAL_FAILED;
        }
        sink.emit(d);
    }
}

fn eval_body_consts(
    body: &mut Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    let mut eval = |op: &mut Operand| {
        let Operand::Constant(c) = op else {
            return;
        };
        let ConstValue::Unevaluated(def, args) = &c.value else {
            return;
        };
        if args.iter().any(|t| t.has_param() || t.is_error()) {
            return;
        }
        if let Some(v) = typeck.eval_assoc_const(resolve, *def, args.clone(), sink) {
            c.value = v;
        }
    };
    for data in &mut body.blocks {
        for stmt in &mut data.statements {
            if let StatementKind::Assign(_, rvalue) = &mut stmt.kind {
                rvalue_operands_mut(rvalue, &mut eval);
            }
        }
        let term = data.terminator.as_mut().expect("block has a terminator");
        match &mut term.kind {
            TerminatorKind::SwitchInt { discr, .. } => eval(discr),
            TerminatorKind::Call { args, .. } => args.iter_mut().for_each(&mut eval),
            _ => {}
        }
    }
}

fn subst_rvalue(rvalue: &mut Rvalue, s: &impl Fn(&mut Ty)) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Repeat(op, _) => {
            subst_operand(op, s);
        }
        Rvalue::Ref(_, place) | Rvalue::Discriminant(place) | Rvalue::Len(place) => {
            subst_place(place, s);
        }
        Rvalue::BinaryOp(_, a, b) => {
            subst_operand(a, s);
            subst_operand(b, s);
        }
        Rvalue::Cast(op, ty) | Rvalue::PointerCoercion(op, ty) => {
            subst_operand(op, s);
            s(ty);
        }
        Rvalue::Aggregate(kind, ops) => {
            match kind {
                AggregateKind::Array(ty) => s(ty),
                AggregateKind::Adt(_, args) | AggregateKind::Variant(_, _, args) => {
                    args.iter_mut().for_each(s);
                }
                AggregateKind::Tuple => {}
            }
            for op in ops {
                subst_operand(op, s);
            }
        }
        Rvalue::Subslice(op, bounds) => {
            subst_operand(op, s);
            for b in bounds.operands_mut() {
                subst_operand(b, s);
            }
        }
    }
}
