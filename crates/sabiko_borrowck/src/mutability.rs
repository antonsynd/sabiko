//! Place mutability: may a place be written to or borrowed mutably?
//!
//! Follows rustc: walk the projections from the root local. The root's own mutability is
//! that of its binding (temporaries and the return place are mutable). A field keeps the
//! state so far. Dereferencing a `&mut T` makes everything beyond it mutable, whatever the
//! binding holding the reference: `*r = 1` is fine for `let r: &mut i32`. Dereferencing a
//! `&T` makes everything beyond it immutable for good.

use sabiko_mir::{Body, Local, LocalKind, Mutability, Place, PlaceElem, Ty};

/// Why a place may, or may not, be mutated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceMutability {
    Mutable,
    /// The place is owned (directly or through `&mut`s) by this local, which is not
    /// declared `mut`.
    ImmutableBinding(Local),
    /// The place is reached through a `&T`; the payload is the place holding that shared
    /// reference (the prefix up to, but excluding, the offending `Deref`).
    BehindSharedRef(Place),
}

/// Classifies `place` according to the rules in the module docs.
#[must_use]
pub fn place_mutability(place: &Place, body: &Body) -> PlaceMutability {
    let decl = body.local_decl(place.local);
    let mut mutable = match decl.kind {
        LocalKind::Return | LocalKind::Temp => true,
        LocalKind::Arg | LocalKind::User => decl.mutability.is_mut(),
    };
    let mut ty = decl.ty.clone();
    for (index, elem) in place.projection.iter().enumerate() {
        match elem {
            PlaceElem::Deref => match ty {
                Ty::Ref(Mutability::Mut, inner) => {
                    mutable = true;
                    ty = *inner;
                }
                Ty::Ref(Mutability::Not, _) => {
                    return PlaceMutability::BehindSharedRef(Place {
                        local: place.local,
                        projection: place.projection[..index].to_vec(),
                    });
                }
                // Ill-typed MIR; the validator reports it. Stay permissive here.
                other => ty = other,
            },
            PlaceElem::Field(_, field_ty) => ty = field_ty.clone(),
        }
    }
    if mutable {
        PlaceMutability::Mutable
    } else {
        PlaceMutability::ImmutableBinding(place.local)
    }
}
