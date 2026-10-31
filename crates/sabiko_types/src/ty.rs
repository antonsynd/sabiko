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
    /// A user struct.
    Adt(DefId),
    Ref(Mutability, Box<Ty>),
    /// `(A,)`, `(A, B)`, ...: never empty, since `()` is [`Ty::Unit`].
    Tuple(Box<Vec<Ty>>),
    /// The zero-sized type of a function item; only callable.
    FnDef(DefId),
    Infer(TyVid),
    /// Produced after an error was reported; unifies with everything silently.
    Error,
}

impl Ty {
    pub const BOOL: Ty = Ty::Prim(PrimTy::Bool);
    pub const CHAR: Ty = Ty::Prim(PrimTy::Char);
    pub const I32: Ty = Ty::Prim(PrimTy::I32);
    pub const F64: Ty = Ty::Prim(PrimTy::F64);

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
            Ty::Adt(_) | Ty::Infer(_) => false,
        }
    }

    #[must_use]
    pub fn has_infer(&self) -> bool {
        match self {
            Ty::Infer(_) => true,
            Ty::Ref(_, inner) => inner.has_infer(),
            Ty::Tuple(elems) => elems.iter().any(Ty::has_infer),
            _ => false,
        }
    }

    /// Whether the type contains a reference anywhere (directly, or in a tuple element).
    #[must_use]
    pub fn has_ref(&self) -> bool {
        match self {
            Ty::Ref(..) => true,
            Ty::Tuple(elems) => elems.iter().any(Ty::has_ref),
            _ => false,
        }
    }

    /// Renders the type with item names taken from `resolve`.
    #[must_use]
    pub fn display(&self, resolve: &ResolveResults) -> String {
        let mut out = String::new();
        self.write(resolve, &mut out);
        out
    }

    fn write(&self, resolve: &ResolveResults, out: &mut String) {
        match self {
            Ty::Prim(p) => out.push_str(p.name()),
            Ty::Unit => out.push_str("()"),
            Ty::Never => out.push('!'),
            Ty::Adt(d) => out.push_str(&resolve.def(*d).name),
            Ty::Ref(m, inner) => {
                out.push('&');
                out.push_str(m.prefix_str());
                inner.write(resolve, out);
            }
            Ty::Tuple(elems) => {
                out.push('(');
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    e.write(resolve, out);
                }
                if elems.len() == 1 {
                    out.push(',');
                }
                out.push(')');
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
}
