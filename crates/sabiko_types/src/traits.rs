//! Trait solving (#116, #117): whether a type implements a trait at some arguments, and
//! through which `impl`.
//!
//! A *trait reference* is a trait with its `Self` type and its own arguments
//! (`u16: From<u8>`, decision 95). It holds when a trait `impl` block's header matches it
//! (the block's type parameters standing for any type, consistently, across the type and
//! the trait's arguments) and the block's own bounds hold at the types matched, or, for a
//! type parameter, when a bound in scope implies it (the bound itself, or a supertrait of
//! it, with the arguments substituted). Coherence (at most one `impl` per trait reference,
//! checked when the signatures are collected) makes the matching `impl` unique, so
//! selection needs no ranking. This is rustc's selection reduced to what Sabiko has:
//! candidate assembly (bounds in scope, `impl` blocks, the built-in `Copy` and `Clone`),
//! then confirmation of the one candidate, whose bounds become new goals.

use std::collections::HashMap;

use sabiko_resolve::{DefId, LangItem, PrimTy, ResolveResults};
use sabiko_span::Span;

use crate::collect::{ImplDef, ItemTypes};
use crate::ty::{Mutability, Ty};

/// A bound on a type parameter, `param: trait_<args>`, with the trait's arguments lowered
/// (`T: From<U>`; empty for a trait without parameters): written after the parameter, in
/// a `where` clause, or implied (a trait's `Self` implements the trait at its own
/// parameters). Defaults are filled in (`T: Add` is `T: Add<T>`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Predicate {
    pub param: DefId,
    pub trait_: DefId,
    pub args: Vec<Ty>,
    /// The trait's path in the bound.
    pub span: Span,
}

/// The answer to "does this type implement that trait?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Solution {
    Yes,
    No,
    /// The type or the arguments still have inference variables that decide it.
    Ambiguous,
}

/// How deep a chain of `impl` bounds may get (`impl<T: Show> Show for W<T>` at `W<W<..>>`)
/// before solving gives up, as rustc's recursion limit does.
const DEPTH_LIMIT: usize = 64;

/// How many trait references the supertraits of one bound may imply before elaboration
/// stops (`trait A<T>: A<(T,)>` implies endlessly many; rustc reports the cycle).
const ELABORATION_LIMIT: usize = 64;

impl ItemTypes {
    /// The trait `tr` and all its supertraits, transitively, each once, `tr` first.
    #[must_use]
    pub fn trait_closure(&self, tr: DefId) -> Vec<DefId> {
        let mut out = vec![tr];
        let mut next = 0;
        while next < out.len() {
            let current = out[next];
            next += 1;
            if let Some(t) = self.trait_def(current) {
                for (s, _) in &t.supertraits {
                    if !out.contains(s) {
                        out.push(*s);
                    }
                }
            }
        }
        out
    }

    /// The number of the trait `tr`'s own parameters, after `Self`.
    #[must_use]
    pub fn trait_param_count(&self, tr: DefId) -> usize {
        self.trait_def(tr).map_or(0, |t| t.params.len())
    }

    /// The trait references `self_ty: tr<args>` implies: itself, then its supertraits with
    /// `Self` and the trait's parameters substituted, transitively, each once
    /// (rustc's elaboration of a where clause).
    #[must_use]
    pub fn elaborate(&self, tr: DefId, self_ty: &Ty, args: &[Ty]) -> Vec<(DefId, Vec<Ty>)> {
        let mut out = vec![(tr, args.to_vec())];
        let mut next = 0;
        while next < out.len() && out.len() < ELABORATION_LIMIT {
            let (current, current_args) = out[next].clone();
            next += 1;
            let Some(t) = self.trait_def(current) else {
                continue;
            };
            let mut params = vec![t.self_param];
            params.extend(&t.params);
            let mut subst = vec![self_ty.clone()];
            subst.extend(current_args);
            subst.resize(params.len(), Ty::Error);
            for (sup, sup_args) in &t.supertraits {
                let implied = (
                    *sup,
                    sup_args.iter().map(|a| a.subst(&params, &subst)).collect(),
                );
                if !out.contains(&implied) {
                    out.push(implied);
                }
            }
        }
        out
    }

    /// The trait `impl` of `tr` whose header matches `ty: tr<args>`, with the block's
    /// generic arguments at that match (rustc's candidate assembly from impls, which
    /// coherence makes unique). Bounds of the block are not checked.
    #[must_use]
    pub fn select_impl(&self, tr: DefId, ty: &Ty, args: &[Ty]) -> Option<(&ImplDef, Vec<Ty>)> {
        self.impls_of(tr).find_map(|imp| {
            let mut bound: Vec<Option<Ty>> = vec![None; imp.generics.len()];
            // A block's type parameters are `Sized`, as every type parameter is.
            let matches = imp.trait_args.len() == args.len()
                && match_ty(&imp.self_ty, ty, &imp.generics, &mut bound)
                && imp
                    .trait_args
                    .iter()
                    .zip(args)
                    .all(|(pat, arg)| match_ty(pat, arg, &imp.generics, &mut bound));
            if matches && !bound.iter().flatten().any(is_unsized) {
                let args = bound.into_iter().map(|b| b.unwrap_or(Ty::Error)).collect();
                Some((imp, args))
            } else {
                None
            }
        })
    }

    /// [`ItemTypes::select_impl`] for a member of the trait `tr` used at the generic
    /// arguments `args`: `Self`, then the trait's own arguments, then the member's.
    #[must_use]
    pub fn select_member_impl(&self, tr: DefId, args: &[Ty]) -> Option<(&ImplDef, Vec<Ty>)> {
        let n = 1 + self.trait_param_count(tr);
        let (self_ty, trait_args) = args.get(..n)?.split_first()?;
        self.select_impl(tr, self_ty, trait_args)
    }

    /// Whether `ty` implements `tr<args>`, where the bounds `env` hold (those of the
    /// function being checked). An error type implements everything, so nothing is
    /// reported twice.
    #[must_use]
    pub fn implements(&self, ty: &Ty, tr: DefId, args: &[Ty], env: &[Predicate]) -> Solution {
        self.implements_at(ty, tr, args, env, 0)
    }

    fn implements_at(
        &self,
        ty: &Ty,
        tr: DefId,
        args: &[Ty],
        env: &[Predicate],
        depth: usize,
    ) -> Solution {
        if depth > DEPTH_LIMIT || ty.references_error() || args.iter().any(Ty::references_error) {
            return Solution::Yes;
        }
        if self.holds_in_env(ty, tr, args, env) {
            return Solution::Yes;
        }
        if ty.has_infer() || args.iter().any(Ty::has_infer) {
            return Solution::Ambiguous;
        }
        if let Some(solution) = self.builtin_impl(ty, tr, env, depth) {
            return solution;
        }
        let Some((imp, impl_args)) = self.select_impl(tr, ty, args) else {
            return Solution::No;
        };
        let mut result = Solution::Yes;
        for pred in &imp.preds {
            let (self_ty, pred_args) = instantiate_pred(pred, &imp.generics, &impl_args);
            match self.implements_at(&self_ty, pred.trait_, &pred_args, env, depth + 1) {
                Solution::Yes => {}
                Solution::No => return Solution::No,
                Solution::Ambiguous => result = Solution::Ambiguous,
            }
        }
        result
    }

    /// Whether a bound in `env` implies `ty: tr<args>` (`ty` a type parameter).
    fn holds_in_env(&self, ty: &Ty, tr: DefId, args: &[Ty], env: &[Predicate]) -> bool {
        let Ty::Param(p) = ty else {
            return false;
        };
        env.iter().filter(|pred| pred.param == *p).any(|pred| {
            self.elaborate(pred.trait_, ty, &pred.args)
                .iter()
                .any(|(t, a)| *t == tr && a.as_slice() == args)
        })
    }

    /// rustc's built-in implementations of `Copy` and `Clone` (decision 86): every
    /// primitive but `str`, `()`, `!`, shared references and function items have both;
    /// `&mut T`, slices and `str` have neither; a tuple or an array has them when its
    /// elements do; a struct or enum that derives one has it when its type arguments do,
    /// as rustc's derive bounds each parameter. `None` for any other trait or type: the
    /// `impl` blocks decide (a struct's or enum's own `impl Copy`).
    fn builtin_impl(
        &self,
        ty: &Ty,
        tr: DefId,
        env: &[Predicate],
        depth: usize,
    ) -> Option<Solution> {
        let lang = self.lang_of(tr)?;
        if !matches!(lang, LangItem::Copy | LangItem::Clone) {
            return None;
        }
        let all = |tys: &mut dyn Iterator<Item = &Ty>| {
            let mut result = Solution::Yes;
            for t in tys {
                match self.implements_at(t, tr, &[], env, depth + 1) {
                    Solution::Yes => {}
                    Solution::No => return Solution::No,
                    Solution::Ambiguous => result = Solution::Ambiguous,
                }
            }
            result
        };
        Some(match ty {
            Ty::Prim(PrimTy::Str) | Ty::Slice(_) | Ty::Ref(Mutability::Mut, _) => Solution::No,
            Ty::Prim(_) | Ty::Unit | Ty::Never | Ty::FnDef(_) | Ty::Ref(..) | Ty::Error => {
                Solution::Yes
            }
            Ty::Tuple(elems) => all(&mut elems.iter()),
            Ty::Array(elem, _) => all(&mut std::iter::once(&**elem)),
            Ty::Adt(def, args) => {
                let adt = self.adt_def(*def)?;
                let derived = match lang {
                    LangItem::Copy => adt.derives_copy,
                    _ => adt.derives_clone,
                };
                if !derived {
                    return None;
                }
                all(&mut args.iter())
            }
            Ty::Param(_) | Ty::Infer(_) => return None,
        })
    }

    /// Whether values of `ty` are copied rather than moved where the bounds `env` hold
    /// (a type parameter with a `Copy` bound is, decision 86).
    #[must_use]
    pub fn is_copy_in(&self, ty: &Ty, env: &[Predicate]) -> bool {
        match self.lang_item(LangItem::Copy) {
            Some(copy) => self.implements(ty, copy, &[], env) == Solution::Yes,
            None => ty.is_copy(),
        }
    }

    /// Whether values of `ty`, which holds no type parameter, are copied rather than
    /// moved.
    #[must_use]
    pub fn is_copy(&self, ty: &Ty) -> bool {
        self.is_copy_in(ty, &[])
    }

    /// The function a call of the trait method `def` at the generic arguments `args`
    /// (`Self` first, then the trait's own arguments, then the method's) runs, with its
    /// generic arguments: the implementing block's method (its block's arguments, then the
    /// method's own), or the trait's provided method itself when the block does not
    /// override it. Any other function, or a trait reference no block matches, is
    /// returned as it is.
    #[must_use]
    pub fn resolve_trait_call(
        &self,
        resolve: &ResolveResults,
        def: DefId,
        args: &[Ty],
    ) -> (DefId, Vec<Ty>) {
        let unchanged = (def, args.to_vec());
        let Some(tr) = resolve
            .def(def)
            .parent
            .filter(|&p| self.trait_def(p).is_some())
        else {
            return unchanged;
        };
        let Some((imp, impl_args)) = self.select_member_impl(tr, args) else {
            return unchanged;
        };
        match resolve.impl_item(imp.def, &resolve.def(def).name) {
            Some(method) => {
                let mut all = impl_args;
                all.extend(args[1 + self.trait_param_count(tr)..].iter().cloned());
                (method, all)
            }
            None => unchanged,
        }
    }
}

/// The trait `tr` at the arguments `args` (after `Self`), as diagnostics show it:
/// `From<u8>`, or `Show` for a trait without parameters.
#[must_use]
pub fn trait_ref_str(resolve: &ResolveResults, tr: DefId, args: &[Ty]) -> String {
    let name = &resolve.def(tr).name;
    if args.is_empty() {
        return name.clone();
    }
    let args: Vec<String> = args.iter().map(|a| a.display(resolve)).collect();
    format!("{name}<{}>", args.join(", "))
}

/// A bound of an `impl` block (or any item with type parameters `params`) at the
/// arguments `args`: its type and its trait's arguments.
#[must_use]
pub fn instantiate_pred(pred: &Predicate, params: &[DefId], args: &[Ty]) -> (Ty, Vec<Ty>) {
    (
        Ty::Param(pred.param).subst(params, args),
        pred.args.iter().map(|a| a.subst(params, args)).collect(),
    )
}

/// Whether `ty` has no size: `str` or a slice, which only references hold. A type
/// parameter never stands for one (rustc's implicit `Sized` bound).
#[must_use]
pub fn is_unsized(ty: &Ty) -> bool {
    matches!(ty, Ty::Prim(sabiko_resolve::PrimTy::Str) | Ty::Slice(_))
}

/// Whether `ty` is an instance of the pattern `pat`, whose parameters `params` stand for
/// any type (the same one at each occurrence): `bound` collects what each stands for.
pub(crate) fn match_ty(pat: &Ty, ty: &Ty, params: &[DefId], bound: &mut [Option<Ty>]) -> bool {
    if let Ty::Param(p) = pat {
        if let Some(i) = params.iter().position(|q| q == p) {
            if let Some(b) = &bound[i] {
                return b == ty;
            }
            bound[i] = Some(ty.clone());
            return true;
        }
    }
    match (pat, ty) {
        // Only the prelude's blocks hold references (`From<&Option<T>> for Option<&T>`,
        // `check_impl_type`); no block's type holds a slice.
        (Ty::Ref(m1, a), Ty::Ref(m2, b)) => m1 == m2 && match_ty(a, b, params, bound),
        (Ty::Array(a, n), Ty::Array(b, m)) => n == m && match_ty(a, b, params, bound),
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| match_ty(x, y, params, bound))
        }
        (Ty::Adt(d1, a), Ty::Adt(d2, b)) => {
            d1 == d2
                && a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| match_ty(x, y, params, bound))
        }
        (a, b) => a == b,
    }
}

/// The substitution under which two lists of patterns are the same types, when one
/// exists: their parameters (`vars`, distinct between them) stand for any types. Two
/// trait `impl` blocks of one trait overlap when their headers unify (rustc's E0119).
pub(crate) fn unify_headers(a: &[Ty], b: &[Ty], vars: &[DefId]) -> Option<HashMap<DefId, Ty>> {
    let mut subst = HashMap::new();
    let ok = a.len() == b.len() && a.iter().zip(b).all(|(l, r)| unify(l, r, vars, &mut subst));
    ok.then_some(subst)
}

/// Whether two patterns, whose parameters are `a_vars` and `b_vars`, can never be the
/// same type because their outermost constructors differ (a cheap test before
/// [`unify_headers`]).
pub(crate) fn heads_differ(a: &Ty, b: &Ty, a_vars: &[DefId], b_vars: &[DefId]) -> bool {
    let var = |t: &Ty, vars: &[DefId]| matches!(t, Ty::Param(p) if vars.contains(p));
    if var(a, a_vars) || var(b, b_vars) {
        return false;
    }
    match (a, b) {
        (Ty::Adt(x, _), Ty::Adt(y, _)) => x != y,
        (Ty::Tuple(x), Ty::Tuple(y)) => x.len() != y.len(),
        (Ty::Array(_, n), Ty::Array(_, m)) => n != m,
        (Ty::Ref(m1, _), Ty::Ref(m2, _)) => m1 != m2,
        (Ty::Slice(_), Ty::Slice(_)) => false,
        (Ty::Adt(..) | Ty::Tuple(_) | Ty::Array(..) | Ty::Ref(..) | Ty::Slice(_), _)
        | (_, Ty::Adt(..) | Ty::Tuple(_) | Ty::Array(..) | Ty::Ref(..) | Ty::Slice(_)) => true,
        _ => a != b,
    }
}

/// `ty` with every variable the substitution binds replaced, transitively.
pub(crate) fn apply(ty: &Ty, vars: &[DefId], subst: &HashMap<DefId, Ty>) -> Ty {
    ty.map_params(&mut |p| {
        subst
            .get(&p)
            .filter(|_| vars.contains(&p))
            .map(|t| apply(t, vars, subst))
    })
}

fn walk(ty: &Ty, vars: &[DefId], s: &HashMap<DefId, Ty>) -> Ty {
    let mut ty = ty.clone();
    while let Ty::Param(p) = &ty {
        match s.get(p) {
            Some(t) if vars.contains(p) => ty = t.clone(),
            _ => break,
        }
    }
    ty
}

/// Whether the variable `var` occurs in `ty` under the substitution: binding it there
/// would make an infinite type (`T = Option<T>`).
fn occurs(var: DefId, ty: &Ty, vars: &[DefId], s: &HashMap<DefId, Ty>) -> bool {
    match walk(ty, vars, s) {
        Ty::Param(p) => p == var,
        Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => occurs(var, &inner, vars, s),
        Ty::Tuple(elems) => elems.iter().any(|e| occurs(var, e, vars, s)),
        Ty::Adt(_, args) => args.iter().any(|a| occurs(var, a, vars, s)),
        _ => false,
    }
}

fn unify(left: &Ty, right: &Ty, vars: &[DefId], subst: &mut HashMap<DefId, Ty>) -> bool {
    let (left, right) = (walk(left, vars, subst), walk(right, vars, subst));
    match (&left, &right) {
        (Ty::Param(p), Ty::Param(q)) if p == q => true,
        (Ty::Param(p), _) if vars.contains(p) => {
            if occurs(*p, &right, vars, subst) {
                return false;
            }
            subst.insert(*p, right.clone());
            true
        }
        (_, Ty::Param(q)) if vars.contains(q) => {
            if occurs(*q, &left, vars, subst) {
                return false;
            }
            subst.insert(*q, left.clone());
            true
        }
        (Ty::Ref(lm, li), Ty::Ref(rm, ri)) => lm == rm && unify(li, ri, vars, subst),
        (Ty::Array(li, ln), Ty::Array(ri, rn)) => ln == rn && unify(li, ri, vars, subst),
        (Ty::Tuple(ls), Ty::Tuple(rs)) => {
            ls.len() == rs.len()
                && ls
                    .iter()
                    .zip(rs.iter())
                    .all(|(l, r)| unify(l, r, vars, subst))
        }
        (Ty::Adt(ld, ls), Ty::Adt(rd, rs)) => {
            ld == rd
                && ls.len() == rs.len()
                && ls
                    .iter()
                    .zip(rs.iter())
                    .all(|(l, r)| unify(l, r, vars, subst))
        }
        _ => left == right,
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use sabiko_diagnostics::DiagnosticSink;
    use sabiko_resolve::{DefKind, ResolveResults};
    use sabiko_span::FileId;

    use super::*;
    use crate::TypeckResults;

    fn run(src: &str) -> (TypeckResults, ResolveResults, Vec<String>) {
        let mut sink = DiagnosticSink::new();
        let mut ast = sabiko_parser::parse_file(FileId(0), src, &mut sink);
        assert!(!sink.has_errors(), "parse errors: {:?}", sink.diagnostics());
        let resolve = sabiko_resolve::resolve(&mut ast, &mut sink);
        let results = crate::typeck(&ast, &resolve, &mut sink);
        let msgs = sink
            .into_vec()
            .into_iter()
            .map(|d| format!("{} {}", d.code, d.message))
            .collect();
        (results, resolve, msgs)
    }

    fn named(resolve: &ResolveResults, kind: DefKind, name: &str) -> DefId {
        resolve
            .defs_of_kind(kind)
            .find(|&d| resolve.def(d).name == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    fn adt(resolve: &ResolveResults, name: &str) -> Ty {
        Ty::Adt(named(resolve, DefKind::Struct, name), Vec::new().into())
    }

    const TWO_IMPLS: &str = "
struct S;
struct W<T> { V: T }
trait Conv<T> { fn Conv(&self) -> T; }
impl Conv<i32> for S { fn Conv(&self) -> i32 { 1 } }
impl Conv<bool> for S { fn Conv(&self) -> bool { true } }
impl<T> Conv<T> for W<T> { fn Conv(&self) -> T { loop {} } }
trait Sup<T> { }
trait Sub<T>: Sup<T> { }
trait Add<Rhs = Self> { fn Add(&self, r: Rhs) -> i32; }
impl Add for S { fn Add(&self, r: S) -> i32 { 1 } }
impl Add<i32> for S { fn Add(&self, r: i32) -> i32 { r } }
";

    /// Selection picks the block by the trait's arguments, binds a blanket block's
    /// parameter across the type and the arguments, and fills in a default argument.
    #[test]
    fn selection_matches_type_and_arguments() {
        let (r, resolve, msgs) = run(TWO_IMPLS);
        assert!(msgs.is_empty(), "{msgs:?}");
        let items = &r.items;
        let conv = named(&resolve, DefKind::Trait, "Conv");
        let s = adt(&resolve, "S");
        let i32_ = Ty::Prim(PrimTy::I32);
        let bool_ = Ty::Prim(PrimTy::Bool);
        let (a, _) = items
            .select_impl(conv, &s, std::slice::from_ref(&i32_))
            .unwrap();
        let (b, _) = items
            .select_impl(conv, &s, std::slice::from_ref(&bool_))
            .unwrap();
        assert_ne!(a.def, b.def);
        assert!(items
            .select_impl(conv, &s, &[Ty::Prim(PrimTy::U8)])
            .is_none());
        assert!(items.select_impl(conv, &s, &[]).is_none());
        let w = Ty::Adt(
            named(&resolve, DefKind::Struct, "W"),
            vec![bool_.clone()].into(),
        );
        let (_, args) = items
            .select_impl(conv, &w, std::slice::from_ref(&bool_))
            .unwrap();
        assert_eq!(args, std::slice::from_ref(&bool_));
        assert!(items
            .select_impl(conv, &w, std::slice::from_ref(&i32_))
            .is_none());
        let no = items.implements(&w, conv, std::slice::from_ref(&i32_), &[]);
        assert_eq!(no, Solution::No);
        assert_eq!(items.implements(&s, conv, &[bool_], &[]), Solution::Yes);
        // `impl Add for S` is `impl Add<S> for S`.
        let add = named(&resolve, DefKind::Trait, "Add");
        let blocks: Vec<&Vec<Ty>> = items.impls_of(add).map(|i| &i.trait_args).collect();
        assert_eq!(blocks, [&vec![s.clone()], &vec![i32_.clone()]]);
        // A member call resolves to the block of its arguments.
        let method = resolve.trait_item(add, "Add").unwrap();
        let (to_s, _) = items.resolve_trait_call(&resolve, method, &[s.clone(), s.clone()]);
        let (to_i, _) = items.resolve_trait_call(&resolve, method, &[s.clone(), i32_]);
        assert_ne!(to_s, to_i);
        assert_ne!(to_s, method);
        // Too few arguments: nothing to select.
        assert_eq!(items.resolve_trait_call(&resolve, method, &[s]).0, method);
    }

    /// A bound implies its supertraits at the arguments substituted, and only those.
    #[test]
    fn bounds_imply_supertraits_at_their_arguments() {
        let (r, resolve, _) = run(TWO_IMPLS);
        let items = &r.items;
        let (sub, sup) = (
            named(&resolve, DefKind::Trait, "Sub"),
            named(&resolve, DefKind::Trait, "Sup"),
        );
        let param = named(&resolve, DefKind::TyParam, "Rhs");
        let t = Ty::Param(param);
        let env = [Predicate {
            param,
            trait_: sub,
            args: vec![Ty::Prim(PrimTy::I32)],
            span: Span::dummy(),
        }];
        let i32_ = [Ty::Prim(PrimTy::I32)];
        assert_eq!(items.implements(&t, sup, &i32_, &env), Solution::Yes);
        assert_eq!(items.implements(&t, sub, &i32_, &env), Solution::Yes);
        let bool_ = [Ty::Prim(PrimTy::Bool)];
        assert_eq!(items.implements(&t, sup, &bool_, &env), Solution::No);
        assert_eq!(
            items.elaborate(sub, &t, &i32_),
            [(sub, i32_.to_vec()), (sup, i32_.to_vec())]
        );
        assert_eq!(items.trait_param_count(sub), 1);
    }

    /// Coherence unifies the whole headers, with an occurs check, and does not count a
    /// block whose bound fails where only this file could make it hold.
    #[test]
    fn coherence_over_headers() {
        let (_, _, msgs) = run("
struct S;
struct W<T> { V: T }
trait Tr<T> { }
impl<T> Tr<T> for T { }
impl<T> Tr<T> for W<T> { }
impl Tr<i32> for S { }
impl Tr<bool> for S { }
trait Show { }
trait Loud { }
impl<T: Show> Loud for T { }
impl Loud for S { }
");
        assert!(msgs.is_empty(), "{msgs:?}");
        let (_, _, msgs) = run("
struct S;
trait Tr<T> { }
impl<T> Tr<T> for T { }
impl Tr<S> for S { }
trait Show { }
trait Loud { }
impl Show for S { }
impl<T: Show> Loud for T { }
impl Loud for S { }
impl<U> Tr<i32> for i64 { }
");
        assert_eq!(
            msgs,
            [
                "E0352 the type parameter `U` is not constrained by the impl trait, self type, or predicates",
                "E0215 conflicting implementations of trait `Tr<S>` for type `S`",
                "E0215 conflicting implementations of trait `Loud` for type `S`",
            ]
        );
    }

    /// Inference: one candidate block decides an integer argument, a bound in scope
    /// decides a trait argument, and two candidates leave it to an annotation.
    #[test]
    fn selection_guides_inference() {
        let (r, resolve, msgs) = run("
struct S;
trait Mk<A> { fn Mk(a: A) -> Self; }
impl Mk<u8> for S { fn Mk(a: u8) -> S { S } }
impl Mk<bool> for S { fn Mk(a: bool) -> S { S } }
fn Lit() -> S { let n = 7; Mk::Mk(n) }
fn Bound<T: Mk<U>, U>(u: U) -> T { let v = u; T::Mk(v) }
");
        assert!(msgs.is_empty(), "{msgs:?}");
        let body = r.body(named(&resolve, DefKind::Fn, "Lit"));
        let n: Vec<String> = body
            .local_types
            .values()
            .map(|t| t.display(&resolve))
            .collect();
        assert_eq!(n, ["u8"]);
        let (_, _, msgs) = run("
struct S;
trait Conv<T> { fn Conv(&self) -> T; }
impl Conv<i32> for S { fn Conv(&self) -> i32 { 1 } }
impl Conv<bool> for S { fn Conv(&self) -> bool { true } }
fn F() -> i32 { let c = S.Conv(); 1 }
fn G() -> i32 { S.Conv() }
");
        assert_eq!(msgs, ["E0313 type annotations needed"]);
    }

    #[test]
    fn heads_and_headers() {
        let (a, b) = (DefId(1000), DefId(1001));
        let pa = Ty::Param(a);
        let i32_ = Ty::Prim(PrimTy::I32);
        let tup = |t: Ty| Ty::Tuple(Box::new(vec![t]));
        assert!(!heads_differ(&pa, &i32_, &[a], &[]));
        assert!(!heads_differ(&i32_, &Ty::Param(b), &[], &[b]));
        assert!(heads_differ(&i32_, &Ty::Prim(PrimTy::U8), &[], &[]));
        assert!(heads_differ(&tup(i32_.clone()), &i32_, &[], &[]));
        assert!(heads_differ(&i32_, &tup(i32_.clone()), &[], &[]));
        assert!(!heads_differ(
            &tup(i32_.clone()),
            &tup(Ty::Prim(PrimTy::U8)),
            &[],
            &[]
        ));
        assert!(heads_differ(
            &Ty::shared_ref(i32_.clone()),
            &Ty::mut_ref(i32_.clone()),
            &[],
            &[]
        ));
        // `T: Tr<T>` against `(U,): Tr<U>`: `T = (U,)` and `T = U` would be infinite.
        let looping = unify_headers(
            &[pa.clone(), pa.clone()],
            &[tup(Ty::Param(b)), Ty::Param(b)],
            &[a, b],
        );
        assert!(looping.is_none());
        let s = unify_headers(
            &[pa.clone(), i32_.clone()],
            &[Ty::Param(b), Ty::Param(b)],
            &[a, b],
        )
        .unwrap();
        assert_eq!(apply(&pa, &[a, b], &s), i32_);
        assert!(unify_headers(std::slice::from_ref(&pa), &[], &[a]).is_none());
        let r = Ty::shared_ref(pa.clone());
        let m = unify_headers(std::slice::from_ref(&r), &[Ty::mut_ref(i32_.clone())], &[a]);
        assert!(m.is_none());
        assert!(unify_headers(&[r], &[Ty::shared_ref(i32_)], &[a]).is_some());
    }
}
