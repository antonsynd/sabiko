//! Item-level structure of a generated program: struct definitions, constants and
//! function signatures. Bodies are generated afterwards by [`crate::body`] against this
//! skeleton, so that every call site knows the callee's signature and every struct literal
//! its fields.

use std::fmt::Write as _;

use crate::rng::Rng;
use crate::ty::{IntTy, Prim, Ty, ValTy};
use crate::Knobs;

pub const FN_NAMES: &[&str] = &[
    "Compute", "Blend", "Scale", "Shift", "Measure", "Combine", "Adjust", "Weigh", "Fold", "Probe",
    "Twist", "Gather", "Refine", "Settle", "Sample", "Tally",
];
pub const STRUCT_NAMES: &[&str] = &[
    "Point", "Pair", "Rect", "Node", "Entry", "Frame", "Token", "Record", "Slot", "Grid",
];
pub const FIELD_NAMES: &[&str] = &[
    "A", "B", "X", "Y", "Count", "Total", "Flag", "Tag", "Left", "Right", "Depth", "Weight",
    "Inner", "Outer",
];
pub const CONST_NAMES: &[&str] = &[
    "LIMIT", "SCALE", "STEP", "BASE", "MASK", "OFFSET", "RATIO", "SEED", "WIDTH", "BIAS",
];

/// Characters a generated `char` literal or value may hold: printable ASCII without the
/// quote and backslash, so no escaping is ever needed.
pub const CHARS: &[char] = &[
    'a', 'b', 'c', 'x', 'y', 'z', 'A', 'M', 'Z', '0', '7', '9', ' ', '!', '#', '*', '+', '-', '.',
    '/', ':', '=', '?', '@', '_', '~',
];

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: ValTy,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub copy: bool,
    pub fields: Vec<Field>,
}

/// A constant's value, known to the generator so initialisers never overflow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstValue {
    Int(i128),
    Float(f64),
    Bool(bool),
    Char(char),
}

#[derive(Debug, Clone)]
pub struct ConstDef {
    pub name: String,
    pub ty: Prim,
    pub value: ConstValue,
    /// The initialiser as source text.
    pub init: String,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub mutable: bool,
    pub ty: Ty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetTy {
    Unit,
    Val(ValTy),
    /// A reference into the single reference parameter's referent (Rust's one elision rule
    /// both compilers share).
    Ref {
        mutable: bool,
        inner: ValTy,
    },
}

impl RetTy {
    pub fn as_ty(self) -> Option<Ty> {
        match self {
            RetTy::Unit => None,
            RetTy::Val(v) => Some(Ty::Val(v)),
            RetTy::Ref { mutable, inner } => Some(Ty::Ref { mutable, inner }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FnSig {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: RetTy,
}

/// The skeleton of a program. Function `i` may only call functions `< i`, which rules out
/// recursion; `Main` is last.
#[derive(Debug, Clone)]
pub struct Program {
    pub structs: Vec<StructDef>,
    /// The tuple types the program uses, by element types; `ValTy::Tuple(i)` is
    /// `tuples[i]`. Never empty tuples, and no tuple contains itself.
    pub tuples: Vec<Vec<ValTy>>,
    pub consts: Vec<ConstDef>,
    pub fns: Vec<FnSig>,
}

impl Program {
    pub fn is_copy(&self, ty: Ty) -> bool {
        match ty {
            Ty::Val(ValTy::Prim(_)) | Ty::Ref { mutable: false, .. } => true,
            Ty::Val(ValTy::Struct(s)) => self.structs[s].copy,
            Ty::Val(ValTy::Tuple(t)) => self.tuples[t].iter().all(|e| self.is_copy(Ty::Val(*e))),
            Ty::Ref { mutable: true, .. } => false,
        }
    }

    pub fn val_name(&self, v: ValTy) -> String {
        match v {
            ValTy::Prim(p) => p.name().to_owned(),
            ValTy::Struct(s) => self.structs[s].name.clone(),
            ValTy::Tuple(t) => {
                let elems: Vec<String> = self.tuples[t].iter().map(|e| self.val_name(*e)).collect();
                if elems.len() == 1 {
                    format!("({},)", elems[0])
                } else {
                    format!("({})", elems.join(", "))
                }
            }
        }
    }

    pub fn ty_name(&self, ty: Ty) -> String {
        match ty {
            Ty::Val(v) => self.val_name(v),
            Ty::Ref { mutable, inner } => {
                format!(
                    "&{}{}",
                    if mutable { "mut " } else { "" },
                    self.val_name(inner)
                )
            }
        }
    }

    pub fn ret_name(&self, ret: RetTy) -> String {
        match ret.as_ty() {
            None => String::new(),
            Some(ty) => format!(" -> {}", self.ty_name(ty)),
        }
    }

    /// All primitive leaves of a value, as `(path suffix, type)`: `("", p)` for a
    /// primitive, `(".A.X", i32)` for nested struct fields.
    pub fn leaves(&self, v: ValTy) -> Vec<(String, Prim)> {
        let mut out = Vec::new();
        self.collect_leaves(v, String::new(), &mut out);
        out
    }

    fn collect_leaves(&self, v: ValTy, prefix: String, out: &mut Vec<(String, Prim)>) {
        match v {
            ValTy::Prim(p) => out.push((prefix, p)),
            ValTy::Struct(s) => {
                for f in &self.structs[s].fields {
                    self.collect_leaves(f.ty, format!("{prefix}.{}", f.name), out);
                }
            }
            ValTy::Tuple(t) => {
                for (i, e) in self.tuples[t].iter().enumerate() {
                    self.collect_leaves(*e, format!("{prefix}.{i}"), out);
                }
            }
        }
    }

    /// All sub-places of a value of type `v`, including the value itself (`""`), with their
    /// types, up to a nesting depth of three.
    pub fn sub_places(&self, v: ValTy) -> Vec<(String, ValTy)> {
        let mut out = Vec::new();
        self.collect_sub_places(v, "", 0, &mut out);
        out
    }

    fn collect_sub_places(
        &self,
        v: ValTy,
        prefix: &str,
        depth: u32,
        out: &mut Vec<(String, ValTy)>,
    ) {
        out.push((prefix.to_owned(), v));
        if depth >= 3 {
            return;
        }
        match v {
            ValTy::Struct(s) => {
                for f in &self.structs[s].fields {
                    self.collect_sub_places(f.ty, &format!("{prefix}.{}", f.name), depth + 1, out);
                }
            }
            ValTy::Tuple(t) => {
                for (i, e) in self.tuples[t].iter().enumerate() {
                    self.collect_sub_places(*e, &format!("{prefix}.{i}"), depth + 1, out);
                }
            }
            ValTy::Prim(_) => {}
        }
    }

    /// Renders the items that precede the function bodies.
    pub fn render_items(&self) -> String {
        let mut out = String::new();
        for s in &self.structs {
            if s.copy {
                out.push_str("#[derive(Copy, Clone)]\n");
            }
            let _ = writeln!(out, "struct {} {{", s.name);
            for f in &s.fields {
                let _ = writeln!(out, "    {}: {},", f.name, self.val_name(f.ty));
            }
            out.push_str("}\n\n");
        }
        for c in &self.consts {
            let _ = writeln!(out, "const {}: {} = {};", c.name, c.ty.name(), c.init);
        }
        if !self.consts.is_empty() {
            out.push('\n');
        }
        out
    }

    pub fn render_sig(&self, f: &FnSig) -> String {
        let params: Vec<String> = f
            .params
            .iter()
            .map(|p| {
                format!(
                    "{}{}: {}",
                    if p.mutable { "mut " } else { "" },
                    p.name,
                    self.ty_name(p.ty)
                )
            })
            .collect();
        format!(
            "fn {}({}){} {{",
            f.name,
            params.join(", "),
            self.ret_name(f.ret)
        )
    }
}

/// Picks `name` from `pool` without repeating a name already in `used`, appending a
/// counter when the pool is exhausted.
pub fn unique_name(rng: &mut Rng, pool: &[&str], used: &mut Vec<String>) -> String {
    let free: Vec<&&str> = pool
        .iter()
        .filter(|n| !used.iter().any(|u| u == *n))
        .collect();
    let name = if free.is_empty() {
        let base = rng.pick(pool);
        let mut i = 2;
        loop {
            let candidate = format!("{base}{i}");
            if !used.contains(&candidate) {
                break candidate;
            }
            i += 1;
        }
    } else {
        (**rng.pick(&free)).to_owned()
    };
    used.push(name.clone());
    name
}

/// A random primitive type, integers most often.
pub fn random_prim(rng: &mut Rng) -> Prim {
    match rng.weighted(&[60, 10, 10, 12, 8]) {
        0 => Prim::Int(*rng.pick(&IntTy::ALL)),
        1 => Prim::F32,
        2 => Prim::F64,
        3 => Prim::Bool,
        _ => Prim::Char,
    }
}

/// The source text of an integer literal of type `ty` with value `v` (always suffixed, so
/// the type never depends on inference).
pub fn int_literal(ty: IntTy, v: i128) -> String {
    debug_assert!(ty.contains(v));
    format!("{v}{}", ty.name())
}

/// A random value of an integer type: small values most of the time (so arithmetic on them
/// rarely overflows), occasionally a boundary or a random bit pattern.
pub fn random_int(rng: &mut Rng, ty: IntTy) -> i128 {
    match rng.weighted(&[66, 16, 6, 6, 6]) {
        0 => {
            let hi = if ty.bits() == 8 { 9 } else { 20 };
            i128::from(rng.range(0, hi))
        }
        1 => {
            let hi = i64::try_from(ty.max().min(1000)).expect("fits");
            let lo = i64::try_from(ty.min().max(-1000)).expect("fits");
            i128::from(rng.range(lo, hi))
        }
        2 => *rng.pick(&[ty.max(), ty.min(), ty.max() - 1, ty.min() + 1]),
        3 => random_bits(rng, ty.bits() - 1),
        _ => match ty {
            // All 128 bits, reinterpreted as a signed value.
            #[allow(clippy::cast_possible_wrap)]
            IntTy::I128 => (u128::from(rng.next_u64()) << 64 | u128::from(rng.next_u64())) as i128,
            // The generator's `u128` values stay below `2^127` (see `IntTy::max`).
            IntTy::U128 => random_bits(rng, 127),
            _ => ty.wrap(i128::from(rng.bits(ty.bits()))),
        },
    }
}

/// A non-negative value of `bits` random bits (`bits <= 127`); the same draws as
/// `rng.bits` up to 64 bits.
fn random_bits(rng: &mut Rng, bits: u32) -> i128 {
    debug_assert!(bits <= 127);
    if bits <= 64 {
        i128::from(rng.bits(bits))
    } else {
        let high = i128::from(rng.bits(bits - 64));
        let low = i128::from(rng.next_u64());
        high << 64 | low
    }
}

pub const FLOAT_LITERALS: &[&str] = &[
    "0.0", "0.5", "1.0", "1.5", "2.0", "2.5", "3.75", "10.0", "0.25", "100.0", "0.1", "7.0",
    "-1.5", "1234.5", "0.001", "-0.0", "64.0", "-33.25",
];

pub fn float_literal(rng: &mut Rng, p: Prim) -> (String, f64) {
    let text = *rng.pick(FLOAT_LITERALS);
    let value: f64 = text.parse().expect("float literal");
    let value = if p == Prim::F32 {
        // The literal's value as an `f32`, then widened; `Debug` output round-trips.
        #[allow(clippy::cast_possible_truncation)]
        let f = value as f32;
        f64::from(f)
    } else {
        value
    };
    (format!("{text}{}", p.name()), value)
}

/// A random literal of a primitive type, with its value.
pub fn random_literal(rng: &mut Rng, p: Prim) -> (String, ConstValue) {
    match p {
        Prim::Int(i) => {
            let v = random_int(rng, i);
            (int_literal(i, v), ConstValue::Int(v))
        }
        Prim::F32 | Prim::F64 => {
            let (text, v) = float_literal(rng, p);
            (text, ConstValue::Float(v))
        }
        Prim::Bool => {
            let b = rng.chance(50);
            (b.to_string(), ConstValue::Bool(b))
        }
        Prim::Char => {
            let c = *rng.pick(CHARS);
            (format!("'{c}'"), ConstValue::Char(c))
        }
    }
}

/// Generates the structs: each struct's fields are primitives, earlier structs or tuples
/// of primitives (`program.tuples` holds those so far), and a `Copy` struct only contains
/// `Copy` fields.
fn gen_structs(rng: &mut Rng, knobs: &Knobs, program: &mut Program) {
    let count = rng.index(knobs.max_structs + 1);
    let mut used = Vec::new();
    for _ in 0..count {
        let name = unique_name(rng, STRUCT_NAMES, &mut used);
        let copy = rng.chance(50);
        let n_fields = rng.range(1, 4);
        let mut field_names = Vec::new();
        let mut fields = Vec::new();
        for _ in 0..n_fields {
            let fname = unique_name(rng, FIELD_NAMES, &mut field_names);
            let candidates: Vec<usize> = program
                .structs
                .iter()
                .enumerate()
                .filter(|(_, s)| !copy || s.copy)
                .map(|(i, _)| i)
                .collect();
            let ty = if !candidates.is_empty() && rng.chance(30) {
                ValTy::Struct(*rng.pick(&candidates))
            } else if !program.tuples.is_empty() && rng.chance(15) {
                // Tuples of primitives so far: always `Copy`.
                ValTy::Tuple(rng.index(program.tuples.len()))
            } else {
                ValTy::Prim(random_prim(rng))
            };
            fields.push(Field { name: fname, ty });
        }
        program.structs.push(StructDef { name, copy, fields });
    }
}

/// Generates up to `count` tuple types. With `aggregates`, elements may also be structs and
/// earlier tuples (nesting); otherwise they are primitives. Now and then a tuple is wider
/// than seven elements, past `System.ValueTuple`'s direct fields.
fn gen_tuples(rng: &mut Rng, count: usize, aggregates: bool, program: &mut Program) {
    for _ in 0..count {
        let n = if rng.chance(8) {
            rng.range(8, 10)
        } else {
            rng.range(1, 4)
        };
        let mut elems = Vec::new();
        for _ in 0..n {
            let ty = match rng.weighted(&[
                70,
                if aggregates && !program.structs.is_empty() {
                    15
                } else {
                    0
                },
                if aggregates && !program.tuples.is_empty() {
                    15
                } else {
                    0
                },
            ]) {
                0 => ValTy::Prim(random_prim(rng)),
                1 => ValTy::Struct(rng.index(program.structs.len())),
                _ => ValTy::Tuple(rng.index(program.tuples.len())),
            };
            elems.push(ty);
        }
        program.tuples.push(elems);
    }
}

fn eval_int_binop(op: &str, a: i128, b: i128, ty: IntTy) -> Option<i128> {
    let r = match op {
        "+" => a.checked_add(b)?,
        "-" => a.checked_sub(b)?,
        "*" => a.checked_mul(b)?,
        "/" => {
            if b == 0 {
                return None;
            }
            a.checked_div(b)?
        }
        "%" => {
            if b == 0 {
                return None;
            }
            a.checked_rem(b)?
        }
        "&" => a & b,
        "|" => a | b,
        "^" => a ^ b,
        _ => return None,
    };
    ty.contains(r).then_some(r)
}

/// Generates the constants. Every initialiser is evaluated here with checked arithmetic,
/// because an overflowing or zero-dividing constant is a hard error in both compilers
/// (not a borrow verdict, and not a run-time panic).
fn gen_consts(rng: &mut Rng, knobs: &Knobs) -> Vec<ConstDef> {
    let count = rng.index(knobs.max_consts + 1);
    let mut used = Vec::new();
    let mut consts: Vec<ConstDef> = Vec::new();
    for _ in 0..count {
        let name = unique_name(rng, CONST_NAMES, &mut used);
        let ty = random_prim(rng);
        let (mut init, mut value) = random_literal(rng, ty);
        if let Prim::Int(ity) = ty {
            // Sometimes an arithmetic expression over literals and earlier constants.
            let same_ty: Vec<&ConstDef> = consts.iter().filter(|c| c.ty == ty).collect();
            if rng.chance(50) {
                let (lhs_text, lhs) = if !same_ty.is_empty() && rng.chance(50) {
                    let c = rng.pick(&same_ty);
                    let ConstValue::Int(v) = c.value else {
                        unreachable!()
                    };
                    (c.name.clone(), v)
                } else {
                    let v = random_int(rng, ity);
                    (int_literal(ity, v), v)
                };
                let rhs = i128::from(rng.range(1, 12));
                let op = *rng.pick(&["+", "-", "*", "/", "%", "&", "|", "^"]);
                if let Some(r) = eval_int_binop(op, lhs, rhs, ity) {
                    init = format!("{lhs_text} {op} {}", int_literal(ity, rhs));
                    value = ConstValue::Int(r);
                }
            } else if rng.chance(40) {
                // A wrapping cast from another integer type.
                let from = *rng.pick(&IntTy::ALL);
                let v = random_int(rng, from);
                // A negative value as `u128` is outside the generator's value model.
                if let Some(wrapped) = ity.wrap_checked(v) {
                    init = format!("{} as {}", int_literal(from, v), ity.name());
                    value = ConstValue::Int(wrapped);
                }
            }
        } else if ty == Prim::Bool && rng.chance(50) {
            let ity = *rng.pick(&IntTy::ALL);
            let a = random_int(rng, ity);
            let b = random_int(rng, ity);
            let ops: [(&str, bool); 6] = [
                ("<", a < b),
                ("<=", a <= b),
                (">", a > b),
                (">=", a >= b),
                ("==", a == b),
                ("!=", a != b),
            ];
            let (op, r) = *rng.pick(&ops);
            init = format!("{} {op} {}", int_literal(ity, a), int_literal(ity, b));
            value = ConstValue::Bool(r);
        }
        consts.push(ConstDef {
            name,
            ty,
            value,
            init,
        });
    }
    consts
}

/// A random parameter or local type.
fn random_ty(rng: &mut Rng, program: &Program, allow_refs: bool) -> Ty {
    let has_structs = !program.structs.is_empty();
    let has_tuples = !program.tuples.is_empty();
    let choice = rng.weighted(&[
        55,
        if has_structs { 15 } else { 0 },
        if has_tuples { 12 } else { 0 },
        if allow_refs { 15 } else { 0 },
        if allow_refs { 15 } else { 0 },
    ]);
    let inner = |rng: &mut Rng| {
        if has_tuples && rng.chance(20) {
            ValTy::Tuple(rng.index(program.tuples.len()))
        } else if has_structs && rng.chance(60) {
            ValTy::Struct(rng.index(program.structs.len()))
        } else {
            ValTy::Prim(random_prim(rng))
        }
    };
    match choice {
        0 => Ty::prim(random_prim(rng)),
        1 => Ty::Val(ValTy::Struct(rng.index(program.structs.len()))),
        2 => Ty::Val(ValTy::Tuple(rng.index(program.tuples.len()))),
        3 => Ty::Ref {
            mutable: false,
            inner: inner(rng),
        },
        _ => Ty::Ref {
            mutable: true,
            inner: inner(rng),
        },
    }
}

pub const LOCAL_NAMES: &[&str] = &[
    "a", "b", "c", "d", "n", "k", "m", "total", "sum", "acc", "delta", "flag", "tmp", "val",
    "item", "left", "right", "cur", "prev", "next",
];

fn gen_sig(rng: &mut Rng, program: &Program, name: String, is_main: bool) -> FnSig {
    if is_main {
        return FnSig {
            name,
            params: Vec::new(),
            ret: RetTy::Unit,
        };
    }
    let n_params = rng.range(0, 3);
    let mut used = Vec::new();
    let mut params = Vec::new();
    for _ in 0..n_params {
        let pname = unique_name(rng, LOCAL_NAMES, &mut used);
        let ty = random_ty(rng, program, true);
        let mutable = !ty.is_ref() && rng.chance(25);
        params.push(Param {
            name: pname,
            mutable,
            ty,
        });
    }
    let ref_params: Vec<&Param> = params.iter().filter(|p| p.ty.is_ref()).collect();
    let ret = match rng.weighted(&[
        25,
        50,
        if program.structs.is_empty() { 0 } else { 12 },
        if ref_params.len() == 1 { 25 } else { 0 },
        if program.tuples.is_empty() { 0 } else { 12 },
    ]) {
        0 => RetTy::Unit,
        1 => RetTy::Val(ValTy::Prim(random_prim(rng))),
        2 => RetTy::Val(ValTy::Struct(rng.index(program.structs.len()))),
        4 => RetTy::Val(ValTy::Tuple(rng.index(program.tuples.len()))),
        _ => {
            let Ty::Ref { mutable, inner } = ref_params[0].ty else {
                unreachable!()
            };
            let targets = program.sub_places(inner);
            let (_, target) = rng.pick(&targets);
            RetTy::Ref {
                mutable: mutable && rng.chance(60),
                inner: *target,
            }
        }
    };
    FnSig { name, params, ret }
}

/// Generates the whole skeleton: tuples of primitives, structs (which may hold them), more
/// tuples over everything so far, constants, then the signatures.
pub fn gen_program(rng: &mut Rng, knobs: &Knobs) -> Program {
    let mut program = Program {
        structs: Vec::new(),
        tuples: Vec::new(),
        consts: Vec::new(),
        fns: Vec::new(),
    };
    let count = rng.index(knobs.max_tuples / 2 + 1);
    gen_tuples(rng, count, false, &mut program);
    gen_structs(rng, knobs, &mut program);
    let count = rng.index(knobs.max_tuples.saturating_sub(program.tuples.len()) + 1);
    gen_tuples(rng, count, true, &mut program);
    program.consts = gen_consts(rng, knobs);
    let n_fns = rng.range(
        i64::try_from(knobs.min_fns.max(1)).expect("fits"),
        i64::try_from(knobs.max_fns.max(knobs.min_fns).max(1)).expect("fits"),
    );
    let mut used = Vec::new();
    for i in 0..n_fns {
        let is_main = i == n_fns - 1;
        let name = if is_main {
            "Main".to_owned()
        } else {
            unique_name(rng, FN_NAMES, &mut used)
        };
        let sig = gen_sig(rng, &program, name, is_main);
        program.fns.push(sig);
    }
    program
}
