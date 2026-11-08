//! The type universe of the generated subset: the primitive types, user structs and enums,
//! tuples and arrays of value types, slices and `str` (behind references only), references
//! to any of them, and references to those references (#11: `&&T`, `&&mut T`, `&mut &T`,
//! `&mut &mut T`, `&&str`, `&&[T]`; locals and parameters only, never three levels).
//! Tuples, arrays, struct fields and enum payloads of these types never hold references,
//! so no `&str` either: arrays of references only appear as temporaries indexed in place
//! (`["a", s][i]`, `[&x, r][i]`, #43) and in locals of a block of their own
//! (`BodyGen::stmt_ref_aggregate`: `[&T; N]`, `&[&T]`, `(&T, U)`, `[&mut T; 2]` of
//! primitive referents), which this type universe does not model. TODO(#92) for such
//! aggregates as parameters and longer-lived locals, and for `&str` (and `&&T`) in them;
//! struct fields and enum payloads wait for lifetimes (Phase 9).

/// The twelve integer types of the shared subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    Isize,
    U8,
    U16,
    U32,
    U64,
    Usize,
    I128,
    U128,
}

impl IntTy {
    pub const ALL: [IntTy; 12] = [
        IntTy::I8,
        IntTy::I16,
        IntTy::I32,
        IntTy::I64,
        IntTy::Isize,
        IntTy::U8,
        IntTy::U16,
        IntTy::U32,
        IntTy::U64,
        IntTy::Usize,
        IntTy::I128,
        IntTy::U128,
    ];

    pub fn name(self) -> &'static str {
        match self {
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::Isize => "isize",
            IntTy::U8 => "u8",
            IntTy::U16 => "u16",
            IntTy::U32 => "u32",
            IntTy::U64 => "u64",
            IntTy::Usize => "usize",
            IntTy::I128 => "i128",
            IntTy::U128 => "u128",
        }
    }

    pub fn signed(self) -> bool {
        matches!(
            self,
            IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::Isize | IntTy::I128
        )
    }

    /// Both compilers target 64-bit platforms here, so `isize`/`usize` are 64 bits wide.
    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::Isize | IntTy::U64 | IntTy::Usize => 64,
            IntTy::I128 | IntTy::U128 => 128,
        }
    }

    pub fn min(self) -> i128 {
        if self.signed() {
            i128::MIN >> (128 - self.bits())
        } else {
            0
        }
    }

    /// The largest value the generator uses. Values are modelled as `i128`, so for `u128`
    /// this is `i128::MAX`: literals and constants stay below `2^127` (run-time values can
    /// exceed it; both compilers compute those).
    pub fn max(self) -> i128 {
        if self.signed() || self.bits() == 128 {
            i128::MAX >> (128 - self.bits())
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    pub fn contains(self, v: i128) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// `v` reduced into the type's range the way an `as` cast does. Panics when the result
    /// is outside the generator's value model (a negative value cast to `u128`).
    pub fn wrap(self, v: i128) -> i128 {
        self.wrap_checked(v)
            .expect("a negative value as `u128` is outside the i128 value model")
    }

    /// [`IntTy::wrap`], or `None` when the result does not fit the `i128` value model.
    pub fn wrap_checked(self, v: i128) -> Option<i128> {
        match self {
            IntTy::I128 => return Some(v),
            IntTy::U128 => return (v >= 0).then_some(v),
            _ => {}
        }
        let modulus = 1i128 << self.bits();
        let low = v.rem_euclid(modulus);
        Some(if self.signed() && low > self.max() {
            low - modulus
        } else {
            low
        })
    }
}

/// A primitive type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prim {
    Int(IntTy),
    F32,
    F64,
    Bool,
    Char,
}

impl Prim {
    pub fn name(self) -> &'static str {
        match self {
            Prim::Int(i) => i.name(),
            Prim::F32 => "f32",
            Prim::F64 => "f64",
            Prim::Bool => "bool",
            Prim::Char => "char",
        }
    }

    pub fn is_int(self) -> bool {
        matches!(self, Prim::Int(_))
    }

    pub fn is_float(self) -> bool {
        matches!(self, Prim::F32 | Prim::F64)
    }

    pub fn is_numeric(self) -> bool {
        self.is_int() || self.is_float()
    }
}

/// A type a value can have: a primitive, a struct (by index into the program's structs), a
/// tuple (by index into the program's tuple types), an enum (by index into its enums) or an
/// array (by index into its array types). An array type without a length is a slice
/// `[T]`, which is unsized: it only appears behind a reference (`Ty::Ref`, `RetTy::Ref`).
/// So does `str` (#10), and only behind a shared one (`&str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValTy {
    Prim(Prim),
    Struct(usize),
    Tuple(usize),
    Enum(usize),
    Array(usize),
    Str,
}

/// A type a local or parameter can have: a value, a reference to one, or a reference to a
/// reference to one (#11). Nothing is returned as `&&T` (with one input lifetime, rustc
/// would not let a function return a reference to its parameter's reference anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    Val(ValTy),
    /// `&T` (`mutable == false`) or `&mut T`.
    Ref {
        mutable: bool,
        inner: ValTy,
    },
    /// `&R` (`outer == false`) or `&mut R`, where `R` is `&T` (`mutable == false`) or
    /// `&mut T`: `&&T`, `&&mut T`, `&mut &T`, `&mut &mut T`.
    RefRef {
        outer: bool,
        mutable: bool,
        inner: ValTy,
    },
}

impl Ty {
    /// `&str`, the only reference to a `str` in the subset.
    pub const STR: Ty = Ty::Ref {
        mutable: false,
        inner: ValTy::Str,
    };

    pub fn prim(p: Prim) -> Ty {
        Ty::Val(ValTy::Prim(p))
    }

    pub fn as_val(self) -> Option<ValTy> {
        match self {
            Ty::Val(v) => Some(v),
            Ty::Ref { .. } | Ty::RefRef { .. } => None,
        }
    }

    /// A reference of either depth.
    pub fn is_ref(self) -> bool {
        matches!(self, Ty::Ref { .. } | Ty::RefRef { .. })
    }

    /// The number of lifetimes an elided reference type introduces: 0, 1 or 2.
    pub fn lifetimes(self) -> usize {
        match self {
            Ty::Val(_) => 0,
            Ty::Ref { .. } => 1,
            Ty::RefRef { .. } => 2,
        }
    }
}
