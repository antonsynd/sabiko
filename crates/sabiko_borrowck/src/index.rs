//! The reads rustc's MIR builder emits for the bounds check of each index in a place
//! (decision 96), which Sabiko's MIR leaves implicit.
//!
//! Before `a[i]` is used, rustc reads the indexed place `a` to compare `i` with its length
//! (`len_of_slice_or_array`):
//! - an array's length is part of its type, but the place is still read, by a
//!   `FakeRead(ForIndex, a)` that the borrow checker checks for moves and initialisation
//!   only, conflicting with no loan;
//! - a slice reached as `*s` from a local `s: &[T]` (a `Copy` reference) has its length
//!   read from `copy s`, an ordinary read of `s`;
//! - any other slice (`*s` with `s: &mut [T]`, `*t.0`, `**ss`) has its length read through
//!   a fake raw borrow, `&raw const (fake) *s`: a borrow for the move checks ("borrow of
//!   moved value") and a shallow read of the length for the loan checks, which conflicts
//!   with a mutable loan of the slice place or of a prefix of it, not with a loan of an
//!   element (rustc's `ArtificialField::ArrayLength`).
//!
//! Every read, write or borrow of an element place in Sabiko's MIR checks these bases
//! through its own access, but a `PlaceMention` (`let _ = a[i];`, a pattern that binds
//! nothing, and the mentions MIR building puts before `&a[i]`, `a[i] = v`, `m[i][j]`,
//! `*a[i]` and match scrutinees) reads nothing itself: the borrow checker derives the
//! reads from it with [`index_base_reads`].

use sabiko_mir::{Body, Mutability, Place, PlaceElem, Ty};

/// How the length of one indexed place is read; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BaseRead {
    /// `FakeRead(ForIndex, a)` of an array.
    Array,
    /// `copy s` of a local `s: &[T]`, for an index of `*s`.
    SharedSliceRef,
    /// `&raw const (fake) *s` of any other slice place.
    SliceFakeBorrow,
}

/// The place read for an index's bounds check, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexBaseRead {
    /// The indexed place, except for [`BaseRead::SharedSliceRef`]: the local holding the
    /// reference.
    pub place: Place,
    pub kind: BaseRead,
}

/// The reads of the places indexed by `place`'s `Index` projections, outermost first.
pub(crate) fn index_base_reads<'a>(
    place: &'a Place,
    body: &'a Body,
) -> impl Iterator<Item = IndexBaseRead> + 'a {
    place.index_bases().map(move |base| {
        if !matches!(base.ty(body), Ty::Slice(_)) {
            return IndexBaseRead {
                place: base,
                kind: BaseRead::Array,
            };
        }
        let shared_ref_local = base.projection == [PlaceElem::Deref]
            && matches!(body.local_decl(base.local).ty, Ty::Ref(Mutability::Not, _));
        if shared_ref_local {
            IndexBaseRead {
                place: Place::local(base.local),
                kind: BaseRead::SharedSliceRef,
            }
        } else {
            IndexBaseRead {
                place: base,
                kind: BaseRead::SliceFakeBorrow,
            }
        }
    })
}
