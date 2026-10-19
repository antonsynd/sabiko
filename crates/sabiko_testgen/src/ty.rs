//! The type universe of the generated subset: the primitive types without `i128`/`u128`,
//! user structs, and references to either (never to references).

/// The ten integer types of the shared subset.
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
}

impl IntTy {
    pub const ALL: [IntTy; 10] = [
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
        }
    }

    pub fn signed(self) -> bool {
        matches!(
            self,
            IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::Isize
        )
    }

    /// Both compilers target 64-bit platforms here, so `isize`/`usize` are 64 bits wide.
    pub fn bits(self) -> u32 {
        match self {
            IntTy::I8 | IntTy::U8 => 8,
            IntTy::I16 | IntTy::U16 => 16,
            IntTy::I32 | IntTy::U32 => 32,
            IntTy::I64 | IntTy::Isize | IntTy::U64 | IntTy::Usize => 64,
        }
    }

    pub fn min(self) -> i128 {
        if self.signed() {
            -(1i128 << (self.bits() - 1))
        } else {
            0
        }
    }

    pub fn max(self) -> i128 {
        if self.signed() {
            (1i128 << (self.bits() - 1)) - 1
        } else {
            (1i128 << self.bits()) - 1
        }
    }

    pub fn contains(self, v: i128) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// `v` reduced into the type's range the way an `as` cast does.
    pub fn wrap(self, v: i128) -> i128 {
        let modulus = 1i128 << self.bits();
        let low = v.rem_euclid(modulus);
        if self.signed() && low > self.max() {
            low - modulus
        } else {
            low
        }
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

/// A type a value can have: a primitive or a struct (by index into the program's structs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValTy {
    Prim(Prim),
    Struct(usize),
}

/// A type a local or parameter can have. References point at values only (`&&T` is not
/// in the subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    Val(ValTy),
    /// `&T` (`mutable == false`) or `&mut T`.
    Ref {
        mutable: bool,
        inner: ValTy,
    },
}

impl Ty {
    pub fn prim(p: Prim) -> Ty {
        Ty::Val(ValTy::Prim(p))
    }

    pub fn as_val(self) -> Option<ValTy> {
        match self {
            Ty::Val(v) => Some(v),
            Ty::Ref { .. } => None,
        }
    }

    pub fn is_ref(self) -> bool {
        matches!(self, Ty::Ref { .. })
    }
}
