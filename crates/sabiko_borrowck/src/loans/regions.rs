//! Origins ("regions") for the loan check.
//!
//! Sabiko has no lifetime syntax yet, so every reference type gets an inferred origin:
//! a [`RegionTy`] mirrors a [`Ty`] and carries one [`Origin`] per `Ref` node. Struct
//! fields never hold references in this phase, so origins only ever appear along a chain
//! of `Ref`s (`&&T`, `&mut &T`).
//!
//! Function signatures use *universal* origins: one per reference in the parameter types.
//! The return type follows Rust's elision rule for functions without `self`: when exactly
//! one origin occurs across all parameters, every reference in the return type uses it;
//! otherwise the signature is unusable and the caller reports E0416.
//!
//! This module also defines the atom newtypes the Polonius engine indexes its facts with.

use polonius_engine::Atom;
use sabiko_mir::{Mutability, PlaceElem, Ty};
use sabiko_types::FnSig;

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

    /// The number of origins created so far (every origin is below this).
    #[must_use]
    pub fn count(&self) -> usize {
        self.next
    }
}

/// A type with an origin on every reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionTy {
    Ref(Mutability, Origin, Box<RegionTy>),
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
            _ => Self::Other,
        }
    }

    /// `ty` with the same origin on every reference (the elided return type).
    #[must_use]
    pub fn uniform(ty: &Ty, origin: Origin) -> Self {
        match ty {
            Ty::Ref(m, inner) => Self::Ref(*m, origin, Box::new(Self::uniform(inner, origin))),
            _ => Self::Other,
        }
    }

    /// Every origin in the type, outermost first.
    #[must_use]
    pub fn origins(&self) -> Vec<Origin> {
        let mut out = Vec::new();
        let mut cur = self;
        while let Self::Ref(_, origin, inner) = cur {
            out.push(*origin);
            cur = inner;
        }
        out
    }

    /// The type of `*self`; `Other` when `self` is not a reference.
    #[must_use]
    pub fn referent(&self) -> &RegionTy {
        match self {
            Self::Ref(_, _, inner) => inner,
            Self::Other => self,
        }
    }

    /// The type of a place built from `self` by `projection`. A field access yields
    /// `Other`: fields hold no references.
    #[must_use]
    pub fn project(&self, projection: &[PlaceElem]) -> RegionTy {
        let mut cur = self;
        for elem in projection {
            match elem {
                PlaceElem::Deref => cur = cur.referent(),
                PlaceElem::Field(..) => return Self::Other,
            }
        }
        cur.clone()
    }
}

/// Whether `ty` contains a reference anywhere.
fn has_ref(ty: &Ty) -> bool {
    matches!(ty, Ty::Ref(..))
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
    /// Every origin of the parameters, outermost first within each parameter.
    pub fn input_origins(&self) -> impl Iterator<Item = Origin> + '_ {
        self.inputs.iter().flat_map(RegionTy::origins)
    }

    /// `(origin, parameter index, depth)` for every parameter origin: `depth` is how
    /// many references enclose the one with this origin (`0` for `x: &T`, `1` for the
    /// inner reference of `x: &mut &T`).
    pub fn input_origin_sites(&self) -> impl Iterator<Item = (Origin, usize, usize)> + '_ {
        self.inputs.iter().enumerate().flat_map(|(param, ty)| {
            ty.origins()
                .into_iter()
                .enumerate()
                .map(move |(depth, origin)| (origin, param, depth))
        })
    }

    /// Implied bounds: inside `&'a &'b T` the referent outlives the reference, `'b: 'a`,
    /// returned as `(longer, shorter)` pairs, i.e. Polonius' `known_placeholder_subset`.
    pub fn implied_bounds(&self) -> Vec<(Origin, Origin)> {
        let mut out = Vec::new();
        for ty in &self.inputs {
            let origins = ty.origins();
            for pair in origins.windows(2) {
                out.push((pair[1], pair[0]));
            }
        }
        out
    }
}

/// Assigns fresh origins to the parameters of `sig` and elides the return type.
pub fn signature_regions(sig: &FnSig, supply: &mut OriginSupply) -> SigRegions {
    let inputs: Vec<RegionTy> = sig
        .inputs
        .iter()
        .map(|ty| RegionTy::fresh(ty, supply))
        .collect();
    let input_origins: Vec<Origin> = inputs.iter().flat_map(RegionTy::origins).collect();
    let output = if !has_ref(&sig.output) {
        Some(RegionTy::Other)
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
