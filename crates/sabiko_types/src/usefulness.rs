//! Exhaustiveness and reachability checking for `match`, `let`, `if let` and parameter
//! patterns: rustc's usefulness algorithm.
//!
//! Adapted from the Rust compiler (<https://github.com/rust-lang/rust>), crate
//! `rustc_pattern_analysis` (`usefulness.rs`, `constructor.rs`, `rustc.rs`) and the witness
//! printing of `rustc_mir_build` (`thir/pattern/check_match.rs`, `thir/print.rs`).
//! Copyright (c) The Rust Project Contributors, dual-licensed under MIT and Apache-2.0.
//! See `THIRDPARTY.md`.
//!
//! The checker is independent of the type checker: it takes patterns lowered to [`Pat`]
//! (every node carrying its type) and asks a [`PatCx`] for the shape of user types. The
//! algorithm and its choices of witnesses follow rustc 1.99.1 exactly, because Sabiko's
//! E0004/E0005 messages and `unreachable pattern` warnings must read like rustc's:
//!
//! - A matrix of pattern rows is specialized column by column. The constructors seen in the
//!   head column are *split* against the constructors of the column's type (integer ranges
//!   are cut at every boundary of a seen range), and the ones absent from the column are
//!   represented by one `Missing` constructor that only wildcards match.
//! - A row is useful when, after every column is consumed, no unguarded row is above it.
//!   An arm is reachable when one of its rows is useful; an or-pattern alternative that is
//!   never part of a useful row is reported on its own.
//! - Witnesses of non-exhaustiveness are built while unwinding: a specialization by a
//!   present constructor reports its witnesses only when the column has no missing
//!   constructor ("relevancy"), so `match (a, b) { (true, true) => .. }` reports
//!   `(false, _)` alone. Missing constructors are listed one by one at the scrutinee and
//!   whenever some constructor of the column is present, and as `_` otherwise.
//! - Empty types follow rustc's `min_exhaustive_patterns` rules: an uninhabited variant or
//!   struct may be omitted when the place is known to hold a valid value (any place not
//!   behind a reference, unless the scrutinee itself is a dereference), `match n {}` is
//!   exhaustive for `n: !` or an empty enum even behind a reference, and only a pattern on
//!   such a scrutinee of type `!`/empty enum is reported unreachable for being empty.
//! - `isize` and `usize` have no observable minimum and maximum: as in rustc, only
//!   half-open ranges ([`PatKind::OpenRange`]) cover their ends.

use std::collections::HashMap;
use std::rc::Rc;

use sabiko_resolve::{DefId, PrimTy};
use sabiko_span::Span;

use crate::ty::Ty;

// ---------------------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------------------

/// Information about user types the checker needs. `Ty::Adt(def)` is either a struct or an
/// enum.
pub trait PatCx {
    fn adt(&self, def: DefId) -> AdtShape;

    /// The type parameters of a generic struct or enum, which its field types mention.
    fn generics(&self, _def: DefId) -> Vec<DefId> {
        Vec::new()
    }
}

/// The shape of a user type, as [`PatCx::adt`] describes it.
#[derive(Clone, Debug, PartialEq)]
pub enum AdtShape {
    /// A struct: one constructor. Unit structs have no fields.
    Struct {
        name: String,
        fields: Vec<FieldShape>,
        kind: VariantKind,
    },
    /// An enum; `variants` may be empty (`enum Never {}`, uninhabited).
    Enum {
        name: String,
        variants: Vec<VariantShape>,
    },
}

/// One variant of an enum.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantShape {
    pub name: String,
    pub kind: VariantKind,
    pub fields: Vec<FieldShape>,
}

/// One field of a struct or variant, in declaration order.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldShape {
    /// `"0"`, `"1"`, .. for tuple structs and tuple variants.
    pub name: String,
    pub ty: Ty,
}

/// How a struct or variant is declared, which decides how a witness prints it: `S`,
/// `S(_)` or `S { F: _, .. }`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VariantKind {
    Unit,
    Tuple,
    Named,
}

/// A pattern lowered for checking. Every node carries the type it matches (after type
/// checking; references peeled by default binding modes are explicit [`PatKind::Deref`]
/// nodes).
#[derive(Clone, Debug)]
pub struct Pat {
    pub kind: PatKind,
    pub ty: Ty,
    pub span: Span,
}

/// The kinds of [`Pat`].
#[derive(Clone, Debug)]
pub enum PatKind {
    /// `_`, a binding `x` / `ref x` without subpattern, `..`-elided fields. A binding with
    /// a subpattern, `x @ p`, is lowered to just `p`.
    Wild,
    /// A struct, tuple, `()` or unit struct pattern: all fields, in declaration order
    /// (fields not mentioned in `S { a, .. }` are [`PatKind::Wild`]). For an enum,
    /// `variant` is the index of the variant.
    Ctor {
        variant: Option<usize>,
        fields: Vec<Pat>,
    },
    /// A pattern matched through a reference: the type is `&T`/`&mut T`, the inner pattern
    /// has type `T`.
    Deref(Box<Pat>),
    /// An integer or `char` literal or an inclusive range pattern `lo..=hi` (`a..b` is
    /// passed as `a..=b-1`). The values are the mathematical values, except for `u128`,
    /// whose values are passed as their bit pattern (`v as i128`, so `u128::MAX` is `-1`).
    /// For `char` they are the scalar values.
    Range { lo: i128, hi: i128 },
    /// A half-open range pattern, `lo..` (`hi: None`) or `..=hi` (`lo: None`), same value
    /// encoding as [`PatKind::Range`]. For every type but `isize` and `usize` this is the
    /// same as a `Range` up to the type's `MIN`/`MAX`; for those two, as in rustc, only an
    /// open end covers the values beyond `MIN`/`MAX` (`0..=usize::MAX` is not exhaustive,
    /// `0..` is).
    OpenRange { lo: Option<i128>, hi: Option<i128> },
    /// A `true` or `false` literal.
    Bool(bool),
    /// A string literal; its type is `&str` (or `str` under an explicit [`PatKind::Deref`]).
    /// Equal strings cover each other; strings never cover the type.
    Str(String),
    /// A float literal. Literals equal as floats (`0.0` and `-0.0` too) cover each other;
    /// floats never cover the type.
    Float(f64),
    /// A float range pattern (#49), `lo..=hi` or `lo..hi`, an open end infinite (`lo..`
    /// is `lo..=inf`): it covers the literals and ranges inside it by rustc's `F64Range`
    /// rule (a range ending at the same bound is covered only with the same end, so
    /// `0.0..1.0` is not covered by `0.0..=1.0`), but floats never cover the type.
    FloatRange { lo: f64, hi: f64, inclusive: bool },
    /// Any other literal value: it covers nothing but itself and never makes a match
    /// exhaustive. Of type `&str` it behaves like a `Str` pattern.
    Opaque,
    /// `p | q | ...`, in source order. Nested or-patterns are flattened, as in rustc.
    Or(Vec<Pat>),
}

/// One arm of a `match` (or the single pattern of a `let`, `if let` or parameter).
#[derive(Clone, Debug)]
pub struct Arm {
    pub pat: Pat,
    pub has_guard: bool,
}

/// The result of [`check_match`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// One entry per pattern (arm, or alternative of an or-pattern at any depth) that is
    /// unreachable, in source order: the span of that pattern. A whole arm that is
    /// unreachable reports only the arm's pattern span, not its alternatives.
    ///
    /// rustc emits the warnings in a different order: first every unreachable
    /// alternative of reachable arms, then the unreachable arms.
    pub unreachable: Vec<Span>,
    /// Witnesses of non-exhaustiveness (empty when exhaustive), formatted and ordered as
    /// rustc prints them in E0004/E0005.
    pub witnesses: Vec<String>,
}

/// Checks a `match` whose scrutinee place is known to hold a valid value: anything but a
/// dereference (`match *r`), see [`check_match_with_validity`].
///
/// A `let`, `if let`, `while let`, `let else` or parameter pattern is checked as a
/// one-arm match: it is irrefutable when `witnesses` is empty.
#[must_use]
pub fn check_match(cx: &dyn PatCx, scrut_ty: &Ty, arms: &[Arm]) -> Report {
    check_match_with_validity(cx, scrut_ty, arms, true)
}

/// [`check_match`], saying whether the scrutinee place is known to hold a valid value.
///
/// rustc treats a scrutinee that is a dereference (`match *r`, also through fields such as
/// `match (*r).0` and `match r.0`, where the field access dereferences `r` implicitly) as
/// possibly invalid: there an uninhabited variant must still be matched
/// (`match *r { E::A => .. }` for `enum E { A, B(Never) }` reports `E::B(_)`), whereas a
/// local, a field of a local, a call result or any other value expression is valid.
#[must_use]
pub fn check_match_with_validity(
    cx: &dyn PatCx,
    scrut_ty: &Ty,
    arms: &[Arm],
    scrut_known_valid: bool,
) -> Report {
    if ty_has_error(scrut_ty) {
        return Report::default();
    }
    let mut checker = Checker::new(cx);
    let roots: Vec<PatId> = arms.iter().map(|arm| checker.lower(&arm.pat)).collect();
    if checker.saw_error {
        return Report::default();
    }

    let mut matrix = Matrix {
        rows: Vec::new(),
        places: vec![Place {
            ty: scrut_ty.clone(),
            valid: scrut_known_valid,
            is_scrutinee: true,
        }],
        wildcard_row_is_relevant: true,
    };
    for (i, (&root, arm)) in roots.iter().zip(arms).enumerate() {
        checker.push_expanded(
            &mut matrix,
            Row {
                pats: vec![PatOrWild::Pat(root)],
                parent: i,
                under_guard: arm.has_guard,
                useful: false,
            },
        );
    }
    let witnesses = checker.compute(&mut matrix);

    let mut unreachable = Vec::new();
    for &root in &roots {
        if checker.pat_is_useful(root) {
            let mut redundant = Vec::new();
            checker.collect_redundant(root, &mut redundant);
            redundant.sort();
            unreachable.extend(redundant);
        } else {
            unreachable.push(checker.pats[root].span);
        }
    }

    let witnesses = witnesses
        .iter()
        .map(|stack| {
            debug_assert_eq!(stack.len(), 1);
            checker.print_witness(&stack[0])
        })
        .collect();
    Report {
        unreachable,
        witnesses,
    }
}

/// The message tail rustc uses after "non-exhaustive patterns: ": "`A` not covered",
/// "`A` and `B` not covered", "`A`, `B` and `C` not covered", "`A`, `B`, `C` and 2 more
/// not covered". `witnesses` must not be empty.
#[must_use]
pub fn joined_witnesses(witnesses: &[String]) -> String {
    format!("{} not covered", joined_list(witnesses))
}

/// rustc's primary label for E0004 and E0005: "pattern `A` not covered" for one witness,
/// "patterns `A` and `B` not covered" (and so on, as [`joined_witnesses`]) for several.
#[must_use]
pub fn witnesses_label(witnesses: &[String]) -> String {
    let noun = if witnesses.len() == 1 {
        "pattern"
    } else {
        "patterns"
    };
    format!("{noun} {}", joined_witnesses(witnesses))
}

/// Whether rustc words a non-exhaustive `match` as "non-exhaustive patterns: type `T` is
/// non-empty" instead of listing witnesses: the match has no arms and the scrutinee type
/// is not an enum with at least one variant (`i32`, a struct, `&Light`, `&Never`). Only
/// meaningful when [`Report::witnesses`] is not empty.
#[must_use]
pub fn reports_type_non_empty(cx: &dyn PatCx, scrut_ty: &Ty, arms: &[Arm]) -> bool {
    if !arms.is_empty() {
        return false;
    }
    let non_empty_enum = match scrut_ty {
        Ty::Adt(def, _) => {
            matches!(cx.adt(*def), AdtShape::Enum { variants, .. } if !variants.is_empty())
        }
        _ => false,
    };
    !non_empty_enum
}

fn joined_list(witnesses: &[String]) -> String {
    const LIMIT: usize = 3;
    match witnesses {
        [] => String::new(),
        [only] => format!("`{only}`"),
        [head @ .., tail] if head.len() < LIMIT => {
            format!("`{}` and `{tail}`", head.join("`, `"))
        }
        _ => {
            let (head, tail) = witnesses.split_at(LIMIT);
            format!("`{}` and {} more", head.join("`, `"), tail.len())
        }
    }
}

/// Whether the type mentions an error or inference type, through references and tuples.
fn ty_has_error(ty: &Ty) -> bool {
    match ty {
        Ty::Error | Ty::Infer(_) => true,
        Ty::Ref(_, inner) => ty_has_error(inner),
        Ty::Tuple(elems) => elems.iter().any(ty_has_error),
        Ty::Array(elem, _) | Ty::Slice(elem) => ty_has_error(elem),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------
// Integer ranges
// ---------------------------------------------------------------------------------------

/// An integer boundary. Values are biased so that every type's values are ordered as
/// unsigned `u128`s: a signed value `v` of `n` bits is stored as `v + 2^(n-1)`.
/// `JustAfterMax` is one past `u128::MAX` (the end of an inclusive range ending at a
/// 128-bit type's maximum); the infinities bound the ranges of `isize` and `usize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MaybeInf {
    NegInf,
    Finite(u128),
    JustAfterMax,
    PosInf,
}

impl MaybeInf {
    fn plus_one(self) -> Option<Self> {
        match self {
            Self::Finite(n) => Some(n.checked_add(1).map_or(Self::JustAfterMax, Self::Finite)),
            Self::JustAfterMax => None,
            x => Some(x),
        }
    }

    fn minus_one(self) -> Option<Self> {
        match self {
            Self::Finite(n) => n.checked_sub(1).map(Self::Finite),
            x => Some(x),
        }
    }
}

/// A half-open range `[lo, hi)` of biased integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IntRange {
    lo: MaybeInf,
    hi: MaybeInf,
}

impl IntRange {
    /// The inclusive range `lo..=hi`.
    fn inclusive(lo: MaybeInf, hi: MaybeInf) -> Self {
        let hi = hi.plus_one().unwrap_or(MaybeInf::PosInf);
        Self { lo, hi }
    }

    fn is_singleton(self) -> bool {
        self.lo.plus_one() == Some(self.hi)
    }

    fn is_subrange(self, other: Self) -> bool {
        other.lo <= self.lo && self.hi <= other.hi
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let lo = self.lo.max(other.lo);
        let hi = self.hi.min(other.hi);
        (lo < hi).then_some(Self { lo, hi })
    }

    /// Splits `self` at every boundary of `seen` (intersected with `self`), saying for each
    /// piece whether a seen range covers it. Unseen pieces are maximal.
    fn split(self, seen: &[IntRange]) -> Vec<(bool, IntRange)> {
        let mut boundaries: Vec<(MaybeInf, isize)> = seen
            .iter()
            .filter_map(|r| self.intersection(*r))
            .flat_map(|r| [(r.lo, 1), (r.hi, -1)])
            .collect();
        boundaries.sort_unstable();
        let mut out = Vec::new();
        let mut count = 0isize;
        let mut prev = self.lo;
        for (bdy, delta) in boundaries.into_iter().chain([(self.hi, 0)]) {
            if prev != bdy {
                out.push((count > 0, IntRange { lo: prev, hi: bdy }));
            }
            prev = bdy;
            count += delta;
        }
        out
    }
}

/// The integer facts of a primitive integer type.
#[derive(Clone, Copy, Debug)]
struct IntTy {
    signed: bool,
    bits: u32,
    /// `isize`/`usize`: their minimum and maximum are not observable, so their range is
    /// unbounded (rustc's rule).
    ptr_sized: bool,
    name: &'static str,
}

fn mask(bits: u32) -> u128 {
    if bits >= 128 {
        u128::MAX
    } else {
        (1u128 << bits) - 1
    }
}

impl IntTy {
    fn of(prim: PrimTy) -> Option<Self> {
        let (signed, bits, ptr_sized) = match prim {
            PrimTy::I8 => (true, 8, false),
            PrimTy::I16 => (true, 16, false),
            PrimTy::I32 => (true, 32, false),
            PrimTy::I64 => (true, 64, false),
            PrimTy::I128 => (true, 128, false),
            PrimTy::Isize => (true, 64, true),
            PrimTy::U8 => (false, 8, false),
            PrimTy::U16 => (false, 16, false),
            PrimTy::U32 => (false, 32, false),
            PrimTy::U64 => (false, 64, false),
            PrimTy::U128 => (false, 128, false),
            PrimTy::Usize => (false, 64, true),
            _ => return None,
        };
        Some(Self {
            signed,
            bits,
            ptr_sized,
            name: prim.name(),
        })
    }

    fn bias(self) -> u128 {
        if self.signed {
            1u128 << (self.bits - 1)
        } else {
            0
        }
    }

    /// The biased encoding of a value as [`PatKind::Range`] passes it.
    #[allow(clippy::cast_sign_loss)] // two's complement reinterpretation, then the bias
    fn encode(self, value: i128) -> u128 {
        (value as u128).wrapping_add(self.bias())
    }

    /// The values of the type.
    fn range(self) -> IntRange {
        if self.ptr_sized {
            let lo = if self.signed {
                MaybeInf::NegInf
            } else {
                MaybeInf::Finite(0)
            };
            IntRange {
                lo,
                hi: MaybeInf::PosInf,
            }
        } else {
            IntRange::inclusive(MaybeInf::Finite(0), MaybeInf::Finite(mask(self.bits)))
        }
    }

    /// Prints a (biased) value as rustc prints a typed constant: `5_u8`, `-1_i32`,
    /// `i32::MIN`, `u8::MAX`, `0_usize`.
    #[allow(clippy::cast_possible_wrap)] // two's complement reinterpretation
    fn print(self, biased: u128) -> String {
        let name = self.name;
        if self.signed {
            let value = biased.wrapping_sub(self.bias()) as i128;
            let max = mask(self.bits - 1) as i128;
            if value == max {
                format!("{name}::MAX")
            } else if value == -max - 1 {
                format!("{name}::MIN")
            } else {
                format!("{value}_{name}")
            }
        } else if biased == mask(self.bits) {
            format!("{name}::MAX")
        } else {
            format!("{biased}_{name}")
        }
    }
}

/// The integer-like types: integers and `char`.
#[derive(Clone, Copy, Debug)]
enum IntKind {
    Int(IntTy),
    Char,
}

impl IntKind {
    fn of(ty: &Ty) -> Option<Self> {
        match ty {
            Ty::Prim(PrimTy::Char) => Some(Self::Char),
            Ty::Prim(p) => IntTy::of(*p).map(Self::Int),
            _ => None,
        }
    }

    #[allow(clippy::cast_sign_loss)] // a `char` scalar value is never negative
    fn encode(self, value: i128) -> u128 {
        match self {
            Self::Int(int) => int.encode(value),
            Self::Char => value as u128,
        }
    }

    /// A finite boundary of a witness range, or `PosInf` when it does not fit the type
    /// (the end of an unbounded `usize`/`isize` range): rustc's `hoist_pat_range_bdy`.
    fn hoist(self, bdy: MaybeInf) -> MaybeInf {
        match bdy {
            MaybeInf::NegInf => MaybeInf::NegInf,
            MaybeInf::Finite(n) => {
                let bits = match self {
                    Self::Int(int) => int.bits,
                    Self::Char => 32,
                };
                // The unbiased bits must fit the type's size; the bias only flips their top
                // bit, so the biased value fits exactly when they do (rustc unbiases first).
                if n <= mask(bits) {
                    bdy
                } else {
                    MaybeInf::PosInf
                }
            }
            MaybeInf::JustAfterMax | MaybeInf::PosInf => MaybeInf::PosInf,
        }
    }

    fn max(self) -> u128 {
        match self {
            Self::Int(int) => mask(int.bits),
            Self::Char => 0x10_FFFF,
        }
    }

    fn print_value(self, biased: u128) -> String {
        match self {
            Self::Int(int) => int.print(biased),
            Self::Char => match u32::try_from(biased).ok().and_then(char::from_u32) {
                Some(c) => format!("{c:?}"),
                None => format!("'\\u{{{biased:x}}}'"),
            },
        }
    }

    /// Prints a witness range as rustc does: `_` for the whole of an unbounded type, a
    /// single value, or an inclusive range (exclusive only for `..isize::MIN`), with open
    /// ends for unbounded types.
    fn print_range(self, range: IntRange) -> String {
        if range.lo == MaybeInf::NegInf && range.hi == MaybeInf::PosInf {
            return "_".to_owned();
        }
        if range.is_singleton() {
            if let MaybeInf::Finite(n) = range.lo {
                return self.print_value(n);
            }
        }
        let mut lo = self.hoist(range.lo);
        if lo == MaybeInf::PosInf {
            lo = MaybeInf::Finite(self.max());
        }
        let (hi, exclusive) = match range.hi.minus_one() {
            Some(hi) => (hi, false),
            None => (range.hi, true),
        };
        let hi = self.hoist(hi);
        let mut out = String::new();
        if let MaybeInf::Finite(n) = lo {
            out.push_str(&self.print_value(n));
        }
        if let MaybeInf::Finite(n) = hi {
            out.push_str(if exclusive { ".." } else { "..=" });
            out.push_str(&self.print_value(n));
        } else {
            out.push_str("..");
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------------------

type PatId = usize;

/// A constructor: what a pattern node tests, or a value class a witness names.
#[derive(Clone, Debug, PartialEq)]
enum Ctor {
    /// The single constructor of a struct, tuple, `()` (and of error-typed places).
    Struct,
    Variant(usize),
    /// `&p`.
    Ref,
    Bool(bool),
    IntRange(IntRange),
    Str(String),
    /// A float literal (`lo == hi`, inclusive) or range, as rustc's `F64Range`.
    FloatRange(f64, f64, bool),
    /// Covers nothing but the pattern it came from.
    Opaque(u32),
    /// An or-pattern; its fields are the alternatives. Never a matrix head.
    Or,
    Wildcard,
    /// The values of a type that cannot be listed (floats, strings); printed `_`.
    NonExhaustive,
    /// The (absent) constructor of an empty type; printed `_`.
    Never,
    /// Stands for every constructor of the column that no row mentions.
    Missing,
}

impl Ctor {
    /// Whether every value `self` stands for is matched by a pattern with constructor
    /// `other`.
    #[allow(clippy::float_cmp)] // float literal patterns compare as floats, as in rustc
    fn is_covered_by(&self, other: &Ctor) -> bool {
        match (self, other) {
            (_, Ctor::Wildcard) | (Ctor::Struct, Ctor::Struct) | (Ctor::Ref, Ctor::Ref) => true,
            (Ctor::Variant(a), Ctor::Variant(b)) => a == b,
            (Ctor::Bool(a), Ctor::Bool(b)) => a == b,
            (Ctor::IntRange(a), Ctor::IntRange(b)) => a.is_subrange(*b),
            (Ctor::Str(a), Ctor::Str(b)) => a == b,
            (Ctor::FloatRange(lo, hi, inclusive), Ctor::FloatRange(o_lo, o_hi, o_inclusive)) => {
                // rustc's `is_covered_by` for float ranges: with equal upper bounds the
                // ends must be equal too, so rustc (1.99) takes `0.0..1.0` as not covered by
                // `0.0..=1.0`.
                lo >= o_lo
                    && match hi.partial_cmp(o_hi) {
                        Some(std::cmp::Ordering::Less) => true,
                        Some(std::cmp::Ordering::Equal) => inclusive == o_inclusive,
                        _ => false,
                    }
            }
            (Ctor::Opaque(a), Ctor::Opaque(b)) => a == b,
            _ => false,
        }
    }
}

/// Whether a variant (or struct) has values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Visible,
    Empty,
}

/// The constructors of a type.
#[derive(Clone, Debug)]
enum CtorSet {
    Struct {
        empty: bool,
    },
    Variants(Vec<Visibility>),
    Ref,
    Bool,
    Integers {
        range_1: IntRange,
        range_2: Option<IntRange>,
    },
    /// Floats and strings: only a wildcard covers them.
    Unlistable,
    /// `!` and empty enums.
    NoCtors,
}

/// The constructors of a column split by what its rows mention.
#[derive(Debug, Default)]
struct SplitSet {
    present: Vec<Ctor>,
    missing: Vec<Ctor>,
    /// Missing constructors without values (uninhabited variants or structs).
    missing_empty: Vec<Ctor>,
}

// ---------------------------------------------------------------------------------------
// Patterns, matrices and witnesses
// ---------------------------------------------------------------------------------------

/// A lowered pattern node; `fields` are its subpatterns (the alternatives for [`Ctor::Or`]).
#[derive(Clone, Debug)]
struct DPat {
    ctor: Ctor,
    fields: Vec<PatId>,
    span: Span,
}

#[derive(Clone, Copy, Debug)]
enum PatOrWild {
    /// A wildcard introduced by specializing a wildcard.
    Wild,
    Pat(PatId),
}

#[derive(Clone, Debug)]
struct Row {
    /// The patterns, the head column last.
    pats: Vec<PatOrWild>,
    /// The row of the parent matrix this row was specialized from (the arm, at the top).
    parent: usize,
    under_guard: bool,
    useful: bool,
}

/// What the checker knows about a column.
#[derive(Clone, Debug)]
struct Place {
    ty: Ty,
    /// Whether the place is known to hold a valid value: false behind a reference.
    valid: bool,
    is_scrutinee: bool,
}

#[derive(Debug)]
struct Matrix {
    rows: Vec<Row>,
    /// The columns, the head column last.
    places: Vec<Place>,
    /// False when the witnesses this matrix would report are already reported through
    /// another branch (a present constructor of a column that also has missing ones).
    wildcard_row_is_relevant: bool,
}

/// A witness pattern.
#[derive(Clone, Debug)]
struct Wit {
    ctor: Ctor,
    ty: Ty,
    fields: Vec<Wit>,
}

/// One witness under construction: one pattern per column, the head column last.
type WitStack = Vec<Wit>;

struct Checker<'a> {
    cx: &'a dyn PatCx,
    adts: HashMap<DefId, Rc<AdtShape>>,
    uninhabited: HashMap<DefId, bool>,
    pats: Vec<DPat>,
    useful: Vec<bool>,
    next_opaque: u32,
    saw_error: bool,
}

impl<'a> Checker<'a> {
    fn new(cx: &'a dyn PatCx) -> Self {
        Self {
            cx,
            adts: HashMap::new(),
            uninhabited: HashMap::new(),
            pats: Vec::new(),
            useful: Vec::new(),
            next_opaque: 0,
            saw_error: false,
        }
    }

    fn adt(&mut self, def: DefId) -> Rc<AdtShape> {
        let cx = self.cx;
        Rc::clone(self.adts.entry(def).or_insert_with(|| Rc::new(cx.adt(def))))
    }

    /// The field type `ty` of `def` at its instance with arguments `args`.
    fn instance_field(&self, def: DefId, ty: &Ty, args: &[Ty]) -> Ty {
        if args.is_empty() {
            return ty.clone();
        }
        ty.subst(&self.cx.generics(def), args)
    }

    // --- types -------------------------------------------------------------------------

    /// Whether the type has no values, as rustc decides it: references are always
    /// inhabited.
    fn is_uninhabited(&mut self, ty: &Ty) -> bool {
        match ty {
            Ty::Never => true,
            Ty::Tuple(elems) => elems.iter().any(|e| self.is_uninhabited(e)),
            Ty::Adt(def, _) => {
                if let Some(&known) = self.uninhabited.get(def) {
                    return known;
                }
                // A type that reaches itself by value is reported elsewhere; treat the
                // cycle as inhabited.
                self.uninhabited.insert(*def, false);
                let shape = self.adt(*def);
                let empty = match &*shape {
                    AdtShape::Struct { fields, .. } => {
                        fields.iter().any(|f| self.is_uninhabited(&f.ty))
                    }
                    AdtShape::Enum { variants, .. } => variants
                        .iter()
                        .all(|v| v.fields.iter().any(|f| self.is_uninhabited(&f.ty))),
                };
                self.uninhabited.insert(*def, empty);
                empty
            }
            _ => false,
        }
    }

    fn ctors_for_ty(&mut self, ty: &Ty) -> CtorSet {
        match ty {
            Ty::Prim(PrimTy::Bool) => CtorSet::Bool,
            Ty::Prim(PrimTy::Char) => CtorSet::Integers {
                range_1: IntRange::inclusive(MaybeInf::Finite(0), MaybeInf::Finite(0xD7FF)),
                range_2: Some(IntRange::inclusive(
                    MaybeInf::Finite(0xE000),
                    MaybeInf::Finite(0x10_FFFF),
                )),
            },
            Ty::Prim(p) => match IntTy::of(*p) {
                Some(int) => CtorSet::Integers {
                    range_1: int.range(),
                    range_2: None,
                },
                None => CtorSet::Unlistable,
            },
            Ty::Tuple(_) => CtorSet::Struct {
                empty: self.is_uninhabited(ty),
            },
            // No array or slice patterns yet: only wildcards and bindings match them.
            Ty::Array(..) | Ty::Slice(_) => CtorSet::Unlistable,
            Ty::Adt(def, _) => {
                let shape = self.adt(*def);
                match &*shape {
                    AdtShape::Struct { .. } => CtorSet::Struct {
                        empty: self.is_uninhabited(ty),
                    },
                    AdtShape::Enum { variants, .. } if variants.is_empty() => CtorSet::NoCtors,
                    AdtShape::Enum { variants, .. } => CtorSet::Variants(
                        variants
                            .iter()
                            .map(|v| {
                                if v.fields.iter().any(|f| self.is_uninhabited(&f.ty)) {
                                    Visibility::Empty
                                } else {
                                    Visibility::Visible
                                }
                            })
                            .collect(),
                    ),
                }
            }
            Ty::Ref(..) => CtorSet::Ref,
            Ty::Never => CtorSet::NoCtors,
            // A type parameter's values are opaque: only a wildcard or a binding matches
            // one, as for a struct without fields.
            Ty::Unit | Ty::FnDef(_) | Ty::Param(_) | Ty::Infer(_) | Ty::Error => {
                CtorSet::Struct { empty: false }
            }
        }
    }

    /// The types of the fields of `ctor` at type `ty`.
    fn field_tys(&mut self, ty: &Ty, ctor: &Ctor) -> Vec<Ty> {
        match (ctor, ty) {
            (Ctor::Struct, Ty::Tuple(elems)) => elems.to_vec(),
            (Ctor::Struct, Ty::Adt(def, args)) => match &*self.adt(*def) {
                AdtShape::Struct { fields, .. } => fields
                    .iter()
                    .map(|f| self.instance_field(*def, &f.ty, args))
                    .collect(),
                AdtShape::Enum { .. } => Vec::new(),
            },
            (Ctor::Variant(i), Ty::Adt(def, args)) => match &*self.adt(*def) {
                AdtShape::Enum { variants, .. } => variants
                    .get(*i)
                    .map(|v| {
                        v.fields
                            .iter()
                            .map(|f| self.instance_field(*def, &f.ty, args))
                            .collect()
                    })
                    .unwrap_or_default(),
                AdtShape::Struct { .. } => Vec::new(),
            },
            (Ctor::Ref, Ty::Ref(_, inner)) => vec![(**inner).clone()],
            _ => Vec::new(),
        }
    }

    // --- lowering ----------------------------------------------------------------------

    fn alloc(&mut self, ctor: Ctor, fields: Vec<PatId>, span: Span) -> PatId {
        self.pats.push(DPat { ctor, fields, span });
        self.useful.push(false);
        self.pats.len() - 1
    }

    fn fresh_opaque(&mut self) -> Ctor {
        self.next_opaque += 1;
        Ctor::Opaque(self.next_opaque)
    }

    fn lower(&mut self, pat: &Pat) -> PatId {
        if ty_has_error(&pat.ty) {
            self.saw_error = true;
        }
        let span = pat.span;
        match &pat.kind {
            PatKind::Wild => self.alloc(Ctor::Wildcard, Vec::new(), span),
            PatKind::Ctor { variant, fields } => {
                let ctor = match (variant, &pat.ty) {
                    (Some(i), Ty::Adt(def, _))
                        if matches!(&*self.adt(*def), AdtShape::Enum { .. }) =>
                    {
                        Ctor::Variant(*i)
                    }
                    _ => Ctor::Struct,
                };
                let arity = self.field_tys(&pat.ty, &ctor).len();
                let mut ids: Vec<PatId> = fields.iter().map(|f| self.lower(f)).collect();
                ids.truncate(arity);
                while ids.len() < arity {
                    let wild = self.alloc(Ctor::Wildcard, Vec::new(), span);
                    ids.push(wild);
                }
                self.alloc(ctor, ids, span)
            }
            PatKind::Deref(inner) => {
                let inner = self.lower(inner);
                self.alloc(Ctor::Ref, vec![inner], span)
            }
            PatKind::Range { lo, hi } => {
                let ctor = match IntKind::of(&pat.ty) {
                    Some(kind) => Ctor::IntRange(IntRange::inclusive(
                        MaybeInf::Finite(kind.encode(*lo)),
                        MaybeInf::Finite(kind.encode(*hi)),
                    )),
                    None => self.fresh_opaque(),
                };
                self.alloc(ctor, Vec::new(), span)
            }
            PatKind::OpenRange { lo, hi } => {
                let ctor = match IntKind::of(&pat.ty) {
                    Some(kind) => {
                        let lo = lo.map_or(MaybeInf::NegInf, |v| MaybeInf::Finite(kind.encode(v)));
                        let hi = hi.map_or(MaybeInf::PosInf, |v| MaybeInf::Finite(kind.encode(v)));
                        Ctor::IntRange(IntRange::inclusive(lo, hi))
                    }
                    None => self.fresh_opaque(),
                };
                self.alloc(ctor, Vec::new(), span)
            }
            PatKind::Bool(b) => self.alloc(Ctor::Bool(*b), Vec::new(), span),
            PatKind::Str(s) => self.lower_through_ref(&pat.ty, Ctor::Str(s.clone()), span),
            PatKind::Float(f) => self.alloc(Ctor::FloatRange(*f, *f, true), Vec::new(), span),
            PatKind::FloatRange { lo, hi, inclusive } => {
                self.alloc(Ctor::FloatRange(*lo, *hi, *inclusive), Vec::new(), span)
            }
            PatKind::Opaque => {
                let ctor = self.fresh_opaque();
                self.lower_through_ref(&pat.ty, ctor, span)
            }
            PatKind::Or(alts) => {
                let mut leaves = Vec::new();
                flatten_or(alts, &mut leaves);
                let ids = leaves.into_iter().map(|p| self.lower(p)).collect();
                self.alloc(Ctor::Or, ids, span)
            }
        }
    }

    /// A string-like literal of type `&str` is a `&` pattern around the literal, as in
    /// rustc, so that it composes with explicit `&` patterns.
    fn lower_through_ref(&mut self, ty: &Ty, ctor: Ctor, span: Span) -> PatId {
        let inner = self.alloc(ctor, Vec::new(), span);
        if matches!(ty, Ty::Ref(..)) {
            self.alloc(Ctor::Ref, vec![inner], span)
        } else {
            inner
        }
    }

    // --- the algorithm -----------------------------------------------------------------

    fn head_ctor(&self, row: &Row) -> Ctor {
        match row.pats.last() {
            Some(PatOrWild::Pat(id)) => self.pats[*id].ctor.clone(),
            _ => Ctor::Wildcard,
        }
    }

    /// Pushes `row`, expanding an or-pattern at its head into one row per alternative.
    fn push_expanded(&self, matrix: &mut Matrix, row: Row) {
        if let Some(&PatOrWild::Pat(id)) = row.pats.last() {
            if self.pats[id].ctor == Ctor::Or {
                for &alt in &self.pats[id].fields {
                    let mut new_row = row.clone();
                    *new_row.pats.last_mut().expect("the row has a head") = PatOrWild::Pat(alt);
                    self.push_expanded(matrix, new_row);
                }
                return;
            }
        }
        matrix.rows.push(row);
    }

    /// Computes the witnesses of the rows not covering the matrix's columns, and records
    /// which rows (and which head patterns) are useful.
    fn compute(&mut self, matrix: &mut Matrix) -> Vec<WitStack> {
        let Some(place) = matrix.places.last().cloned() else {
            // No columns: a row is useful iff no unguarded row is above it.
            let mut useful = true;
            for row in &mut matrix.rows {
                row.useful = useful;
                useful &= row.under_guard;
            }
            return if useful && matrix.wildcard_row_is_relevant {
                vec![Vec::new()]
            } else {
                Vec::new()
            };
        };

        let heads: Vec<Ctor> = matrix.rows.iter().map(|r| self.head_ctor(r)).collect();
        let (split_ctors, missing_ctors) = self.split_column(&place, &heads);

        let mut ret = Vec::new();
        for ctor in split_ctors {
            // A present constructor is irrelevant when some constructor is missing: the
            // witnesses it would find are found through `Missing` too.
            let relevant = ctor == Ctor::Missing || missing_ctors.is_empty();
            let mut spec = self.specialize(matrix, &place, &ctor, relevant);
            let mut witnesses = self.compute(&mut spec);
            self.apply_ctor(&mut witnesses, &place.ty, &missing_ctors, &ctor);
            ret.extend(witnesses);
            for child in &spec.rows {
                if child.useful {
                    matrix.rows[child.parent].useful = true;
                }
            }
        }

        for row in &matrix.rows {
            if row.useful {
                if let Some(PatOrWild::Pat(id)) = row.pats.last() {
                    self.useful[*id] = true;
                }
            }
        }
        ret
    }

    /// The constructors to specialize the column with (present ones, then `Missing` if
    /// anything is missing), and the missing constructors witnesses should name.
    fn split_column(&mut self, place: &Place, heads: &[Ctor]) -> (Vec<Ctor>, Vec<Ctor>) {
        let set = self.ctors_for_ty(&place.ty);
        let is_toplevel_exception = place.is_scrutinee && matches!(set, CtorSet::NoCtors);
        // An arm on an empty type is only unreachable for a `!`/empty-enum scrutinee.
        let empty_arms_are_unreachable = place.valid && is_toplevel_exception;
        // Empty constructors may be omitted where the value is known valid.
        let can_omit_empty_arms = place.valid || is_toplevel_exception;

        let split = split_set(&set, heads);
        let all_missing = split.present.is_empty();
        let mut split_ctors = split.present;
        if !(split.missing.is_empty()
            && (split.missing_empty.is_empty() || empty_arms_are_unreachable))
        {
            split_ctors.push(Ctor::Missing);
        }

        let mut missing_ctors = split.missing;
        if !can_omit_empty_arms {
            missing_ctors.extend(split.missing_empty);
        }
        // List the missing constructors at the scrutinee and next to present ones;
        // elsewhere a column that mentions nothing is reported as `_`.
        let report_individual = place.is_scrutinee || !all_missing;
        if !missing_ctors.is_empty() && !report_individual {
            missing_ctors = vec![Ctor::Wildcard];
        } else if missing_ctors.contains(&Ctor::NonExhaustive) {
            missing_ctors = vec![Ctor::NonExhaustive];
        }
        (split_ctors, missing_ctors)
    }

    fn specialize(
        &mut self,
        matrix: &Matrix,
        place: &Place,
        ctor: &Ctor,
        relevant: bool,
    ) -> Matrix {
        let field_tys = self.field_tys(&place.ty, ctor);
        let arity = field_tys.len();
        // The validity of `x: &T` says nothing about `*x`.
        let valid = place.valid && *ctor != Ctor::Ref;
        let rest = matrix.places.len() - 1;
        let mut places = matrix.places[..rest].to_vec();
        places.extend(field_tys.into_iter().rev().map(|ty| Place {
            ty,
            valid,
            is_scrutinee: false,
        }));
        let mut spec = Matrix {
            rows: Vec::new(),
            places,
            wildcard_row_is_relevant: matrix.wildcard_row_is_relevant && relevant,
        };
        for (i, row) in matrix.rows.iter().enumerate() {
            let head = row.pats[rest];
            let head_ctor = self.head_ctor(row);
            if !ctor.is_covered_by(&head_ctor) {
                continue;
            }
            let mut pats = row.pats[..rest].to_vec();
            match head {
                PatOrWild::Pat(id) if head_ctor != Ctor::Wildcard => {
                    pats.extend(
                        self.pats[id]
                            .fields
                            .iter()
                            .rev()
                            .map(|&f| PatOrWild::Pat(f)),
                    );
                }
                _ => pats.extend(std::iter::repeat_n(PatOrWild::Wild, arity)),
            }
            self.push_expanded(
                &mut spec,
                Row {
                    pats,
                    parent: i,
                    under_guard: row.under_guard,
                    useful: false,
                },
            );
        }
        spec
    }

    /// Turns witnesses of a specialized matrix into witnesses of its parent.
    fn apply_ctor(
        &mut self,
        witnesses: &mut Vec<WitStack>,
        ty: &Ty,
        missing: &[Ctor],
        ctor: &Ctor,
    ) {
        if witnesses.is_empty() {
            return;
        }
        if *ctor == Ctor::Missing {
            let mut ret = Vec::new();
            for missing_ctor in missing {
                let fields = self
                    .field_tys(ty, missing_ctor)
                    .into_iter()
                    .map(|ty| Wit {
                        ctor: Ctor::Wildcard,
                        ty,
                        fields: Vec::new(),
                    })
                    .collect();
                let pat = Wit {
                    ctor: missing_ctor.clone(),
                    ty: ty.clone(),
                    fields,
                };
                for stack in witnesses.iter() {
                    let mut stack = stack.clone();
                    stack.push(pat.clone());
                    ret.push(stack);
                }
            }
            *witnesses = ret;
        } else {
            let arity = self.field_tys(ty, ctor).len();
            for stack in witnesses.iter_mut() {
                let fields = stack.drain(stack.len() - arity..).rev().collect();
                stack.push(Wit {
                    ctor: ctor.clone(),
                    ty: ty.clone(),
                    fields,
                });
            }
        }
    }

    // --- reachability ------------------------------------------------------------------

    fn pat_is_useful(&self, id: PatId) -> bool {
        self.useful[id]
            || (self.pats[id].ctor == Ctor::Or
                && self.pats[id]
                    .fields
                    .iter()
                    .any(|&alt| self.pat_is_useful(alt)))
    }

    /// The maximal unreachable subpatterns of a useful pattern.
    fn collect_redundant(&self, id: PatId, out: &mut Vec<Span>) {
        if self.pat_is_useful(id) {
            for &field in &self.pats[id].fields {
                self.collect_redundant(field, out);
            }
        } else {
            out.push(self.pats[id].span);
        }
    }

    // --- printing ----------------------------------------------------------------------

    fn print_witness(&mut self, wit: &Wit) -> String {
        match &wit.ctor {
            Ctor::Bool(b) => b.to_string(),
            Ctor::IntRange(range) => match IntKind::of(&wit.ty) {
                Some(kind) => kind.print_range(*range),
                None => "_".to_owned(),
            },
            Ctor::Struct | Ctor::Variant(_) => self.print_struct_like(wit),
            Ctor::Ref => {
                let prefix = match &wit.ty {
                    Ty::Ref(m, _) => m.prefix_str(),
                    _ => "",
                };
                let inner = wit
                    .fields
                    .first()
                    .map_or_else(|| "_".to_owned(), |f| self.print_witness(f));
                format!("&{prefix}{inner}")
            }
            _ => "_".to_owned(),
        }
    }

    fn print_struct_like(&mut self, wit: &Wit) -> String {
        let fields: Vec<(String, bool)> = wit
            .fields
            .iter()
            .map(|f| (self.print_witness(f), prints_as_wildcard(f)))
            .collect();
        let (name, kind, field_names): (String, VariantKind, Vec<String>) =
            match (&wit.ty, &wit.ctor) {
                (Ty::Adt(def, _), ctor) => match (&*self.adt(*def), ctor) {
                    (AdtShape::Enum { name, variants }, Ctor::Variant(i)) => match variants.get(*i)
                    {
                        Some(v) => (
                            format!("{name}::{}", v.name),
                            v.kind,
                            v.fields.iter().map(|f| f.name.clone()).collect(),
                        ),
                        None => return "_".to_owned(),
                    },
                    (AdtShape::Struct { name, fields, kind }, _) => (
                        name.clone(),
                        *kind,
                        fields.iter().map(|f| f.name.clone()).collect(),
                    ),
                    (AdtShape::Enum { .. }, _) => return "_".to_owned(),
                },
                (Ty::Unit, _) => return "()".to_owned(),
                (Ty::Tuple(elems), _) => {
                    let list: Vec<&str> = fields.iter().map(|(s, _)| s.as_str()).collect();
                    let comma = if elems.len() == 1 { "," } else { "" };
                    return format!("({}{comma})", list.join(", "));
                }
                _ => return "_".to_owned(),
            };
        match kind {
            VariantKind::Named => {
                let mut parts: Vec<String> = field_names
                    .iter()
                    .zip(&fields)
                    .filter(|(_, (_, wild))| !wild)
                    .map(|(fname, (s, _))| format!("{fname}: {s}"))
                    .collect();
                if parts.len() < field_names.len() {
                    parts.push("..".to_owned());
                }
                format!("{name} {{ {} }}", parts.join(", "))
            }
            VariantKind::Tuple | VariantKind::Unit => {
                if fields.is_empty() {
                    name
                } else {
                    let list: Vec<&str> = fields.iter().map(|(s, _)| s.as_str()).collect();
                    format!("{name}({})", list.join(", "))
                }
            }
        }
    }
}

/// Whether a witness of a field prints as `_` (and is therefore left out of `S { .. }`).
/// The whole range of an `isize` or `usize` also prints as `_`, but it is a witness only
/// at the scrutinee: a field's column that mentions no range is reported as a wildcard.
fn prints_as_wildcard(wit: &Wit) -> bool {
    matches!(wit.ctor, Ctor::Wildcard | Ctor::NonExhaustive | Ctor::Never)
}

fn flatten_or<'p>(alts: &'p [Pat], out: &mut Vec<&'p Pat>) {
    for alt in alts {
        if let PatKind::Or(inner) = &alt.kind {
            flatten_or(inner, out);
        } else {
            out.push(alt);
        }
    }
}

/// Splits the constructors of a type by the constructors heading a column's rows.
fn split_set(set: &CtorSet, heads: &[Ctor]) -> SplitSet {
    let mut out = SplitSet::default();
    let mut seen: Vec<&Ctor> = Vec::new();
    for ctor in heads {
        match ctor {
            Ctor::Opaque(_) => out.present.push(ctor.clone()),
            Ctor::Wildcard => {}
            _ => seen.push(ctor),
        }
    }
    match set {
        CtorSet::Struct { empty } => {
            if !seen.is_empty() {
                out.present.push(Ctor::Struct);
            } else if *empty {
                out.missing_empty.push(Ctor::Struct);
            } else {
                out.missing.push(Ctor::Struct);
            }
        }
        CtorSet::Ref => {
            if seen.is_empty() {
                out.missing.push(Ctor::Ref);
            } else {
                out.present.push(Ctor::Ref);
            }
        }
        CtorSet::Variants(variants) => {
            let mut seen_set = vec![false; variants.len()];
            for ctor in &seen {
                if let Ctor::Variant(i) = ctor {
                    if let Some(slot) = seen_set.get_mut(*i) {
                        *slot = true;
                    }
                }
            }
            for (i, vis) in variants.iter().enumerate() {
                let ctor = Ctor::Variant(i);
                if seen_set[i] {
                    out.present.push(ctor);
                } else if *vis == Visibility::Empty {
                    out.missing_empty.push(ctor);
                } else {
                    out.missing.push(ctor);
                }
            }
        }
        CtorSet::Bool => {
            // rustc lists `true` before `false`.
            for b in [true, false] {
                if seen.iter().any(|c| **c == Ctor::Bool(b)) {
                    out.present.push(Ctor::Bool(b));
                } else {
                    out.missing.push(Ctor::Bool(b));
                }
            }
        }
        CtorSet::Integers { range_1, range_2 } => {
            let seen_ranges: Vec<IntRange> = seen
                .iter()
                .filter_map(|c| match c {
                    Ctor::IntRange(r) => Some(*r),
                    _ => None,
                })
                .collect();
            for range in std::iter::once(range_1).chain(range_2) {
                for (present, piece) in range.split(&seen_ranges) {
                    if present {
                        out.present.push(Ctor::IntRange(piece));
                    } else {
                        out.missing.push(Ctor::IntRange(piece));
                    }
                }
            }
        }
        CtorSet::Unlistable => {
            for ctor in seen {
                if !out.present.contains(ctor) {
                    out.present.push(ctor.clone());
                }
            }
            out.missing.push(Ctor::NonExhaustive);
        }
        CtorSet::NoCtors => out.missing_empty.push(Ctor::Never),
    }
    out
}

#[cfg(test)]
mod tests {
    use sabiko_span::FileId;

    use super::*;
    use crate::ty::Mutability;

    // --- a fake type context ---------------------------------------------------------

    struct FakeCx {
        adts: Vec<AdtShape>,
    }

    impl PatCx for FakeCx {
        fn adt(&self, def: DefId) -> AdtShape {
            self.adts[def.0 as usize].clone()
        }
    }

    fn fields(list: &[(&str, Ty)]) -> Vec<FieldShape> {
        list.iter()
            .map(|(name, ty)| FieldShape {
                name: (*name).to_owned(),
                ty: ty.clone(),
            })
            .collect()
    }

    fn tuple_fields(tys: &[Ty]) -> Vec<FieldShape> {
        tys.iter()
            .enumerate()
            .map(|(i, ty)| FieldShape {
                name: i.to_string(),
                ty: ty.clone(),
            })
            .collect()
    }

    fn unit(name: &str) -> VariantShape {
        VariantShape {
            name: name.to_owned(),
            kind: VariantKind::Unit,
            fields: Vec::new(),
        }
    }

    fn tuple_variant(name: &str, tys: &[Ty]) -> VariantShape {
        VariantShape {
            name: name.to_owned(),
            kind: VariantKind::Tuple,
            fields: tuple_fields(tys),
        }
    }

    fn named_variant(name: &str, list: &[(&str, Ty)]) -> VariantShape {
        VariantShape {
            name: name.to_owned(),
            kind: VariantKind::Named,
            fields: fields(list),
        }
    }

    fn enum_shape(name: &str, variants: Vec<VariantShape>) -> AdtShape {
        AdtShape::Enum {
            name: name.to_owned(),
            variants,
        }
    }

    fn struct_shape(name: &str, kind: VariantKind, fields: Vec<FieldShape>) -> AdtShape {
        AdtShape::Struct {
            name: name.to_owned(),
            fields,
            kind,
        }
    }

    const NAMES: [&str; 19] = [
        "Light",
        "Cell",
        "Shape",
        "Never",
        "E",
        "P",
        "U",
        "T",
        "Sh",
        "Z",
        "Y",
        "W",
        "K",
        "Light3",
        "CellI",
        "OptBool",
        "OptOptBool",
        "OptPair",
        "One",
    ];

    fn adt(name: &str) -> Ty {
        let index = NAMES.iter().position(|n| *n == name).expect("known type");
        Ty::adt(DefId(u32::try_from(index).unwrap()))
    }

    fn tup(elems: &[Ty]) -> Ty {
        Ty::Tuple(Box::new(elems.to_vec()))
    }

    fn prim(p: PrimTy) -> Ty {
        Ty::Prim(p)
    }

    fn cx() -> FakeCx {
        use VariantKind::{Named, Tuple, Unit};
        let i32_ = Ty::I32;
        let bool_ = Ty::BOOL;
        let adts = vec![
            enum_shape(
                "Light",
                vec![
                    unit("Red"),
                    unit("Yellow"),
                    unit("Green"),
                    tuple_variant("Blinking", &[Ty::I32]),
                ],
            ),
            enum_shape(
                "Cell",
                vec![unit("Empty"), tuple_variant("Value", &[Ty::BOOL])],
            ),
            enum_shape(
                "Shape",
                vec![
                    unit("Dot"),
                    named_variant("Rect", &[("W", i32_.clone()), ("Filled", bool_.clone())]),
                ],
            ),
            enum_shape("Never", Vec::new()),
            enum_shape("E", vec![unit("A"), tuple_variant("B", &[adt("Never")])]),
            struct_shape(
                "P",
                Named,
                fields(&[("X", i32_.clone()), ("Y", bool_.clone())]),
            ),
            struct_shape("U", Unit, Vec::new()),
            struct_shape("T", Tuple, tuple_fields(&[i32_.clone(), bool_.clone()])),
            enum_shape(
                "Sh",
                vec![
                    unit("Dot"),
                    named_variant("Rect", &[("W", i32_.clone()), ("Filled", bool_.clone())]),
                    named_variant("Empty", &[]),
                    tuple_variant("Tup", &[]),
                ],
            ),
            struct_shape("Z", Named, Vec::new()),
            struct_shape("Y", Tuple, Vec::new()),
            struct_shape("W", Tuple, tuple_fields(&[Ty::BOOL])),
            enum_shape(
                "K",
                vec![
                    named_variant("A", &[]),
                    tuple_variant("B", &[]),
                    named_variant("C", &[("X", bool_.clone())]),
                ],
            ),
            enum_shape("Light3", vec![unit("Red"), unit("Yellow"), unit("Green")]),
            enum_shape(
                "CellI",
                vec![unit("Empty"), tuple_variant("Value", &[Ty::I32])],
            ),
            enum_shape(
                "OptBool",
                vec![unit("None"), tuple_variant("Some", &[Ty::BOOL])],
            ),
            enum_shape(
                "OptOptBool",
                vec![unit("None"), tuple_variant("Some", &[adt("OptBool")])],
            ),
            enum_shape(
                "OptPair",
                vec![
                    unit("None"),
                    tuple_variant("Some", &[tup(&[bool_, prim(PrimTy::U8)])]),
                ],
            ),
            enum_shape("One", vec![tuple_variant("Only", &[i32_])]),
        ];
        assert_eq!(adts.len(), NAMES.len());
        FakeCx { adts }
    }

    // --- a small pattern parser, type-directed like rustc's lowering ---------------------

    struct Parser<'s> {
        cx: &'s FakeCx,
        src: &'s str,
        pos: usize,
        base: usize,
    }

    fn int_bounds(p: PrimTy) -> (i128, i128) {
        let int = IntTy::of(p).expect("an integer type");
        if int.signed {
            let max = i128::try_from(mask(int.bits - 1)).unwrap();
            (-max - 1, max)
        } else {
            #[allow(clippy::cast_possible_wrap)] // `u128` values are passed as bit patterns
            let max = mask(int.bits) as i128;
            (0, max)
        }
    }

    impl Parser<'_> {
        fn rest(&self) -> &str {
            &self.src[self.pos..]
        }

        fn ws(&mut self) {
            let trimmed = self.rest().trim_start();
            self.pos = self.src.len() - trimmed.len();
        }

        fn eat(&mut self, token: &str) -> bool {
            self.ws();
            if self.rest().starts_with(token) {
                self.pos += token.len();
                true
            } else {
                false
            }
        }

        fn expect(&mut self, token: &str) {
            assert!(
                self.eat(token),
                "expected `{token}` at {} in `{}`",
                self.pos,
                self.src
            );
        }

        fn span(&self, start: usize) -> Span {
            let end = start + self.src[start..self.pos].trim_end().len();
            let lo = u32::try_from(self.base + start).unwrap();
            let hi = u32::try_from(self.base + end).unwrap();
            Span::new(FileId(0), lo, hi)
        }

        fn mk(&self, kind: PatKind, ty: &Ty, start: usize) -> Pat {
            Pat {
                kind,
                ty: ty.clone(),
                span: self.span(start),
            }
        }

        fn peek_ident(&mut self) -> Option<String> {
            self.ws();
            let ident: String = self
                .rest()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let starts_ok = ident
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_');
            starts_ok.then_some(ident)
        }

        fn ident(&mut self) -> String {
            let ident = self.peek_ident().expect("an identifier");
            self.pos += ident.len();
            ident
        }

        fn is_binding(&mut self) -> bool {
            self.peek_ident().is_some_and(|id| {
                id.starts_with(|c: char| c.is_lowercase() || c == '_')
                    && id != "true"
                    && id != "false"
                    && PrimTy::from_name(&id).is_none()
            })
        }

        fn pat(&mut self, ty: &Ty) -> Pat {
            self.ws();
            let start = self.pos;
            let mut alts = vec![self.alt(ty)];
            while self.eat("|") {
                alts.push(self.alt(ty));
            }
            if alts.len() == 1 {
                alts.pop().unwrap()
            } else {
                self.mk(PatKind::Or(alts), ty, start)
            }
        }

        fn alt(&mut self, ty: &Ty) -> Pat {
            self.ws();
            let start = self.pos;
            if let Ty::Ref(_, inner) = ty {
                if self.eat("&") {
                    self.eat("mut ");
                    let p = self.alt(inner);
                    return self.mk(PatKind::Deref(Box::new(p)), ty, start);
                }
                let string = **inner == Ty::Prim(PrimTy::Str) && self.rest().starts_with('"');
                if !self.is_binding() && !string {
                    // Default binding mode: peel the reference.
                    let p = self.alt(inner);
                    return self.mk(PatKind::Deref(Box::new(p)), ty, start);
                }
            }
            if self.is_binding() {
                self.ident();
                if self.eat("@") {
                    return self.pat(ty);
                }
                return self.mk(PatKind::Wild, ty, start);
            }
            if self.eat("(") {
                return match ty {
                    Ty::Unit => {
                        self.expect(")");
                        let kind = PatKind::Ctor {
                            variant: None,
                            fields: Vec::new(),
                        };
                        self.mk(kind, ty, start)
                    }
                    Ty::Tuple(elems) => {
                        let fields = self.list(elems, ")");
                        let kind = PatKind::Ctor {
                            variant: None,
                            fields,
                        };
                        self.mk(kind, ty, start)
                    }
                    _ => {
                        let p = self.pat(ty);
                        self.expect(")");
                        p
                    }
                };
            }
            if self.eat("\"") {
                let end = self.rest().find('"').expect("closing quote");
                let s = self.rest()[..end].to_owned();
                self.pos += end + 1;
                return self.mk(PatKind::Str(s), ty, start);
            }
            if self.eat("true") {
                return self.mk(PatKind::Bool(true), ty, start);
            }
            if self.eat("false") {
                return self.mk(PatKind::Bool(false), ty, start);
            }
            if self.eat("..=") {
                let hi = self.value(ty);
                let kind = PatKind::OpenRange {
                    lo: None,
                    hi: Some(hi),
                };
                return self.mk(kind, ty, start);
            }
            let is_value = self
                .rest()
                .starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '\'')
                || self
                    .peek_ident()
                    .is_some_and(|id| PrimTy::from_name(&id).is_some());
            if !is_value {
                return self.adt_pat(ty, start);
            }
            if let Some(f) = self.float() {
                return self.mk(PatKind::Float(f), ty, start);
            }
            let lo = self.value(ty);
            if self.eat("..=") {
                let hi = self.value(ty);
                return self.mk(PatKind::Range { lo, hi }, ty, start);
            }
            if self.eat("..") {
                self.ws();
                let more = self.rest().starts_with(|c: char| {
                    c.is_ascii_digit() || c == '-' || c == '\'' || c.is_alphabetic()
                });
                if more {
                    let hi = self.value(ty) - 1;
                    return self.mk(PatKind::Range { lo, hi }, ty, start);
                }
                let kind = PatKind::OpenRange {
                    lo: Some(lo),
                    hi: None,
                };
                return self.mk(kind, ty, start);
            }
            self.mk(PatKind::Range { lo, hi: lo }, ty, start)
        }

        fn float(&mut self) -> Option<f64> {
            let text: String = self
                .rest()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.')
                .collect();
            let dot = text.find('.')?;
            if !text[dot + 1..].starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            self.pos += text.len();
            Some(text.parse().unwrap())
        }

        fn value(&mut self, ty: &Ty) -> i128 {
            self.ws();
            if self.eat("'") {
                let c = if self.eat("\\u{") {
                    let end = self.rest().find('}').unwrap();
                    let v = u32::from_str_radix(&self.rest()[..end], 16).unwrap();
                    self.pos += end + 1;
                    v
                } else if self.eat("\\0") {
                    0
                } else {
                    let c = self.rest().chars().next().unwrap();
                    self.pos += c.len_utf8();
                    u32::from(c)
                };
                self.expect("'");
                return i128::from(c);
            }
            if let Some(id) = self.peek_ident() {
                if let Some(p) = PrimTy::from_name(&id) {
                    self.ident();
                    self.expect("::");
                    let (min, max) = int_bounds(p);
                    return match self.ident().as_str() {
                        "MIN" => min,
                        "MAX" => max,
                        other => panic!("unknown constant {other}"),
                    };
                }
            }
            let text: String = self
                .rest()
                .char_indices()
                .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
                .map(|(_, c)| c)
                .collect();
            self.pos += text.len();
            if *ty == Ty::Prim(PrimTy::U128) {
                #[allow(clippy::cast_possible_wrap)] // `u128` values are passed as bit patterns
                let v = text.parse::<u128>().unwrap() as i128;
                v
            } else {
                text.parse().unwrap()
            }
        }

        fn wild(&self, ty: &Ty) -> Pat {
            self.mk(PatKind::Wild, ty, self.pos)
        }

        fn list(&mut self, tys: &[Ty], close: &str) -> Vec<Pat> {
            let mut out = Vec::new();
            loop {
                if self.eat(close) {
                    break;
                }
                self.ws();
                if self.rest().starts_with("..") && !self.rest().starts_with("..=") {
                    self.pos += 2;
                    while out.len() < tys.len() {
                        out.push(self.wild(&tys[out.len()]));
                    }
                    self.eat(",");
                    continue;
                }
                let p = self.pat(&tys[out.len()]);
                out.push(p);
                if !self.eat(",") {
                    self.expect(close);
                    break;
                }
            }
            assert_eq!(out.len(), tys.len(), "field count in `{}`", self.src);
            out
        }

        fn named_fields(&mut self, shape_fields: &[FieldShape]) -> Vec<Pat> {
            self.expect("{");
            let mut out: Vec<Option<Pat>> = vec![None; shape_fields.len()];
            loop {
                if self.eat("}") {
                    break;
                }
                if self.eat("..") {
                    self.expect("}");
                    break;
                }
                let fstart = self.pos;
                let name = self.ident();
                let index = shape_fields
                    .iter()
                    .position(|f| f.name == name)
                    .expect("known field");
                let ty = &shape_fields[index].ty;
                let p = if self.eat(":") {
                    self.pat(ty)
                } else {
                    self.mk(PatKind::Wild, ty, fstart)
                };
                out[index] = Some(p);
                if !self.eat(",") {
                    self.expect("}");
                    break;
                }
            }
            out.into_iter()
                .zip(shape_fields)
                .map(|(p, f)| p.unwrap_or_else(|| self.wild(&f.ty)))
                .collect()
        }

        fn adt_pat(&mut self, ty: &Ty, start: usize) -> Pat {
            let mut path = vec![self.ident()];
            while self.eat("::") {
                path.push(self.ident());
            }
            let Ty::Adt(def, _) = ty else {
                panic!("path `{}` at a non-ADT type", path.join("::"))
            };
            let (variant, kind, shape_fields) = match self.cx.adt(*def) {
                AdtShape::Enum { variants, .. } => {
                    let index = variants
                        .iter()
                        .position(|v| v.name == path[1])
                        .expect("known variant");
                    let v = &variants[index];
                    (Some(index), v.kind, v.fields.clone())
                }
                AdtShape::Struct { kind, fields, .. } => (None, kind, fields),
            };
            let fields = match kind {
                VariantKind::Unit => Vec::new(),
                VariantKind::Tuple => {
                    self.expect("(");
                    let tys: Vec<Ty> = shape_fields.iter().map(|f| f.ty.clone()).collect();
                    self.list(&tys, ")")
                }
                VariantKind::Named => self.named_fields(&shape_fields),
            };
            self.mk(PatKind::Ctor { variant, fields }, ty, start)
        }
    }

    /// Runs the checker on arms written in Rust syntax (a trailing ` if` marks a guard);
    /// returns the witnesses and the source text of the unreachable patterns.
    fn run_with(ty: &Ty, arms: &[&str], valid: bool) -> (Vec<String>, Vec<String>) {
        let cx = cx();
        let source = arms.join("\n");
        let mut base = 0;
        let mut lowered = Vec::new();
        for arm in arms {
            let (text, has_guard) = match arm.strip_suffix(" if") {
                Some(text) => (text, true),
                None => (*arm, false),
            };
            let mut parser = Parser {
                cx: &cx,
                src: text,
                pos: 0,
                base,
            };
            let pat = parser.pat(ty);
            parser.ws();
            assert_eq!(parser.pos, text.len(), "unparsed input in `{text}`");
            lowered.push(Arm { pat, has_guard });
            base += arm.len() + 1;
        }
        let report = check_match_with_validity(&cx, ty, &lowered, valid);
        let unreachable = report
            .unreachable
            .iter()
            .map(|s| source[s.start as usize..s.end as usize].to_owned())
            .collect();
        (report.witnesses, unreachable)
    }

    #[track_caller]
    fn check(ty: &Ty, arms: &[&str], witnesses: &[&str], unreachable: &[&str]) {
        let (w, u) = run_with(ty, arms, true);
        assert_eq!(w, witnesses, "witnesses of {arms:?}");
        assert_eq!(u, unreachable, "unreachable patterns of {arms:?}");
    }

    #[track_caller]
    fn message(ty: &Ty, arms: &[&str]) -> String {
        let (w, _) = run_with(ty, arms, true);
        joined_witnesses(&w)
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- message formatting --------------------------------------------------------------

    #[test]
    fn joined_witness_lists() {
        assert_eq!(joined_list(&[]), "");
        assert_eq!(joined_witnesses(&strings(&["A"])), "`A` not covered");
        assert_eq!(
            joined_witnesses(&strings(&["A", "B"])),
            "`A` and `B` not covered"
        );
        assert_eq!(
            joined_witnesses(&strings(&["A", "B", "C"])),
            "`A`, `B` and `C` not covered"
        );
        assert_eq!(
            joined_witnesses(&strings(&["A", "B", "C", "D"])),
            "`A`, `B`, `C` and 1 more not covered"
        );
        assert_eq!(
            joined_witnesses(&strings(&["A", "B", "C", "D", "E"])),
            "`A`, `B`, `C` and 2 more not covered"
        );
        assert_eq!(witnesses_label(&strings(&["A"])), "pattern `A` not covered");
        assert_eq!(
            witnesses_label(&strings(&["A", "B", "C", "D"])),
            "patterns `A`, `B`, `C` and 1 more not covered"
        );
    }

    // --- enums -------------------------------------------------------------------------

    #[test]
    fn missing_variants() {
        let light = adt("Light");
        check(
            &light,
            &["Light::Red", "Light::Yellow", "Light::Blinking(_)"],
            &["Light::Green"],
            &[],
        );
        assert_eq!(
            message(&light, &["Light::Red"]),
            "`Light::Yellow`, `Light::Green` and `Light::Blinking(_)` not covered"
        );
        // `Light::Blinking(5)` leaves other payloads, but rustc names only the absent
        // variants next to a present one.
        assert_eq!(
            message(&light, &["Light::Blinking(5)"]),
            "`Light::Red`, `Light::Yellow` and `Light::Green` not covered"
        );
        assert_eq!(
            message(&light, &["_ if"]),
            "`Light::Red`, `Light::Yellow`, `Light::Green` and 1 more not covered"
        );
        check(
            &adt("Light3"),
            &["Light3::Red"],
            &["Light3::Yellow", "Light3::Green"],
            &[],
        );
        check(
            &light,
            &[
                "Light::Red",
                "Light::Yellow",
                "Light::Green",
                "Light::Blinking(_)",
            ],
            &[],
            &[],
        );
    }

    #[test]
    fn match_without_arms() {
        let cx = cx();
        let report = check_match(&cx, &adt("Light3"), &[]);
        assert_eq!(
            report.witnesses,
            ["Light3::Red", "Light3::Yellow", "Light3::Green"]
        );
        assert!(!reports_type_non_empty(&cx, &adt("Light3"), &[]));
        let non_empty = [
            Ty::I32,
            adt("P"),
            adt("U"),
            Ty::shared_ref(adt("Light3")),
            Ty::shared_ref(adt("Never")),
        ];
        for ty in non_empty {
            assert!(!check_match(&cx, &ty, &[]).witnesses.is_empty(), "{ty:?}");
            assert!(reports_type_non_empty(&cx, &ty, &[]), "{ty:?}");
        }
        assert_eq!(
            check_match(&cx, &Ty::shared_ref(adt("Never")), &[]).witnesses,
            ["&_"]
        );
        let arm = Arm {
            pat: Pat {
                kind: PatKind::Wild,
                ty: Ty::I32,
                span: Span::dummy(),
            },
            has_guard: true,
        };
        assert!(!reports_type_non_empty(&cx, &Ty::I32, &[arm]));
    }

    #[test]
    fn nested_constructors() {
        check(
            &adt("Cell"),
            &["Cell::Empty", "Cell::Value(true)"],
            &["Cell::Value(false)"],
            &[],
        );
        check(
            &adt("Shape"),
            &["Shape::Dot", "Shape::Rect { Filled: true, .. }"],
            &["Shape::Rect { Filled: false, .. }"],
            &[],
        );
        check(
            &tup(&[adt("Cell"), Ty::BOOL]),
            &["(Cell::Empty, true)", "(Cell::Value(_), _)"],
            &["(Cell::Empty, false)"],
            &[],
        );
        // A present constructor next to missing ones reports nothing of its own.
        check(
            &adt("OptOptBool"),
            &["OptOptBool::Some(OptBool::Some(true))"],
            &["OptOptBool::None"],
            &[],
        );
        check(
            &adt("OptPair"),
            &[
                "OptPair::None",
                "OptPair::Some((true, 0..=100))",
                "OptPair::Some((false, _))",
            ],
            &["OptPair::Some((true, 101_u8..=u8::MAX))"],
            &[],
        );
        let pair = tup(&[adt("Light3"), adt("Light3")]);
        assert_eq!(
            message(&pair, &["(Light3::Red, _)", "(_, Light3::Red)"]),
            "`(Light3::Yellow, Light3::Yellow)`, `(Light3::Yellow, Light3::Green)`, \
             `(Light3::Green, Light3::Yellow)` and 1 more not covered"
        );
        check(
            &pair,
            &["(Light3::Red, Light3::Red)"],
            &["(Light3::Yellow, _)", "(Light3::Green, _)"],
            &[],
        );
        check(
            &pair,
            &["(Light3::Red, Light3::Red)", "(Light3::Yellow, _)"],
            &["(Light3::Green, _)"],
            &[],
        );
        let light_bool = tup(&[adt("Light3"), Ty::BOOL]);
        check(&light_bool, &["(_, true)"], &["(_, false)"], &[]);
        check(
            &light_bool,
            &[
                "(Light3::Green, true)",
                "(Light3::Red, true)",
                "(Light3::Yellow, true)",
            ],
            &[
                "(Light3::Red, false)",
                "(Light3::Yellow, false)",
                "(Light3::Green, false)",
            ],
            &[],
        );
    }

    #[test]
    fn structs_and_unit_types() {
        check(
            &adt("Sh"),
            &["Sh::Dot"],
            &["Sh::Rect { .. }", "Sh::Empty {  }", "Sh::Tup"],
            &[],
        );
        check(
            &adt("Sh"),
            &[
                "Sh::Dot",
                "Sh::Empty {}",
                "Sh::Tup()",
                "Sh::Rect { W: 0, Filled: true }",
            ],
            &[
                "Sh::Rect { W: i32::MIN..=-1_i32, .. }",
                "Sh::Rect { W: 1_i32..=i32::MAX, .. }",
            ],
            &[],
        );
        check(
            &adt("P"),
            &["P { Y: true, .. }"],
            &["P { Y: false, .. }"],
            &[],
        );
        check(
            &adt("P"),
            &["P { X: 0, Y }"],
            &[
                "P { X: i32::MIN..=-1_i32, .. }",
                "P { X: 1_i32..=i32::MAX, .. }",
            ],
            &[],
        );
        check(&adt("T"), &["T(_, true)"], &["T(_, false)"], &[]);
        check(
            &tup(&[adt("U"), Ty::BOOL]),
            &["(U, true)"],
            &["(U, false)"],
            &[],
        );
        check(
            &tup(&[Ty::Unit, Ty::BOOL]),
            &["((), true)"],
            &["((), false)"],
            &[],
        );
        check(
            &tup(&[adt("Z"), Ty::BOOL]),
            &["(Z {}, true)"],
            &["(Z {  }, false)"],
            &[],
        );
        check(
            &tup(&[adt("Y"), adt("W")]),
            &["(Y(), W(true))"],
            &["(Y, W(false))"],
            &[],
        );
        check(
            &adt("K"),
            &["K::C { X: true }"],
            &["K::A {  }", "K::B"],
            &[],
        );
        check(
            &adt("K"),
            &["K::A {} | K::B()", "K::C { X: true }"],
            &["K::C { X: false }"],
            &[],
        );
        check(&Ty::Unit, &["()"], &[], &[]);
        check(&Ty::Unit, &["()", "_"], &[], &["_"]);
        check(&adt("U"), &["U"], &[], &[]);
        check(&adt("U"), &["U", "U"], &[], &["U"]);
        check(&tup(&[Ty::BOOL]), &["(true,)"], &["(false,)"], &[]);
    }

    // --- integers and chars ------------------------------------------------------------

    #[test]
    fn integer_gaps() {
        let i32_ = Ty::I32;
        check(
            &i32_,
            &["0", "1 | 2"],
            &["i32::MIN..=-1_i32", "3_i32..=i32::MAX"],
            &[],
        );
        check(&prim(PrimTy::U8), &["0..=9", "10..=254"], &["u8::MAX"], &[]);
        check(&prim(PrimTy::I8), &["-128..=-1", "1..=127"], &["0_i8"], &[]);
        check(&prim(PrimTy::U8), &["1..=254"], &["0_u8", "u8::MAX"], &[]);
        check(
            &prim(PrimTy::I64),
            &["i64::MIN", "i64::MAX", "0"],
            &[
                "-9223372036854775807_i64..=-1_i64",
                "1_i64..=9223372036854775806_i64",
            ],
            &[],
        );
        assert_eq!(
            message(&prim(PrimTy::U16), &["0 | 2 | 4 | 6 | 8"]),
            "`1_u16`, `3_u16`, `5_u16` and 2 more not covered"
        );
        check(&i32_, &["_ if"], &["i32::MIN..=i32::MAX"], &[]);
        check(
            &tup(&[prim(PrimTy::I8), prim(PrimTy::I8)]),
            &["(0, 0)", "(i8::MIN..=-1, _)"],
            &["(1_i8..=i8::MAX, _)"],
            &[],
        );
        let u8_bool = tup(&[prim(PrimTy::U8), Ty::BOOL]);
        check(
            &u8_bool,
            &["(0..=5, true)", "(3..=10, false)"],
            &["(11_u8..=u8::MAX, _)"],
            &[],
        );
        for arms in [
            ["(5, true)", "(0..=4 | 6..=255, true)"],
            ["(0..=4 | 6..=255, true)", "(5, true)"],
        ] {
            check(
                &u8_bool,
                &arms,
                &[
                    "(0_u8..=4_u8, false)",
                    "(5_u8, false)",
                    "(6_u8..=u8::MAX, false)",
                ],
                &[],
            );
        }
        // Exclusive ranges are passed as inclusive ones.
        check(&i32_, &["i32::MIN..0", "0..i32::MAX", "i32::MAX"], &[], &[]);
        check(&i32_, &["..=-1", "1.."], &["0_i32"], &[]);
    }

    #[test]
    fn integers_covered_by_split_ranges() {
        check(
            &prim(PrimTy::U8),
            &["0", "1..=127", "128..=254", "255"],
            &[],
            &[],
        );
        check(
            &prim(PrimTy::I8),
            &["-128..=-1 | 0..=127", "5"],
            &[],
            &["5"],
        );
        check(
            &prim(PrimTy::I8),
            &["-128..=-1", "0..=127", "_"],
            &[],
            &["_"],
        );
        check(
            &Ty::I32,
            &["0..=9", "10..=19", "5..=15", "_"],
            &[],
            &["5..=15"],
        );
    }

    /// `match x { 0 => .. }` for every integer type, as rustc 1.99.1 reports it.
    #[test]
    fn every_integer_type() {
        let cases = [
            (
                PrimTy::I8,
                "`i8::MIN..=-1_i8` and `1_i8..=i8::MAX` not covered",
            ),
            (
                PrimTy::I16,
                "`i16::MIN..=-1_i16` and `1_i16..=i16::MAX` not covered",
            ),
            (
                PrimTy::I32,
                "`i32::MIN..=-1_i32` and `1_i32..=i32::MAX` not covered",
            ),
            (
                PrimTy::I64,
                "`i64::MIN..=-1_i64` and `1_i64..=i64::MAX` not covered",
            ),
            (
                PrimTy::I128,
                "`i128::MIN..=-1_i128` and `1_i128..` not covered",
            ),
            (PrimTy::Isize, "`..=-1_isize` and `1_isize..` not covered"),
            (PrimTy::U8, "`1_u8..=u8::MAX` not covered"),
            (PrimTy::U16, "`1_u16..=u16::MAX` not covered"),
            (PrimTy::U32, "`1_u32..=u32::MAX` not covered"),
            (PrimTy::U64, "`1_u64..=u64::MAX` not covered"),
            (PrimTy::U128, "`1_u128..` not covered"),
            (PrimTy::Usize, "`1_usize..` not covered"),
        ];
        for (p, expected) in cases {
            assert_eq!(message(&prim(p), &["0"]), expected, "{p:?}");
        }
    }

    #[test]
    fn wide_integer_types() {
        let u128_ = prim(PrimTy::U128);
        check(
            &u128_,
            &["0", "340282366920938463463374607431768211455"],
            &["1_u128..=340282366920938463463374607431768211454_u128"],
            &[],
        );
        check(&u128_, &["0"], &["1_u128.."], &[]);
        check(&u128_, &["0..=5", "7.."], &["6_u128"], &[]);
        check(&u128_, &["0..=5", "7..=u128::MAX"], &["6_u128"], &[]);
        check(&u128_, &["1..=u128::MAX"], &["0_u128"], &[]);
        check(&u128_, &["0..=10"], &["11_u128.."], &[]);
        check(&u128_, &["0..u128::MAX"], &["u128::MAX"], &[]);
        let i128_ = prim(PrimTy::I128);
        check(&i128_, &["0"], &["i128::MIN..=-1_i128", "1_i128.."], &[]);
        check(&i128_, &["_ if"], &["i128::MIN.."], &[]);
        check(
            &i128_,
            &["i128::MIN..=-1", "1..=i128::MAX"],
            &["0_i128"],
            &[],
        );
        check(&i128_, &["..=-1", "0..=i128::MAX", "7"], &[], &["7"]);
    }

    #[test]
    fn pointer_sized_integers_have_no_fixed_ends() {
        let usize_ = prim(PrimTy::Usize);
        let isize_ = prim(PrimTy::Isize);
        check(
            &usize_,
            &["0..=18446744073709551615"],
            &["usize::MAX.."],
            &[],
        );
        check(&usize_, &["0..=usize::MAX"], &["usize::MAX.."], &[]);
        check(&usize_, &["0", "1.."], &[], &[]);
        check(&usize_, &["0", "1..", "5"], &[], &["5"]);
        check(&usize_, &["1.."], &["0_usize"], &[]);
        check(&isize_, &["0"], &["..=-1_isize", "1_isize.."], &[]);
        check(
            &isize_,
            &["-9223372036854775808..=9223372036854775807"],
            &["..isize::MIN", "isize::MAX.."],
            &[],
        );
        check(&isize_, &["..=0", "1.."], &[], &[]);
        check(&isize_, &["_ if"], &["_"], &[]);
    }

    #[test]
    fn char_gaps() {
        let char_ = Ty::CHAR;
        assert_eq!(
            message(&char_, &["'a'..='z'", "'A'..='Z'"]),
            "`'\\0'..='@'`, `'['..='`'`, `'{'..='\\u{d7ff}'` and 1 more not covered"
        );
        check(
            &char_,
            &["'\\0'..='\\u{d7ff}'", "'\\u{e000}'..='\\u{ffff}'"],
            &["'𐀀'..='\\u{10ffff}'"],
            &[],
        );
        check(
            &char_,
            &["'\\0'..='\\u{d7ff}'"],
            &["'\\u{e000}'..='\\u{10ffff}'"],
            &[],
        );
        check(
            &char_,
            &["'b'"],
            &[
                "'\\0'..='a'",
                "'c'..='\\u{d7ff}'",
                "'\\u{e000}'..='\\u{10ffff}'",
            ],
            &[],
        );
        check(
            &char_,
            &["_ if"],
            &["'\\0'..='\\u{d7ff}'", "'\\u{e000}'..='\\u{10ffff}'"],
            &[],
        );
        // The surrogates are not values: a range over them covers every scalar value.
        check(&char_, &["'\\0'..='\\u{10ffff}'", "'a'"], &[], &["'a'"]);
        check(
            &char_,
            &["'\\0'..='\\u{d7ff}'", "'\\u{e000}'..='\\u{10ffff}'"],
            &[],
            &[],
        );
    }

    // --- bools and tuples --------------------------------------------------------------

    #[test]
    fn bools_and_tuples_of_bools() {
        let bb = tup(&[Ty::BOOL, Ty::BOOL]);
        check(
            &bb,
            &["(true, _)", "(false, false)"],
            &["(false, true)"],
            &[],
        );
        check(&Ty::BOOL, &["_ if"], &["true", "false"], &[]);
        check(&bb, &["_ if"], &["(_, _)"], &[]);
        for arms in [
            ["(true, true)", "(false, false)"],
            ["(false, false)", "(true, true)"],
        ] {
            check(&bb, &arms, &["(true, false)", "(false, true)"], &[]);
        }
        check(&bb, &["(true, true)"], &["(false, _)"], &[]);
        check(&Ty::BOOL, &["true", "false"], &[], &[]);
        let bbb = tup(&[Ty::BOOL, Ty::BOOL, Ty::BOOL]);
        check(
            &bbb,
            &["(true, true, true)", "(false, false, false)"],
            &["(true, false, _)", "(false, true, _)"],
            &[],
        );
        for arms in [
            ["(false, true, _)", "(true, false, _)", "(_, _, true)"],
            ["(_, _, true)", "(true, false, _)", "(false, true, _)"],
        ] {
            check(
                &bbb,
                &arms,
                &["(true, true, false)", "(false, false, false)"],
                &[],
            );
        }
    }

    // --- guards and reachability -------------------------------------------------------

    #[test]
    fn guards_do_not_count() {
        check(
            &adt("CellI"),
            &["CellI::Empty", "CellI::Value(n) if", "CellI::Value(n) if"],
            &["CellI::Value(_)"],
            &[],
        );
        check(
            &tup(&[Ty::BOOL, Ty::BOOL]),
            &["(true, _) if", "(false, _) if"],
            &["(true, _)", "(false, _)"],
            &[],
        );
        check(&prim(PrimTy::U8), &["_ if", "x", "_"], &[], &["_"]);
        // A column only guarded wildcards mention is reported as `_` (rustc 1.99.1), not
        // opened into its fields.
        check(
            &tup(&[Ty::BOOL, tup(&[Ty::I32, Ty::I32])]),
            &["(true, _)", "(false, _) if"],
            &["(false, _)"],
            &[],
        );
        // A guarded arm can itself be unreachable.
        check(&Ty::BOOL, &["_", "true if"], &[], &["true"]);
    }

    #[test]
    fn unreachable_arms_and_alternatives() {
        let light = adt("Light3");
        check(
            &light,
            &["Light3::Red", "_", "Light3::Green"],
            &[],
            &["Light3::Green"],
        );
        check(
            &light,
            &[
                "Light3::Red",
                "Light3::Yellow",
                "Light3::Red",
                "Light3::Green",
            ],
            &[],
            &["Light3::Red"],
        );
        check(
            &Ty::I32,
            &["0..=9", "10..=19", "5..=15", "1 | 20", "x if", "-1", "_"],
            &[],
            &["5..=15", "1"],
        );
        let bb = tup(&[Ty::BOOL, Ty::BOOL]);
        check(
            &bb,
            &[
                "(true, _) | (_, true)",
                "(true, true) | (false, false)",
                "(false | true, false | true)",
            ],
            &[],
            &["(true, true)", "(false | true, false | true)"],
        );
        check(
            &bb,
            &["(true, true)", "(true, true)", "_", "(false, _)"],
            &[],
            &["(true, true)", "(false, _)"],
        );
        check(
            &adt("CellI"),
            &[
                "CellI::Value(n) if",
                "CellI::Value(n) if",
                "CellI::Empty | CellI::Empty",
                "CellI::Value(_)",
                "CellI::Value(1) | CellI::Empty",
            ],
            &[],
            &["CellI::Empty", "CellI::Value(1) | CellI::Empty"],
        );
    }

    #[test]
    fn nested_or_patterns_report_leaves() {
        check(
            &adt("CellI"),
            &[
                "CellI::Value(1 | 2 | 1)",
                "CellI::Value(3 | (4 | 3))",
                "CellI::Value((5 | 6) | (5 | 6))",
                "_",
            ],
            &[],
            &["1", "3", "5", "6"],
        );
    }

    // --- empty types ---------------------------------------------------------------------

    #[test]
    fn empty_types() {
        let never = adt("Never");
        let e = adt("E");
        let never_ref = Ty::shared_ref(never.clone());
        // `match n {}` is exhaustive; a `_` arm on a valid empty scrutinee is unreachable.
        check(&never, &[], &[], &[]);
        check(&Ty::Never, &[], &[], &[]);
        check(&never, &["_"], &[], &["_"]);
        // `match *r {}` and `match *r { _ => .. }` for `r: &Never`: exhaustive, reachable.
        assert_eq!(run_with(&never, &[], false), (Vec::new(), Vec::new()));
        assert_eq!(run_with(&never, &["_"], false), (Vec::new(), Vec::new()));
        check(&never_ref, &["_"], &[], &[]);
        // An uninhabited variant may be omitted by value and is not reported unreachable.
        check(&e, &["E::A", "E::B(_)"], &[], &[]);
        check(&e, &["E::A", "E::B(x)"], &[], &[]);
        check(&e, &["E::A"], &[], &[]);
        // Behind a reference, or under a dereferencing scrutinee, it must be matched.
        check(&Ty::shared_ref(e.clone()), &["E::A"], &["&E::B(_)"], &[]);
        assert_eq!(
            run_with(&e, &["E::A"], false),
            (strings(&["E::B(_)"]), Vec::new())
        );
        // A tuple with an empty element is empty; one with a reference to one is not.
        check(&tup(&[Ty::I32, never]), &[], &[], &[]);
        check(&tup(&[Ty::I32, Ty::Never]), &[], &[], &[]);
        check(&tup(&[Ty::I32, never_ref]), &[], &["(_, _)"], &[]);
    }

    // --- references, strings and floats ------------------------------------------------

    #[test]
    fn references() {
        let light = adt("Light");
        check(
            &Ty::mut_ref(light.clone()),
            &["Light::Red"],
            &[
                "&mut Light::Yellow",
                "&mut Light::Green",
                "&mut Light::Blinking(_)",
            ],
            &[],
        );
        check(
            &Ty::shared_ref(Ty::shared_ref(light)),
            &["Light::Red", "Light::Yellow", "Light::Green"],
            &["&&Light::Blinking(_)"],
            &[],
        );
        let pair = Ty::shared_ref(tup(&[adt("Light3"), adt("Light3")]));
        check(
            &pair,
            &["(Light3::Red, Light3::Red)", "&(Light3::Yellow, _)"],
            &["&(Light3::Green, _)"],
            &[],
        );
        check(
            &Ty::Ref(Mutability::Not, Box::new(adt("Cell"))),
            &["&Cell::Empty", "Cell::Value(_)", "_"],
            &[],
            &["_"],
        );
    }

    #[test]
    fn strings_and_floats() {
        let str_ref = Ty::shared_ref(prim(PrimTy::Str));
        check(&str_ref, &["\"a\""], &["&_"], &[]);
        check(&str_ref, &["\"a\"", "\"b\"", "\"a\"", "_"], &[], &["\"a\""]);
        check(&Ty::F64, &["1.0"], &["_"], &[]);
        check(
            &Ty::F64,
            &["0.0", "-0.0", "1.0", "1.0", "_"],
            &[],
            &["-0.0", "1.0"],
        );
        check(
            &tup(&[str_ref, Ty::F64]),
            &["(\"a\", 1.0)", "(\"a\", 1.0)", "(\"b\", _)"],
            &["(&_, _)"],
            &["(\"a\", 1.0)"],
        );
        // Opaque patterns cover nothing but themselves.
        let cx = cx();
        let opaque = || Arm {
            pat: Pat {
                kind: PatKind::Opaque,
                ty: Ty::F64,
                span: Span::dummy(),
            },
            has_guard: false,
        };
        let report = check_match(&cx, &Ty::F64, &[opaque(), opaque()]);
        assert_eq!(report.witnesses, ["_"]);
        assert!(report.unreachable.is_empty());
        let wild = Arm {
            pat: Pat {
                kind: PatKind::Wild,
                ty: Ty::F64,
                span: Span::dummy(),
            },
            has_guard: false,
        };
        assert_eq!(
            check_match(&cx, &Ty::F64, &[opaque(), wild]),
            Report::default()
        );
        // Of an integer type too: it is not part of the type's ranges.
        let opaque_int = Arm {
            pat: Pat {
                kind: PatKind::Opaque,
                ty: Ty::I32,
                span: Span::dummy(),
            },
            has_guard: false,
        };
        let wild_int = Arm {
            pat: Pat {
                kind: PatKind::Wild,
                ty: Ty::I32,
                span: Span::dummy(),
            },
            has_guard: false,
        };
        assert_eq!(
            check_match(&cx, &Ty::I32, &[opaque_int, wild_int]),
            Report::default()
        );
    }

    // --- let and parameter patterns -------------------------------------------------------

    #[test]
    fn refutable_and_irrefutable_bindings() {
        check(&adt("CellI"), &["CellI::Value(n)"], &["CellI::Empty"], &[]);
        check(
            &tup(&[Ty::I32, Ty::I32]),
            &["(0, m)"],
            &["(i32::MIN..=-1_i32, _)", "(1_i32..=i32::MAX, _)"],
            &[],
        );
        check(&adt("One"), &["One::Only(n)"], &[], &[]);
        check(&adt("P"), &["P { X, Y: y }"], &[], &[]);
        check(
            &Ty::I32,
            &["x @ 0..=5"],
            &["i32::MIN..=-1_i32", "6_i32..=i32::MAX"],
            &[],
        );
    }

    #[test]
    fn error_types_report_nothing() {
        let cx = cx();
        assert_eq!(check_match(&cx, &Ty::Error, &[]), Report::default());
        // An error behind a reference or inside a tuple counts too.
        let behind_ref = Ty::shared_ref(tup(&[Ty::BOOL, Ty::Error]));
        assert_eq!(check_match(&cx, &behind_ref, &[]), Report::default());
        assert_eq!(check_match(&cx, &tup(&[Ty::Error]), &[]), Report::default());
        // And as an array or slice element.
        let array = Ty::Array(Box::new(Ty::Error), 2);
        assert_eq!(check_match(&cx, &array, &[]), Report::default());
        let slice = Ty::shared_ref(Ty::Slice(Box::new(Ty::Error)));
        assert_eq!(check_match(&cx, &slice, &[]), Report::default());
        let arm = Arm {
            pat: Pat {
                kind: PatKind::Ctor {
                    variant: None,
                    fields: vec![Pat {
                        kind: PatKind::Bool(true),
                        ty: Ty::Error,
                        span: Span::dummy(),
                    }],
                },
                ty: tup(&[Ty::BOOL]),
                span: Span::dummy(),
            },
            has_guard: false,
        };
        assert_eq!(
            check_match(&cx, &tup(&[Ty::BOOL]), &[arm]),
            Report::default()
        );
    }

    // --- efficiency ---------------------------------------------------------------------

    #[test]
    fn wide_matches_stay_fast() {
        // 50 integer arms, then the same 50 again (unreachable).
        let arms: Vec<String> = (0..100).map(|i| (i % 50).to_string()).collect();
        let arms: Vec<&str> = arms.iter().map(String::as_str).collect();
        let (w, u) = run_with(&Ty::I32, &arms, true);
        assert_eq!(w, ["i32::MIN..=-1_i32", "50_i32..=i32::MAX"]);
        assert_eq!(u.len(), 50);

        // Every combination of six bools, then all but the first.
        let six = tup(&[Ty::BOOL; 6]);
        let all: Vec<String> = (0..64u32)
            .map(|n| {
                let bits: Vec<&str> = (0..6)
                    .map(|i| if n >> i & 1 == 1 { "true" } else { "false" })
                    .collect();
                format!("({})", bits.join(", "))
            })
            .collect();
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        check(&six, &all, &[], &[]);
        let (w, u) = run_with(&six, &all[1..], true);
        assert_eq!(w, ["(false, false, false, false, false, false)"]);
        assert!(u.is_empty());
        let (w, _) = run_with(&six, &["_ if"], true);
        assert_eq!(w, ["(_, _, _, _, _, _)"]);

        // A wide enum matched in pairs.
        let pair = tup(&[adt("Light"), adt("Light")]);
        let mut arms = Vec::new();
        for a in ["Red", "Yellow", "Green", "Blinking(_)"] {
            for b in ["Red", "Yellow", "Green", "Blinking(0)"] {
                arms.push(format!("(Light::{a}, Light::{b})"));
            }
        }
        let arms: Vec<&str> = arms.iter().map(String::as_str).collect();
        let (w, _) = run_with(&pair, &arms, true);
        assert_eq!(w.len(), 8, "{w:?}");
        assert_eq!(w[0], "(Light::Red, Light::Blinking(i32::MIN..=-1_i32))");
    }
}
