//! Trait solving (#116): whether a type implements a trait, and through which `impl`.
//!
//! A trait is implemented by a type when a trait `impl` block's type matches it (its type
//! parameters standing for any type, consistently) and the block's own bounds hold at the
//! types matched, or, for a type parameter, when a bound in scope names the trait or one
//! of its subtraits. Coherence (at most one `impl` per trait and type, checked when the
//! signatures are collected) makes the matching `impl` unique, so selection needs no
//! ranking.

use std::collections::HashMap;

use sabiko_resolve::{DefId, LangItem, Predicate, PrimTy, ResolveResults};

use crate::collect::{ImplDef, ItemTypes};
use crate::ty::{Mutability, Ty};

/// The answer to "does this type implement that trait?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Solution {
    Yes,
    No,
    /// The type still has inference variables that decide it.
    Ambiguous,
}

/// How deep a chain of `impl` bounds may get (`impl<T: Show> Show for W<T>` at `W<W<..>>`)
/// before solving gives up, as rustc's recursion limit does.
const DEPTH_LIMIT: usize = 64;

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
                for &s in &t.supertraits {
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
        out
    }

    /// The trait `impl` of `tr` whose type matches `ty`, with the block's generic arguments
    /// at that match. Bounds of the block are not checked.
    #[must_use]
    pub fn select_impl(&self, tr: DefId, ty: &Ty) -> Option<(&ImplDef, Vec<Ty>)> {
        self.impls_of(tr).find_map(|imp| {
            let mut bound: Vec<Option<Ty>> = vec![None; imp.generics.len()];
            // A block's type parameters are `Sized`, as every type parameter is.
            if match_ty(&imp.self_ty, ty, &imp.generics, &mut bound)
                && !bound.iter().flatten().any(is_unsized)
            {
                let args = bound.into_iter().map(|b| b.unwrap_or(Ty::Error)).collect();
                Some((imp, args))
            } else {
                None
            }
        })
    }

    /// Whether `ty` implements `tr`, where the bounds `env` hold (those of the function
    /// being checked). An error type implements everything, so nothing is reported twice.
    #[must_use]
    pub fn implements(&self, ty: &Ty, tr: DefId, env: &[Predicate]) -> Solution {
        self.implements_at(ty, tr, env, 0)
    }

    fn implements_at(&self, ty: &Ty, tr: DefId, env: &[Predicate], depth: usize) -> Solution {
        if depth > DEPTH_LIMIT || has_error(ty) {
            return Solution::Yes;
        }
        if let Ty::Param(p) = ty {
            let bounded = env
                .iter()
                .any(|pred| pred.param == *p && self.trait_closure(pred.trait_).contains(&tr));
            if bounded {
                return Solution::Yes;
            }
        }
        if ty.has_infer() {
            return Solution::Ambiguous;
        }
        if let Some(solution) = self.builtin_impl(ty, tr, env, depth) {
            return solution;
        }
        let Some((imp, args)) = self.select_impl(tr, ty) else {
            return Solution::No;
        };
        let mut result = Solution::Yes;
        for pred in &imp.preds {
            let arg = Ty::Param(pred.param).subst(&imp.generics, &args);
            match self.implements_at(&arg, pred.trait_, env, depth + 1) {
                Solution::Yes => {}
                Solution::No => return Solution::No,
                Solution::Ambiguous => result = Solution::Ambiguous,
            }
        }
        result
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
                match self.implements_at(t, tr, env, depth + 1) {
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
            Some(copy) => self.implements(ty, copy, env) == Solution::Yes,
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
    /// (`Self` first, then the method's own) runs, with its generic arguments: the
    /// implementing block's method (its block's arguments, then the method's own), or the
    /// trait's provided method itself when the block does not override it. Any other
    /// function, or a `Self` no block matches, is returned as it is.
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
        let Some(self_ty) = args.first() else {
            return unchanged;
        };
        let Some((imp, impl_args)) = self.select_impl(tr, self_ty) else {
            return unchanged;
        };
        match resolve.impl_item(imp.def, &resolve.def(def).name) {
            Some(method) => {
                let mut all = impl_args;
                all.extend(args[1..].iter().cloned());
                (method, all)
            }
            None => unchanged,
        }
    }
}

/// Whether `ty` has no size: `str` or a slice, which only references hold. A type
/// parameter never stands for one (rustc's implicit `Sized` bound).
#[must_use]
pub fn is_unsized(ty: &Ty) -> bool {
    matches!(ty, Ty::Prim(sabiko_resolve::PrimTy::Str) | Ty::Slice(_))
}

fn has_error(ty: &Ty) -> bool {
    match ty {
        Ty::Error => true,
        Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => has_error(inner),
        Ty::Tuple(elems) => elems.iter().any(has_error),
        Ty::Adt(_, args) => args.iter().any(has_error),
        _ => false,
    }
}

/// Whether `ty` is an instance of the pattern `pat`, whose parameters `params` stand for
/// any type (the same one at each occurrence): `bound` collects what each stands for.
fn match_ty(pat: &Ty, ty: &Ty, params: &[DefId], bound: &mut [Option<Ty>]) -> bool {
    if let Ty::Param(p) = pat {
        if let Some(i) = params.iter().position(|q| q == p) {
            if let Some(b) = &bound[i] {
                return b == ty;
            }
            bound[i] = Some(ty.clone());
            return true;
        }
    }
    // No block's type holds a reference or a slice (`check_impl_type`, E0107), so `pat`
    // has no `Ref` or `Slice` part: those arms come back with such blocks.
    match (pat, ty) {
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

/// Whether some type is an instance of both patterns `a` and `b`, whose parameters (`vars`,
/// distinct between them) stand for any types: two trait `impl` blocks of one trait
/// overlap when their types unify (rustc's E0119).
pub(crate) fn unify_patterns(a: &Ty, b: &Ty, vars: &[DefId]) -> bool {
    unify(a, b, vars, &mut HashMap::new())
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

fn unify(left: &Ty, right: &Ty, vars: &[DefId], subst: &mut HashMap<DefId, Ty>) -> bool {
    let (left, right) = (walk(left, vars, subst), walk(right, vars, subst));
    match (&left, &right) {
        (Ty::Param(p), _) if vars.contains(p) => {
            if left != right {
                subst.insert(*p, right.clone());
            }
            true
        }
        (_, Ty::Param(q)) if vars.contains(q) => {
            subst.insert(*q, left.clone());
            true
        }
        // Block types hold no reference or slice (`check_impl_type`, E0107): no `Ref`
        // or `Slice` arm until such blocks are allowed.
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
