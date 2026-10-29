//! Origins ("regions") for the loan check.
//!
//! Sabiko has no lifetime syntax yet, so every reference type gets an inferred origin:
//! a [`RegionTy`] mirrors a [`Ty`] and carries one [`Origin`] per `Ref` node. Struct
//! fields never hold references in this phase, so origins appear along chains of `Ref`s
//! (`&&T`, `&mut &T`) and in the elements of tuples (`(&T, &mut U)`).
//!
//! Function signatures use *universal* origins: one per reference in the parameter types.
//! The return type follows Rust's elision rule for functions without `self`: when exactly
//! one origin occurs across all parameters, every reference in the return type uses it;
//! otherwise the signature is unusable and the caller reports E0416.
//!
//! This module also defines the atom newtypes the Polonius engine indexes its facts with.

use polonius_engine::Atom;
use sabiko_mir::{Mutability, PlaceElem, Ty};

use sabiko_types::{FnSig, SelfKind};

macro_rules! atom {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub usize);

        impl $name {
            #[must_use]
            pub const fn index(self) -> usize {
                self.0
            }
        }

        impl From<usize> for $name {
            fn from(index: usize) -> Self {
                Self(index)
            }
        }

        impl From<$name> for usize {
            fn from(atom: $name) -> usize {
                atom.0
            }
        }

        impl Atom for $name {
            fn index(self) -> usize {
                self.0
            }
        }
    };
}

atom!(
    /// A region: the set of loans a reference may hold.
    Origin
);
atom!(
    /// A borrow, `&place` or `&mut place`, in order of appearance in the body. Universal
    /// origins get placeholder loans numbered after the real ones.
    Loan
);
atom!(
    /// A point in the CFG: `Start` or `Mid` of a MIR location (see `facts::LocationTable`).
    Point
);
atom!(
    /// A MIR local.
    Variable
);
atom!(
    /// A move path. Unused: the move checker is a separate dataflow analysis.
    Path
);

/// Hands out fresh origins.
#[derive(Debug, Default)]
pub struct OriginSupply {
    next: usize,
}

impl OriginSupply {
    pub fn fresh(&mut self) -> Origin {
        let origin = Origin(self.next);
        self.next += 1;
        origin
    }
}

/// A type with an origin on every reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionTy {
    Ref(Mutability, Origin, Box<RegionTy>),
    /// A tuple with references among its elements.
    Tuple(Vec<RegionTy>),
    /// Anything without references: primitives, structs, `()`, `!`, errors.
    Other,
}

impl RegionTy {
    /// `ty` with a fresh origin on every reference.
    pub fn fresh(ty: &Ty, supply: &mut OriginSupply) -> Self {
        match ty {
            Ty::Ref(m, inner) => {
                let origin = supply.fresh();
                Self::Ref(*m, origin, Box::new(Self::fresh(inner, supply)))
            }
            Ty::Tuple(elems) if ty.has_ref() => {
                Self::Tuple(elems.iter().map(|e| Self::fresh(e, supply)).collect())
            }
            _ => Self::Other,
        }
    }

    /// `ty` with the same origin on every reference (the elided return type).
    #[must_use]
    pub fn uniform(ty: &Ty, origin: Origin) -> Self {
        match ty {
            Ty::Ref(m, inner) => Self::Ref(*m, origin, Box::new(Self::uniform(inner, origin))),
            Ty::Tuple(elems) if ty.has_ref() => {
                Self::Tuple(elems.iter().map(|e| Self::uniform(e, origin)).collect())
            }
            _ => Self::Other,
        }
    }

    /// Every origin in the type, depth first, outermost first along each chain of
    /// references, tuple elements left to right.
    #[must_use]
    pub fn origins(&self) -> Vec<Origin> {
        let mut out = Vec::new();
        self.sites(&mut Vec::new(), &mut |origin, _: &[SiteElem]| {
            out.push(origin);
        });
        out
    }

    /// Calls `f` with every origin in the type and the projection from a value of this
    /// type to the reference carrying it (`[]` for the outermost reference, `[Deref]` for
    /// the one it points to, `[Field(1)]` for the second element of a tuple).
    pub fn sites(&self, path: &mut Vec<SiteElem>, f: &mut impl FnMut(Origin, &[SiteElem])) {
        match self {
            Self::Ref(_, origin, inner) => {
                f(*origin, path);
                path.push(SiteElem::Deref);
                inner.sites(path, f);
                path.pop();
            }
            Self::Tuple(elems) => {
                for (i, e) in elems.iter().enumerate() {
                    path.push(SiteElem::Field(i));
                    e.sites(path, f);
                    path.pop();
                }
            }
            Self::Other => {}
        }
    }

    /// The type of `*self`; `Other` when `self` is not a reference.
    #[must_use]
    pub fn referent(&self) -> &RegionTy {
        match self {
            Self::Ref(_, _, inner) => inner,
            Self::Tuple(_) | Self::Other => &Self::Other,
        }
    }

    /// The type of a place built from `self` by `projection`. A field of a struct is
    /// `Other` (struct fields hold no references); a tuple element has its own origins.
    #[must_use]
    pub fn project(&self, projection: &[PlaceElem]) -> RegionTy {
        let mut cur = self;
        for elem in projection {
            match (elem, cur) {
                (PlaceElem::Deref, _) => cur = cur.referent(),
                (PlaceElem::Field(index, _), Self::Tuple(elems)) => match elems.get(*index) {
                    Some(e) => cur = e,
                    None => return Self::Other,
                },
                (PlaceElem::Field(..), _) => return Self::Other,
            }
        }
        cur.clone()
    }
}

/// One step of the path from a parameter to a reference in its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteElem {
    Deref,
    Field(usize),
}

/// The origins of one function signature.
#[derive(Clone, Debug)]
pub struct SigRegions {
    pub inputs: Vec<RegionTy>,
    /// `None` when the return type contains a reference but elision cannot pick its
    /// origin (E0416).
    pub output: Option<RegionTy>,
    /// How many reference origins the parameters introduce, for the E0416 note.
    pub input_origins: usize,
}

impl SigRegions {
    /// `(origin, parameter index, path)` for every parameter origin: `path` leads from
    /// the parameter to the reference with this origin (`[]` for `x: &T`, `[Deref]` for
    /// the inner reference of `x: &mut &T`, `[Field(1)]` for `t.1` of `t: (i32, &T)`).
    #[must_use]
    pub fn input_origin_sites(&self) -> Vec<(Origin, usize, Vec<SiteElem>)> {
        let mut out = Vec::new();
        for (param, ty) in self.inputs.iter().enumerate() {
            ty.sites(&mut Vec::new(), &mut |origin, path| {
                out.push((origin, param, path.to_vec()));
            });
        }
        out
    }

    /// Implied bounds: inside `&'a &'b T` (or `&'a (&'b T, U)`) the referent outlives the
    /// reference, `'b: 'a`, returned as `(longer, shorter)` pairs, i.e. Polonius'
    /// `known_placeholder_subset`.
    pub fn implied_bounds(&self) -> Vec<(Origin, Origin)> {
        fn walk(ty: &RegionTy, out: &mut Vec<(Origin, Origin)>) {
            match ty {
                RegionTy::Ref(_, outer, inner) => {
                    for inner_origin in inner.origins() {
                        out.push((inner_origin, *outer));
                    }
                    walk(inner, out);
                }
                RegionTy::Tuple(elems) => elems.iter().for_each(|e| walk(e, out)),
                RegionTy::Other => {}
            }
        }
        let mut out = Vec::new();
        for ty in &self.inputs {
            walk(ty, &mut out);
        }
        out
    }
}

/// Assigns fresh origins to the parameters of `sig` and elides the return type, by Rust's
/// rules: a method taking `&self` or `&mut self` returns references with `self`'s origin,
/// whatever its other parameters; otherwise the one origin of the parameters, if there is
/// exactly one.
pub fn signature_regions(sig: &FnSig, supply: &mut OriginSupply) -> SigRegions {
    let inputs: Vec<RegionTy> = sig
        .inputs
        .iter()
        .map(|ty| RegionTy::fresh(ty, supply))
        .collect();
    let input_origins: Vec<Origin> = inputs.iter().flat_map(RegionTy::origins).collect();
    let self_origin = match (sig.self_kind, inputs.first()) {
        (Some(SelfKind::Ref { .. }), Some(RegionTy::Ref(_, origin, _))) => Some(*origin),
        _ => None,
    };
    let output = if !sig.output.has_ref() {
        Some(RegionTy::Other)
    } else if let Some(origin) = self_origin {
        Some(RegionTy::uniform(&sig.output, origin))
    } else if let [single] = input_origins[..] {
        Some(RegionTy::uniform(&sig.output, single))
    } else {
        None
    };
    SigRegions {
        inputs,
        output,
        input_origins: input_origins.len(),
    }
}
