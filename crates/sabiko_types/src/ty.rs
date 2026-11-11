//! The semantic type representation.

use sabiko_resolve::{DefId, PrimTy, ResolveResults};

/// Whether a reference or binding allows mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mutability {
    Not,
    Mut,
}

impl Mutability {
    #[must_use]
    pub fn from_bool(mutable: bool) -> Self {
        if mutable {
            Self::Mut
        } else {
            Self::Not
        }
    }

    #[must_use]
    pub fn is_mut(self) -> bool {
        self == Self::Mut
    }

    /// `"mut "` or `""`, for printing `&mut T` and `let mut x`.
    #[must_use]
    pub fn prefix_str(self) -> &'static str {
        match self {
            Self::Not => "",
            Self::Mut => "mut ",
        }
    }
}

/// An inference variable. The kind (general, integer literal, float literal) lives in the
/// inference table, not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVid(pub u32);

/// A Sabiko type. After type checking no `Infer` variables remain in recorded types.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Prim(PrimTy),
    /// `()`
    Unit,
    /// `!`: the type of `break`, `continue`, `return` and a `loop` without `break`.
    Never,
    /// A user struct or enum, with its generic arguments (empty for one that is not
    /// generic; one per type parameter otherwise).
    Adt(DefId, Box<[Ty]>),
    Ref(Mutability, Box<Ty>),
    /// `(A,)`, `(A, B)`, ...: never empty, since `()` is [`Ty::Unit`].
    Tuple(Box<Vec<Ty>>),
    /// `[T; N]`, with the length evaluated.
    Array(Box<Ty>, u64),
    /// `[T]`: unsized, only behind a reference.
    Slice(Box<Ty>),
    /// The zero-sized type of a function item; only callable.
    FnDef(DefId),
    /// A generic type parameter (its definition), in the signature and body of the
    /// function that declares it. Monomorphisation substitutes it before code generation
    /// (decision 82), so no later stage than borrow checking sees one.
    Param(DefId),
    Infer(TyVid),
    /// Produced after an error was reported; unifies with everything silently.
    Error,
}

impl Ty {
    pub const BOOL: Ty = Ty::Prim(PrimTy::Bool);
    pub const CHAR: Ty = Ty::Prim(PrimTy::Char);
    pub const I32: Ty = Ty::Prim(PrimTy::I32);
    pub const F64: Ty = Ty::Prim(PrimTy::F64);

    /// A struct or enum that is not generic.
    #[must_use]
    pub fn adt(def: DefId) -> Ty {
        Ty::Adt(def, Box::new([]))
    }

    #[must_use]
    pub fn shared_ref(inner: Ty) -> Ty {
        Ty::Ref(Mutability::Not, Box::new(inner))
    }

    #[must_use]
    pub fn mut_ref(inner: Ty) -> Ty {
        Ty::Ref(Mutability::Mut, Box::new(inner))
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Ty::Error)
    }

    /// Whether the type is `Error` or contains it (an element, a referent, a generic
    /// argument).
    #[must_use]
    pub fn references_error(&self) -> bool {
        match self {
            Ty::Error => true,
            Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => inner.references_error(),
            Ty::Tuple(elems) => elems.iter().any(Ty::references_error),
            Ty::Adt(_, args) => args.iter().any(Ty::references_error),
            _ => false,
        }
    }

    #[must_use]
    pub fn is_never(&self) -> bool {
        matches!(self, Ty::Never)
    }

    /// `!`, a reference to an uninhabited type (`&!`, the type of `&loop {}`), or a tuple
    /// with an uninhabited element: no value of it is ever produced, so code generation
    /// gives it no storage.
    #[must_use]
    pub fn is_uninhabited(&self) -> bool {
        match self {
            Ty::Never => true,
            Ty::Ref(_, inner) => inner.is_uninhabited(),
            Ty::Tuple(elems) => elems.iter().any(Ty::is_uninhabited),
            Ty::Array(elem, n) => *n > 0 && elem.is_uninhabited(),
            _ => false,
        }
    }

    #[must_use]
    pub fn is_unit(&self) -> bool {
        matches!(self, Ty::Unit)
    }

    #[must_use]
    pub fn as_prim(&self) -> Option<PrimTy> {
        match self {
            Ty::Prim(p) => Some(*p),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_integer(&self) -> bool {
        self.as_prim().is_some_and(PrimTy::is_integer)
    }

    #[must_use]
    pub fn is_bool(&self) -> bool {
        matches!(self, Ty::Prim(PrimTy::Bool))
    }

    /// Whether values of this type are copied rather than moved, ignoring
    /// `#[derive(Copy)]`: primitives, `()`, shared references, function items and tuples
    /// of `Copy` elements are `Copy`; `&mut T` is not, and structs are reported as not
    /// `Copy` here. Use `ItemTypes::is_copy` for the item-aware answer.
    #[must_use]
    pub fn is_copy(&self) -> bool {
        match self {
            Ty::Prim(_) | Ty::Unit | Ty::Never | Ty::FnDef(_) | Ty::Error => true,
            Ty::Ref(m, _) => !m.is_mut(),
            Ty::Tuple(elems) => elems.iter().all(Ty::is_copy),
            Ty::Array(elem, _) => elem.is_copy(),
            // A type parameter has no bounds yet, so nothing makes it `Copy` (rustc).
            Ty::Adt(..) | Ty::Slice(_) | Ty::Infer(_) | Ty::Param(_) => false,
        }
    }

    /// Whether the type mentions the type parameter `param`.
    #[must_use]
    pub fn mentions(&self, param: DefId) -> bool {
        match self {
            Ty::Param(p) => *p == param,
            Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => inner.mentions(param),
            Ty::Tuple(elems) => elems.iter().any(|e| e.mentions(param)),
            Ty::Adt(_, args) => args.iter().any(|a| a.mentions(param)),
            _ => false,
        }
    }

    /// Whether the type mentions a type parameter.
    #[must_use]
    pub fn has_param(&self) -> bool {
        match self {
            Ty::Param(_) => true,
            Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => inner.has_param(),
            Ty::Tuple(elems) => elems.iter().any(Ty::has_param),
            Ty::Adt(_, args) => args.iter().any(Ty::has_param),
            _ => false,
        }
    }

    /// The type with each parameter `params[i]` replaced by `args[i]`; parameters not in
    /// `params` stay.
    #[must_use]
    pub fn subst(&self, params: &[DefId], args: &[Ty]) -> Ty {
        if params.is_empty() || !self.has_param() {
            return self.clone();
        }
        self.map_params(&mut |p| params.iter().position(|&q| q == p).map(|i| args[i].clone()))
    }

    /// The type with each parameter for which `f` returns `Some` replaced.
    #[must_use]
    pub fn map_params(&self, f: &mut impl FnMut(DefId) -> Option<Ty>) -> Ty {
        match self {
            Ty::Param(p) => f(*p).unwrap_or_else(|| self.clone()),
            Ty::Ref(m, inner) => Ty::Ref(*m, Box::new(inner.map_params(f))),
            Ty::Array(inner, n) => Ty::Array(Box::new(inner.map_params(f)), *n),
            Ty::Slice(inner) => Ty::Slice(Box::new(inner.map_params(f))),
            Ty::Tuple(elems) => {
                Ty::Tuple(Box::new(elems.iter().map(|e| e.map_params(f)).collect()))
            }
            Ty::Adt(def, args) => Ty::Adt(*def, args.iter().map(|a| a.map_params(f)).collect()),
            _ => self.clone(),
        }
    }

    #[must_use]
    pub fn has_infer(&self) -> bool {
        match self {
            Ty::Infer(_) => true,
            Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => inner.has_infer(),
            Ty::Tuple(elems) => elems.iter().any(Ty::has_infer),
            Ty::Adt(_, args) => args.iter().any(Ty::has_infer),
            _ => false,
        }
    }

    /// Whether the type contains a reference anywhere (directly, or in a tuple element, an
    /// array element, or a generic argument of a struct or enum: every type parameter
    /// appears in a field, E0340).
    #[must_use]
    pub fn has_ref(&self) -> bool {
        match self {
            Ty::Ref(..) => true,
            Ty::Tuple(elems) => elems.iter().any(Ty::has_ref),
            Ty::Adt(_, args) => args.iter().any(Ty::has_ref),
            Ty::Array(elem, _) | Ty::Slice(elem) => elem.has_ref(),
            _ => false,
        }
    }

    /// Renders the type with item names taken from `resolve`.
    #[must_use]
    pub fn display(&self, resolve: &ResolveResults) -> String {
        let mut out = String::new();
        self.write(resolve, &mut out, false);
        out
    }

    /// Renders the type as the names of generated code see it: like [`Ty::display`], with
    /// the `core` prelude's items qualified (`core::Option<i32>`), so that a user type of
    /// the same name never gives an instance the same name (decision 90).
    #[must_use]
    pub fn symbol(&self, resolve: &ResolveResults) -> String {
        let mut out = String::new();
        self.write(resolve, &mut out, true);
        out
    }

    fn write(&self, resolve: &ResolveResults, out: &mut String, symbol: bool) {
        let name = |d: DefId| {
            if symbol {
                resolve.symbol_name(d)
            } else {
                resolve.def(d).name.clone()
            }
        };
        match self {
            Ty::Prim(p) => out.push_str(p.name()),
            Ty::Unit => out.push_str("()"),
            Ty::Never => out.push('!'),
            Ty::Param(d) => out.push_str(&resolve.def(*d).name),
            Ty::Adt(d, args) => {
                out.push_str(&name(*d));
                if !args.is_empty() {
                    out.push('<');
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        a.write(resolve, out, symbol);
                    }
                    out.push('>');
                }
            }
            Ty::Ref(m, inner) => {
                out.push('&');
                out.push_str(m.prefix_str());
                inner.write(resolve, out, symbol);
            }
            Ty::Tuple(elems) => {
                out.push('(');
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    e.write(resolve, out, symbol);
                }
                if elems.len() == 1 {
                    out.push(',');
                }
                out.push(')');
            }
            Ty::Array(elem, n) => {
                out.push('[');
                elem.write(resolve, out, symbol);
                out.push_str("; ");
                out.push_str(&n.to_string());
                out.push(']');
            }
            Ty::Slice(elem) => {
                out.push('[');
                elem.write(resolve, out, symbol);
                out.push(']');
            }
            Ty::FnDef(d) => {
                out.push_str("fn item `");
                out.push_str(&resolve.def(*d).name);
                out.push('`');
            }
            Ty::Infer(_) => out.push('_'),
            Ty::Error => out.push_str("{error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(elems: Vec<Ty>) -> Ty {
        Ty::Tuple(Box::new(elems))
    }

    #[test]
    fn uninhabited_through_references_and_tuples() {
        assert!(Ty::Never.is_uninhabited());
        assert!(Ty::shared_ref(Ty::Never).is_uninhabited());
        assert!(tuple(vec![Ty::BOOL, Ty::Never]).is_uninhabited());
        assert!(tuple(vec![Ty::mut_ref(tuple(vec![Ty::Never]))]).is_uninhabited());
        assert!(!tuple(vec![Ty::BOOL, Ty::Unit]).is_uninhabited());
        assert!(!Ty::shared_ref(Ty::BOOL).is_uninhabited());
        // An array of `!` is uninhabited unless it is empty, as in rustc.
        assert!(Ty::Array(Box::new(Ty::Never), 2).is_uninhabited());
        assert!(Ty::Array(Box::new(Ty::Never), 1).is_uninhabited());
        assert!(!Ty::Array(Box::new(Ty::Never), 0).is_uninhabited());
        assert!(!Ty::Array(Box::new(Ty::BOOL), 2).is_uninhabited());
    }

    /// `Ty::is_copy` is the fallback of `ItemTypes::is_copy_in` for a program without a
    /// `Copy` language item, which the resolver always defines (decision 86), so no program
    /// reaches it: its rules are checked here.
    #[test]
    fn copy_without_items() {
        assert!(Ty::I32.is_copy());
        assert!(Ty::Unit.is_copy());
        assert!(Ty::Never.is_copy());
        assert!(Ty::shared_ref(Ty::Slice(Box::new(Ty::I32))).is_copy());
        assert!(!Ty::mut_ref(Ty::I32).is_copy());
        assert!(tuple(vec![Ty::BOOL, Ty::shared_ref(Ty::I32)]).is_copy());
        assert!(!tuple(vec![Ty::BOOL, Ty::mut_ref(Ty::I32)]).is_copy());
        assert!(Ty::Array(Box::new(Ty::I32), 2).is_copy());
        assert!(!Ty::Array(Box::new(Ty::mut_ref(Ty::I32)), 2).is_copy());
        assert!(!Ty::Slice(Box::new(Ty::I32)).is_copy());
        assert!(!Ty::Adt(DefId(0), Box::new([])).is_copy());
    }

    #[test]
    fn has_ref_through_tuples_arrays_and_slices() {
        assert!(Ty::shared_ref(Ty::BOOL).has_ref());
        assert!(tuple(vec![Ty::BOOL, Ty::mut_ref(Ty::Unit)]).has_ref());
        assert!(Ty::Array(Box::new(Ty::shared_ref(Ty::I32)), 2).has_ref());
        assert!(Ty::Slice(Box::new(tuple(vec![Ty::mut_ref(Ty::I32)]))).has_ref());
        assert!(!Ty::Array(Box::new(Ty::I32), 2).has_ref());
        assert!(!Ty::Slice(Box::new(Ty::BOOL)).has_ref());
    }

    #[test]
    fn has_infer_through_references_and_tuples() {
        let v = Ty::Infer(TyVid(0));
        assert!(v.has_infer());
        assert!(Ty::shared_ref(v.clone()).has_infer());
        assert!(tuple(vec![Ty::BOOL, v.clone()]).has_infer());
        assert!(tuple(vec![Ty::mut_ref(v)]).has_infer());
        assert!(!tuple(vec![Ty::BOOL, Ty::shared_ref(Ty::Unit)]).has_infer());
        assert!(!Ty::Error.has_infer());
    }

    #[test]
    fn references_error_at_any_depth() {
        let adt = |args: Vec<Ty>| Ty::Adt(DefId(0), args.into());
        assert!(Ty::Error.references_error());
        assert!(adt(vec![Ty::I32, Ty::Error]).references_error());
        assert!(Ty::shared_ref(Ty::Array(Box::new(Ty::Error), 2)).references_error());
        assert!(Ty::Slice(Box::new(tuple(vec![Ty::BOOL, Ty::Error]))).references_error());
        assert!(adt(vec![adt(vec![Ty::mut_ref(Ty::Error)])]).references_error());
        assert!(!adt(vec![Ty::Array(Box::new(Ty::I32), 2), Ty::Unit]).references_error());
        assert!(!Ty::Infer(TyVid(0)).references_error());
    }
}
