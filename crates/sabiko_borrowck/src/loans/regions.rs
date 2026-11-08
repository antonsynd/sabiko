//! Origins ("regions") for the loan check.
//!
//! Sabiko has no lifetime syntax yet, so every reference type gets an inferred origin:
//! a [`RegionTy`] mirrors a [`Ty`] and carries one [`Origin`] per `Ref` node. Struct
//! fields never hold references in this phase, so origins appear along chains of `Ref`s
//! (`&&T`, `&mut &T`), in the elements of tuples (`(&T, &mut U)`) and in the element of
//! an array or slice, represented as a one-element tuple (`[&T; 3]`).
//!
//! Function signatures use *universal* origins: one per reference in the parameter types.
//! The return type follows Rust's elision rules: in a method taking `&self` or `&mut self`
//! every reference in the return type has `self`'s origin; otherwise, when exactly one
//! origin occurs across all parameters, every reference in the return type uses it; failing
//! both, the signature is unusable and the caller reports E0416.
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
            // Every element of an array (or slice) has the same origins: one element.
            Ty::Array(elem, _) | Ty::Slice(elem) if ty.has_ref() => {
                Self::Tuple(vec![Self::fresh(elem, supply)])
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
            Ty::Array(elem, _) | Ty::Slice(elem) if ty.has_ref() => {
                Self::Tuple(vec![Self::uniform(elem, origin)])
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
                // An array holding references is a one-element tuple of its element.
                (PlaceElem::Index(_), Self::Tuple(elems)) => match elems.first() {
                    Some(e) => cur = e,
                    None => return Self::Other,
                },
                (PlaceElem::Field(..) | PlaceElem::Index(_), _) => return Self::Other,
                (PlaceElem::Downcast(_), _) => {}
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
///
/// `generic_args` are a call's generic arguments (empty for a function's own signature,
/// whose type parameters hold no reference of its body). Each argument gets its origins
/// once, and every occurrence of its parameter shares them, as rustc's `T = &'1 i32` does:
/// `Id::<&i32>(&y)` returns a reference with the argument's origin. Elision counts only
/// the references the signature writes, not those of the arguments.
pub fn signature_regions(
    sig: &FnSig,
    generic_args: &[Ty],
    supply: &mut OriginSupply,
) -> SigRegions {
    let args: Vec<RegionTy> = generic_args
        .iter()
        .map(|t| RegionTy::fresh(t, supply))
        .collect();
    let params = Params {
        defs: if args.is_empty() { &[] } else { &sig.generics },
        args: &args,
    };
    let inputs: Vec<RegionTy> = sig
        .inputs
        .iter()
        .map(|ty| params.fresh(ty, supply))
        .collect();
    let mut input_origins = Vec::new();
    for (ty, region) in sig.inputs.iter().zip(&inputs) {
        written_origins(ty, region, &mut input_origins);
    }
    let self_origin = match (sig.self_kind, inputs.first()) {
        (Some(SelfKind::Ref { .. }), Some(RegionTy::Ref(_, origin, _))) => Some(*origin),
        _ => None,
    };
    let output = if !sig.output.has_ref() {
        Some(params.uniform(&sig.output, None))
    } else if let Some(origin) = self_origin {
        Some(params.uniform(&sig.output, Some(origin)))
    } else if let [single] = input_origins[..] {
        Some(params.uniform(&sig.output, Some(single)))
    } else {
        None
    };
    SigRegions {
        inputs,
        output,
        input_origins: input_origins.len(),
    }
}

/// The origins of the references written in `ty` (not those a type parameter stands
/// for), given the region type built for it.
fn written_origins(ty: &Ty, region: &RegionTy, out: &mut Vec<Origin>) {
    match (ty, region) {
        (Ty::Ref(_, inner), RegionTy::Ref(_, origin, rinner)) => {
            out.push(*origin);
            written_origins(inner, rinner, out);
        }
        (Ty::Tuple(elems), RegionTy::Tuple(relems)) => {
            for (e, r) in elems.iter().zip(relems) {
                written_origins(e, r, out);
            }
        }
        (Ty::Array(elem, _) | Ty::Slice(elem), RegionTy::Tuple(relems)) => {
            if let Some(r) = relems.first() {
                written_origins(elem, r, out);
            }
        }
        _ => {}
    }
}

/// A call's type parameters and the region types of its generic arguments.
struct Params<'a> {
    defs: &'a [sabiko_resolve::DefId],
    args: &'a [RegionTy],
}

impl Params<'_> {
    fn arg(&self, param: sabiko_resolve::DefId) -> RegionTy {
        self.defs
            .iter()
            .position(|&d| d == param)
            .map_or(RegionTy::Other, |i| self.args[i].clone())
    }

    /// Whether `ty` holds a reference, counting the arguments' references.
    fn holds_ref(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Param(p) => !self.arg(*p).origins().is_empty(),
            Ty::Ref(..) => true,
            Ty::Tuple(elems) => elems.iter().any(|e| self.holds_ref(e)),
            Ty::Array(elem, _) | Ty::Slice(elem) => self.holds_ref(elem),
            _ => false,
        }
    }

    /// [`RegionTy::fresh`], with each type parameter's argument region type.
    fn fresh(&self, ty: &Ty, supply: &mut OriginSupply) -> RegionTy {
        match ty {
            Ty::Param(p) => self.arg(*p),
            Ty::Ref(m, inner) => {
                let origin = supply.fresh();
                RegionTy::Ref(*m, origin, Box::new(self.fresh(inner, supply)))
            }
            Ty::Tuple(elems) if self.holds_ref(ty) => {
                RegionTy::Tuple(elems.iter().map(|e| self.fresh(e, supply)).collect())
            }
            Ty::Array(elem, _) | Ty::Slice(elem) if self.holds_ref(ty) => {
                RegionTy::Tuple(vec![self.fresh(elem, supply)])
            }
            _ => RegionTy::Other,
        }
    }

    /// [`RegionTy::uniform`] with `origin` on every written reference (none when `None`:
    /// the type writes none), and each type parameter's argument region type.
    fn uniform(&self, ty: &Ty, origin: Option<Origin>) -> RegionTy {
        match ty {
            Ty::Param(p) => self.arg(*p),
            Ty::Ref(m, inner) => match origin {
                Some(o) => RegionTy::Ref(*m, o, Box::new(self.uniform(inner, origin))),
                None => RegionTy::Other,
            },
            Ty::Tuple(elems) if self.holds_ref(ty) => {
                RegionTy::Tuple(elems.iter().map(|e| self.uniform(e, origin)).collect())
            }
            Ty::Array(elem, _) | Ty::Slice(elem) if self.holds_ref(ty) => {
                RegionTy::Tuple(vec![self.uniform(elem, origin)])
            }
            _ => RegionTy::Other,
        }
    }
}
