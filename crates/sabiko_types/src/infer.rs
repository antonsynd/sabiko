//! Unification-based inference over [`Ty`].
//!
//! A small union-find: each variable has a parent link and, at the root, an optional
//! binding plus a kind. Integer and float literal variables (`{integer}`, `{float}`) only
//! unify with the matching primitive family and default to `i32`/`f64` when nothing
//! constrains them, as in Rust.

use sabiko_resolve::PrimTy;

use crate::ty::{Ty, TyVid};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarKind {
    General,
    Int,
    Float,
}

#[derive(Debug)]
struct VarData {
    parent: TyVid,
    kind: VarKind,
    value: Option<Ty>,
}

#[derive(Debug, Default)]
pub struct InferCtxt {
    vars: Vec<VarData>,
}

/// Why two types did not unify.
#[derive(Debug, PartialEq, Eq)]
pub struct UnifyError;

impl InferCtxt {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_var(&mut self, kind: VarKind) -> Ty {
        let id = TyVid(u32::try_from(self.vars.len()).expect("too many inference variables"));
        self.vars.push(VarData {
            parent: id,
            kind,
            value: None,
        });
        Ty::Infer(id)
    }

    fn root(&mut self, v: TyVid) -> TyVid {
        let parent = self.vars[v.0 as usize].parent;
        if parent == v {
            return v;
        }
        let r = self.root(parent);
        self.vars[v.0 as usize].parent = r;
        r
    }

    #[must_use]
    pub fn kind(&mut self, v: TyVid) -> VarKind {
        let r = self.root(v);
        self.vars[r.0 as usize].kind
    }

    /// Follows variable bindings at the top level only.
    #[must_use]
    pub fn shallow_resolve(&mut self, ty: &Ty) -> Ty {
        let mut ty = ty.clone();
        while let Ty::Infer(v) = ty {
            let r = self.root(v);
            match &self.vars[r.0 as usize].value {
                Some(t) => ty = t.clone(),
                None => return Ty::Infer(r),
            }
        }
        ty
    }

    /// Replaces every bound variable in `ty` with its value. Unbound variables stay.
    #[must_use]
    pub fn resolve(&mut self, ty: &Ty) -> Ty {
        match self.shallow_resolve(ty) {
            Ty::Ref(m, inner) => Ty::Ref(m, Box::new(self.resolve(&inner))),
            t => t,
        }
    }

    /// Like [`resolve`](Self::resolve) but applies the literal defaults (`i32`, `f64`) to
    /// unbound literal variables. General variables stay unbound.
    #[must_use]
    pub fn resolve_with_defaults(&mut self, ty: &Ty) -> Ty {
        match self.resolve(ty) {
            Ty::Infer(v) => match self.kind(v) {
                VarKind::Int => Ty::I32,
                VarKind::Float => Ty::F64,
                VarKind::General => Ty::Infer(v),
            },
            Ty::Ref(m, inner) => Ty::Ref(m, Box::new(self.resolve_with_defaults(&inner))),
            t => t,
        }
    }

    fn bind(&mut self, v: TyVid, ty: Ty) {
        let r = self.root(v);
        debug_assert!(self.vars[r.0 as usize].value.is_none());
        self.vars[r.0 as usize].value = Some(ty);
    }

    /// Binds every unbound variable inside `ty` to `Error`, after its problem was reported.
    pub fn bind_error(&mut self, ty: &Ty) {
        match self.shallow_resolve(ty) {
            Ty::Infer(v) => self.bind(v, Ty::Error),
            Ty::Ref(_, inner) => self.bind_error(&inner),
            _ => {}
        }
    }

    /// Whether `v` appears in `ty`. Binding `v` to such a type would make it infinite
    /// (`let r; r = &r;`), and resolving it would never terminate.
    fn occurs(&mut self, v: TyVid, ty: &Ty) -> bool {
        match self.shallow_resolve(ty) {
            Ty::Infer(w) => self.root(v) == self.root(w),
            Ty::Ref(_, inner) => self.occurs(v, &inner),
            _ => false,
        }
    }

    /// Whether unifying `a` with `b` fails only because it would build an infinite type.
    pub fn is_cyclic(&mut self, a: &Ty, b: &Ty) -> bool {
        match (self.shallow_resolve(a), self.shallow_resolve(b)) {
            (Ty::Infer(_), Ty::Infer(_)) => false,
            (Ty::Infer(v), other) | (other, Ty::Infer(v)) => self.occurs(v, &other),
            (Ty::Ref(_, ia), Ty::Ref(_, ib)) => self.is_cyclic(&ia, &ib),
            _ => false,
        }
    }

    fn prim_fits(kind: VarKind, p: PrimTy) -> bool {
        match kind {
            VarKind::General => true,
            VarKind::Int => p.is_integer(),
            VarKind::Float => p.is_float(),
        }
    }

    /// Makes `a` and `b` the same type, binding variables as needed. `Error` unifies with
    /// anything without binding. `Never` does not unify with other types here: coercion
    /// from `!` is handled by the caller, so that `!` never leaks into a variable.
    pub fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), UnifyError> {
        let a = self.shallow_resolve(a);
        let b = self.shallow_resolve(b);
        match (&a, &b) {
            (Ty::Error, _) | (_, Ty::Error) => Ok(()),
            (Ty::Infer(va), Ty::Infer(vb)) => {
                let (ra, rb) = (self.root(*va), self.root(*vb));
                if ra == rb {
                    return Ok(());
                }
                let kind = match (self.vars[ra.0 as usize].kind, self.vars[rb.0 as usize].kind) {
                    (k, VarKind::General) | (VarKind::General, k) => k,
                    (k1, k2) if k1 == k2 => k1,
                    _ => return Err(UnifyError),
                };
                self.vars[rb.0 as usize].parent = ra;
                self.vars[ra.0 as usize].kind = kind;
                Ok(())
            }
            (Ty::Infer(v), other) | (other, Ty::Infer(v)) => {
                let kind = self.kind(*v);
                let ok = match other {
                    Ty::Prim(p) => Self::prim_fits(kind, *p),
                    Ty::Never => false,
                    _ => kind == VarKind::General && !self.occurs(*v, other),
                };
                if ok {
                    self.bind(*v, other.clone());
                    Ok(())
                } else {
                    Err(UnifyError)
                }
            }
            (Ty::Ref(ma, ia), Ty::Ref(mb, ib)) => {
                if ma != mb {
                    return Err(UnifyError);
                }
                self.unify(ia, ib)
            }
            _ if a == b => Ok(()),
            _ => Err(UnifyError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_variables_default_and_constrain() {
        let mut cx = InferCtxt::new();
        let a = cx.new_var(VarKind::Int);
        let b = cx.new_var(VarKind::General);
        cx.unify(&a, &b).unwrap();
        assert_eq!(cx.resolve_with_defaults(&b), Ty::I32);
        assert!(cx.unify(&a, &Ty::F64).is_err());
        assert!(cx.unify(&a, &Ty::Prim(PrimTy::U8)).is_ok());
        assert_eq!(cx.resolve(&b), Ty::Prim(PrimTy::U8));

        let f = cx.new_var(VarKind::Float);
        assert!(cx.unify(&f, &Ty::BOOL).is_err());
        assert_eq!(cx.resolve_with_defaults(&f), Ty::F64);
    }

    #[test]
    fn references_unify_structurally() {
        let mut cx = InferCtxt::new();
        let v = cx.new_var(VarKind::General);
        let r = Ty::shared_ref(v.clone());
        cx.unify(&r, &Ty::shared_ref(Ty::BOOL)).unwrap();
        assert_eq!(cx.resolve(&v), Ty::BOOL);
        assert!(cx
            .unify(&Ty::mut_ref(Ty::BOOL), &Ty::shared_ref(Ty::BOOL))
            .is_err());
        assert!(cx.unify(&Ty::Never, &Ty::BOOL).is_err());
        assert!(cx.unify(&Ty::Error, &Ty::BOOL).is_ok());
    }

    #[test]
    fn occurs_check_rejects_infinite_types() {
        let mut cx = InferCtxt::new();
        let v = cx.new_var(VarKind::General);
        let w = cx.new_var(VarKind::General);
        cx.unify(&v, &w).unwrap();
        assert!(cx
            .unify(&v, &Ty::shared_ref(Ty::shared_ref(w.clone())))
            .is_err());
        assert!(cx.unify(&Ty::mut_ref(v.clone()), &v).is_err());
        assert!(matches!(cx.resolve(&v), Ty::Infer(_)));

        assert!(cx.is_cyclic(&v, &Ty::shared_ref(w.clone())));
        assert!(cx.is_cyclic(
            &Ty::mut_ref(Ty::shared_ref(v.clone())),
            &Ty::mut_ref(w.clone())
        ));
        assert!(!cx.is_cyclic(&Ty::shared_ref(v.clone()), &Ty::shared_ref(Ty::BOOL)));
        assert!(!cx.is_cyclic(&v, &w));
        assert!(!cx.is_cyclic(&Ty::BOOL, &Ty::shared_ref(v.clone())));
        let i = cx.new_var(VarKind::Int);
        let f = cx.new_var(VarKind::Float);
        assert!(!cx.is_cyclic(&i, &f));
    }

    /// `!` coerces to anything but never unifies: a variable stays open, so a diverging
    /// initialiser falls back to `()` instead of making the binding `!`.
    #[test]
    fn never_does_not_bind_a_variable() {
        let mut cx = InferCtxt::new();
        let v = cx.new_var(VarKind::General);
        assert!(cx.unify(&v, &Ty::Never).is_err());
        assert!(cx.unify(&Ty::Never, &v).is_err());
        assert!(matches!(cx.resolve(&v), Ty::Infer(_)));
    }
}
