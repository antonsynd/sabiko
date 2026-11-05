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
/// Names of methods and associated functions; `Count` and `Total` are also field names
/// (fields and methods are separate namespaces).
pub const METHOD_NAMES: &[&str] = &[
    "Get", "Bump", "Peek", "Grow", "Mix", "Take", "Reset", "Make", "Build", "Merge", "Count",
    "Total", "Nudge", "Pick",
];
pub const STRUCT_NAMES: &[&str] = &[
    "Point", "Pair", "Rect", "Node", "Entry", "Frame", "Token", "Record", "Slot", "Grid",
];
pub const FIELD_NAMES: &[&str] = &[
    "A", "B", "X", "Y", "Count", "Total", "Flag", "Tag", "Left", "Right", "Depth", "Weight",
    "Inner", "Outer",
];
/// Enum names, disjoint from the struct and field names (types share a namespace, and a
/// field name can become a local through field shorthand in a struct pattern).
pub const ENUM_NAMES: &[&str] = &[
    "Shape", "State", "Mode", "Kind", "Signal", "Event", "Phase", "Choice", "Status", "Outcome",
];
/// Variant names; variants are always written qualified (`E::Idle`).
pub const VARIANT_NAMES: &[&str] = &[
    "Idle", "Busy", "Done", "Empty", "Single", "Both", "Named", "Up", "Down", "Ready", "Failed",
    "Waiting", "Some", "None", "Only",
];
pub const CONST_NAMES: &[&str] = &[
    "LIMIT", "SCALE", "STEP", "BASE", "MASK", "OFFSET", "RATIO", "SEED", "WIDTH", "BIAS",
];
/// Names of the `usize` constants that array lengths are written with (`[i32; LEN]`).
pub const LEN_NAMES: &[&str] = &["LEN", "SIZE", "CAP", "SLOTS", "ROWS", "COLS"];

/// Characters a generated `char` literal or value may hold: printable ASCII without the
/// quote and backslash, so no escaping is ever needed.
pub const CHARS: &[char] = &[
    'a', 'b', 'c', 'x', 'y', 'z', 'A', 'M', 'Z', '0', '7', '9', ' ', '!', '#', '*', '+', '-', '.',
    '/', ':', '=', '?', '@', '_', '~',
];

/// String literals (#10) as source text. TODO(decision 62): all of them are ASCII, since
/// Sabiko's strings are UTF-16, so `len()` and the indices of a range differ from rustc's
/// UTF-8 bytes on other text (by design, not a bug; no generated string may hold a
/// non-ASCII character, escapes included). The words share letters, so that comparisons, slices and literal patterns
/// often meet; a few hold escapes (`\n`, `\t`, `\\`, `\"`, `\'`, `\0`, `\x41`,
/// `\u{61}`), and none a semicolon or a brace outside `\u{..}` (the line-based tools
/// look at line ends).
pub const STR_LITERALS: &[&str] = &[
    "", "a", "b", "c", "ab", "ba", "bc", "abc", "cab", "abcab", "xyz", "key", "val", "key=val",
    "A", "Ab", "a b", "hello", "Hello!", "0", "10", "9", "~", "a-b", "--x--", "  ",
];
pub const STR_ESCAPED: &[&str] = &[
    "tab\\there",
    "line\\nbreak",
    "q\\\"t",
    "back\\\\",
    "it\\'s",
    "\\x41\\x62c",
    "\\u{61}z",
    "nul\\0",
    "\\t",
    "\\n",
    "a\\nb",
];

/// The length (in bytes, all ASCII) of the value of a string literal's source text: the
/// escapes [`STR_ESCAPED`] uses count one each.
pub fn str_len(src: &str) -> usize {
    let bytes = src.as_bytes();
    let (mut i, mut n) = (0, 0);
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += match bytes[i + 1] {
                b'x' => 4,
                b'u' => src[i..].find('}').expect("a closing brace") + 1,
                _ => 2,
            };
        } else {
            i += 1;
        }
        n += 1;
    }
    n
}

/// A random string literal, with quotes, and its length.
pub fn str_literal(rng: &mut Rng) -> (String, usize) {
    let src = if rng.chance(12) {
        *rng.pick(STR_ESCAPED)
    } else {
        *rng.pick(STR_LITERALS)
    };
    (format!("\"{src}\""), str_len(src))
}

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
    /// `Some(path)` for a struct with an `impl Drop` (never a `Copy` one): its `drop`
    /// prints the struct's name and the primitive leaf `self{path}`, so the output shows
    /// which value was dropped when.
    pub drop: Option<String>,
}

/// The payload of an enum variant.
#[derive(Debug, Clone)]
pub enum VariantKind {
    Unit,
    Tuple(Vec<ValTy>),
    Struct(Vec<Field>),
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    pub kind: VariantKind,
    /// An explicit discriminant (`A = 3`), only in fieldless enums.
    pub discriminant: Option<i128>,
}

impl Variant {
    /// The payload's element types, with the field names of a struct variant.
    pub fn elems(&self) -> Vec<(Option<&str>, ValTy)> {
        match &self.kind {
            VariantKind::Unit => Vec::new(),
            VariantKind::Tuple(elems) => elems.iter().map(|t| (None, *t)).collect(),
            VariantKind::Struct(fields) => fields
                .iter()
                .map(|f| (Some(f.name.as_str()), f.ty))
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub copy: bool,
    pub variants: Vec<Variant>,
    /// An `impl Drop` (never on a `Copy` or fieldless enum): its `drop` matches on `self` and
    /// prints the variant's name and its first primitive payload element.
    pub drop: bool,
}

impl EnumDef {
    /// A one-line match arm for variant `v` of a reference to this enum that prints
    /// `prefix`, the variant's path and its first primitive payload element (bound as
    /// `binding`), if any.
    pub fn print_arm(&self, v: &Variant, binding: &str, prefix: &str) -> String {
        let path = format!("{}::{}", self.name, v.name);
        let first_prim = v
            .elems()
            .iter()
            .position(|(_, t)| matches!(t, ValTy::Prim(_)));
        let Some(i) = first_prim else {
            let pat = match v.kind {
                VariantKind::Unit => path.clone(),
                VariantKind::Tuple(_) => format!("{path}(..)"),
                VariantKind::Struct(_) => format!("{path} {{ .. }}"),
            };
            return format!("{pat} => PrintLine(\"{prefix}{path}\"),");
        };
        let pat = if let VariantKind::Struct(fields) = &v.kind {
            format!("{path} {{ {}: {binding}, .. }}", fields[i].name)
        } else {
            let mut parts = vec!["_"; i];
            parts.push(binding);
            format!("{path}({}, ..)", parts.join(", "))
        };
        format!("{pat} => {{ Print(\"{prefix}{path} \"); PrintLine(*{binding}); }}")
    }

    /// Only unit variants: such an enum casts to integers with `as`.
    pub fn fieldless(&self) -> bool {
        self.variants
            .iter()
            .all(|v| matches!(v.kind, VariantKind::Unit))
    }
}

/// An array type `[T; N]` (`len: Some(N)`), or a slice type `[T]` (`len: None`), which is
/// unsized and only appears behind a reference.
#[derive(Debug, Clone)]
pub struct ArrayDef {
    pub elem: ValTy,
    pub len: Option<usize>,
    /// The length as the type spells it: a literal (`3`, `3usize`), a `usize` constant
    /// (`LEN`) or an expression over one (`LEN + 1usize`). Empty for a slice.
    pub len_text: String,
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

/// How a method takes its receiver, `params[0]` (named `self`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    /// `self` (`mut self` when the parameter is mutable).
    Value,
    /// `&self` or `&mut self`.
    Ref { mutable: bool },
}

/// The struct or enum an associated function's `impl` block is for (`ValTy::Struct` or
/// `ValTy::Enum`), and its receiver (`None` for an associated function without `self`,
/// such as a constructor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub ty: ValTy,
    pub receiver: Option<Receiver>,
}

#[derive(Debug, Clone)]
pub struct FnSig {
    pub name: String,
    /// The receiver first, for a method.
    pub params: Vec<Param>,
    pub ret: RetTy,
    /// `Some` for a function of an `impl` block, rendered in an `impl` block of its own
    /// and called as `Type::Name(..)` or, with a receiver, as a method.
    pub owner: Option<Owner>,
}

/// The skeleton of a program. Function `i` may only call functions `< i`, which rules out
/// recursion; `Main` is last. Methods and associated functions are functions with an
/// [`Owner`].
#[derive(Debug, Clone)]
pub struct Program {
    pub structs: Vec<StructDef>,
    /// Enums; a payload only holds primitives, tuples, structs and earlier enums.
    pub enums: Vec<EnumDef>,
    /// The tuple types the program uses, by element types; `ValTy::Tuple(i)` is
    /// `tuples[i]`. Never empty tuples, and no tuple contains itself.
    pub tuples: Vec<Vec<ValTy>>,
    /// The array and slice types (#9); `ValTy::Array(i)` is `arrays[i]`. No two have the
    /// same element type and length, and no element type is a slice.
    pub arrays: Vec<ArrayDef>,
    pub consts: Vec<ConstDef>,
    pub fns: Vec<FnSig>,
}

impl Program {
    pub fn is_copy(&self, ty: Ty) -> bool {
        match ty {
            Ty::Val(ValTy::Prim(_)) | Ty::Ref { mutable: false, .. } => true,
            Ty::Val(ValTy::Struct(s)) => self.structs[s].copy,
            Ty::Val(ValTy::Tuple(t)) => self.tuples[t].iter().all(|e| self.is_copy(Ty::Val(*e))),
            Ty::Val(ValTy::Enum(e)) => self.enums[e].copy,
            // A slice is never a value; an array is `Copy` when its elements are.
            Ty::Val(ValTy::Array(a)) => {
                self.arrays[a].len.is_some() && self.is_copy(Ty::Val(self.arrays[a].elem))
            }
            // Nor is a `str`.
            Ty::Val(ValTy::Str) | Ty::Ref { mutable: true, .. } => false,
        }
    }

    /// Whether values of type `v` have a size: everything but a slice and `str`.
    pub fn is_sized(&self, v: ValTy) -> bool {
        !matches!(v, ValTy::Str) && !matches!(v, ValTy::Array(a) if self.arrays[a].len.is_none())
    }

    /// The element type and length (`None` for a slice) of an array or slice type.
    pub fn array_of(&self, v: ValTy) -> Option<(ValTy, Option<usize>)> {
        match v {
            ValTy::Array(a) => Some((self.arrays[a].elem, self.arrays[a].len)),
            _ => None,
        }
    }

    /// The slice type `[elem]`, if the program has one.
    pub fn slice_of(&self, elem: ValTy) -> Option<ValTy> {
        self.arrays
            .iter()
            .position(|a| a.len.is_none() && a.elem == elem)
            .map(ValTy::Array)
    }

    /// Whether a reference to a `from` coerces to a reference to a `to` where the expected
    /// type is known: the same type, or an array and a slice of its element type (`&[T; N]`
    /// to `&[T]`).
    pub fn ref_coerces(&self, from: ValTy, to: ValTy) -> bool {
        from == to
            || matches!(
                (self.array_of(from), self.array_of(to)),
                (Some((a, Some(_))), Some((b, None))) if a == b
            )
    }

    /// Whether a value of type `v` is a struct or enum with `impl Drop`: nothing may be
    /// moved out of it (E0509).
    pub fn has_drop_impl(&self, v: ValTy) -> bool {
        match v {
            ValTy::Struct(s) => self.structs[s].drop.is_some(),
            ValTy::Enum(e) => self.enums[e].drop,
            ValTy::Prim(_) | ValTy::Tuple(_) | ValTy::Array(_) | ValTy::Str => false,
        }
    }

    /// Whether a value of type `v` has drop glue: an `impl Drop` in it, at any depth.
    pub fn needs_drop(&self, v: ValTy) -> bool {
        match v {
            ValTy::Prim(_) | ValTy::Str => false,
            ValTy::Struct(s) => {
                let def = &self.structs[s];
                def.drop.is_some() || def.fields.iter().any(|f| self.needs_drop(f.ty))
            }
            ValTy::Enum(e) => {
                let def = &self.enums[e];
                def.drop
                    || def
                        .variants
                        .iter()
                        .any(|var| var.elems().iter().any(|&(_, t)| self.needs_drop(t)))
            }
            ValTy::Tuple(t) => self.tuples[t].iter().any(|&e| self.needs_drop(e)),
            ValTy::Array(a) => self.needs_drop(self.arrays[a].elem),
        }
    }

    pub fn val_name(&self, v: ValTy) -> String {
        match v {
            ValTy::Prim(p) => p.name().to_owned(),
            ValTy::Str => "str".to_owned(),
            ValTy::Struct(s) => self.structs[s].name.clone(),
            ValTy::Enum(e) => self.enums[e].name.clone(),
            ValTy::Array(a) => {
                let def = &self.arrays[a];
                match def.len {
                    Some(_) => format!("[{}; {}]", self.val_name(def.elem), def.len_text),
                    None => format!("[{}]", self.val_name(def.elem)),
                }
            }
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
    /// primitive, `(".A.X", i32)` for nested struct fields, `("[1].0", u8)` for an array
    /// element. An enum has none (its payload is not a field path), nor has a slice (its
    /// length is not known).
    pub fn leaves(&self, v: ValTy) -> Vec<(String, Prim)> {
        let mut out = Vec::new();
        self.collect_leaves(v, String::new(), &mut out);
        out
    }

    fn collect_leaves(&self, v: ValTy, prefix: String, out: &mut Vec<(String, Prim)>) {
        match v {
            ValTy::Prim(p) => out.push((prefix, p)),
            ValTy::Enum(_) | ValTy::Str => {}
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
            ValTy::Array(a) => {
                let def = &self.arrays[a];
                for k in 0..def.len.unwrap_or(0) {
                    self.collect_leaves(def.elem, format!("{prefix}[{k}]"), out);
                }
            }
        }
    }

    /// Whether the sub-place `path` (as [`Program::sub_places`] spells it) of a value of
    /// type `root` cannot be moved out of: it lies inside a struct with `impl Drop` (rustc's
    /// E0509: `.Inner` of such a struct, `.A.Inner` of a plain struct whose `A` is one), or
    /// in an array or slice element (E0508: `[1]`, `[1].X`). The value itself (`""`) can.
    pub fn moves_out_of_drop(&self, root: ValTy, path: &str) -> bool {
        let mut ty = root;
        for seg in path_segments(path) {
            if seg.starts_with('[') {
                return true;
            }
            let seg = &seg[1..];
            ty = match ty {
                ValTy::Struct(s) => {
                    let def = &self.structs[s];
                    if def.drop.is_some() {
                        return true;
                    }
                    def.fields
                        .iter()
                        .find(|f| f.name == seg)
                        .expect("a field of the struct")
                        .ty
                }
                ValTy::Tuple(t) => self.tuples[t][seg.parse::<usize>().expect("an index")],
                ValTy::Prim(_) | ValTy::Enum(_) | ValTy::Array(_) | ValTy::Str => {
                    unreachable!("primitives, enums, arrays and strings have no fields")
                }
            };
        }
        false
    }

    /// The lengths of the arrays (`None` for slices) that the index segments of the
    /// sub-place `path` of a value of type `root` index, in order.
    pub fn path_lens(&self, root: ValTy, path: &str) -> Vec<Option<usize>> {
        let mut ty = root;
        let mut out = Vec::new();
        for seg in path_segments(path) {
            ty = match ty {
                ValTy::Array(a) => {
                    out.push(self.arrays[a].len);
                    self.arrays[a].elem
                }
                ValTy::Struct(s) => {
                    self.structs[s]
                        .fields
                        .iter()
                        .find(|f| f.name == seg[1..])
                        .expect("a field of the struct")
                        .ty
                }
                ValTy::Tuple(t) => self.tuples[t][seg[1..].parse::<usize>().expect("an index")],
                ValTy::Prim(_) | ValTy::Enum(_) | ValTy::Str => unreachable!("no sub-places"),
            };
        }
        out
    }

    /// All sub-places of a value of type `v`, including the value itself (`""`), with their
    /// types, up to a nesting depth of three. An enum's payload is reached by patterns only.
    /// The elements of an array are reached with constant indices (`[0]`, `[1].X`), those
    /// of a slice through the placeholder `[?]`, which the user of the path replaces with an
    /// index expression (the length is not known).
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
            ValTy::Array(a) => {
                let def = &self.arrays[a];
                let indices: Vec<String> = match def.len {
                    Some(n) => (0..n).map(|k| k.to_string()).collect(),
                    None => vec!["?".to_owned()],
                };
                for k in indices {
                    self.collect_sub_places(def.elem, &format!("{prefix}[{k}]"), depth + 1, out);
                }
            }
            ValTy::Prim(_) | ValTy::Enum(_) | ValTy::Str => {}
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
            if let Some(leaf) = &s.drop {
                let _ = writeln!(
                    out,
                    "impl Drop for {0} {{\n    fn drop(&mut self) {{\n        Print(\"drop {0} \");\n        PrintLine(self{leaf});\n    }}\n}}\n",
                    s.name
                );
            }
        }
        for e in &self.enums {
            self.render_enum(e, &mut out);
        }
        for c in &self.consts {
            let _ = writeln!(out, "const {}: {} = {};", c.name, c.ty.name(), c.init);
        }
        if !self.consts.is_empty() {
            out.push('\n');
        }
        out
    }

    /// An enum on one line per variant (no line ends in `{` or `;`, so no statement is
    /// ever inserted there), then its `impl Drop`, which matches on `self`.
    fn render_enum(&self, e: &EnumDef, out: &mut String) {
        if e.copy {
            out.push_str("#[derive(Copy, Clone)]\n");
        }
        let _ = writeln!(out, "enum {} {{", e.name);
        for v in &e.variants {
            let payload = match &v.kind {
                VariantKind::Unit => String::new(),
                VariantKind::Tuple(elems) => {
                    let elems: Vec<String> = elems.iter().map(|t| self.val_name(*t)).collect();
                    format!("({})", elems.join(", "))
                }
                VariantKind::Struct(fields) => {
                    let fields: Vec<String> = fields
                        .iter()
                        .map(|f| format!("{}: {}", f.name, self.val_name(f.ty)))
                        .collect();
                    format!(" {{ {} }}", fields.join(", "))
                }
            };
            let disc = v
                .discriminant
                .map(|d| format!(" = {d}"))
                .unwrap_or_default();
            let _ = writeln!(out, "    {}{payload}{disc},", v.name);
        }
        out.push_str("}\n\n");
        if e.drop {
            let _ = writeln!(
                out,
                "impl Drop for {} {{\n    fn drop(&mut self) {{\n        match self {{",
                e.name
            );
            for v in &e.variants {
                let _ = writeln!(out, "            {}", e.print_arm(v, "x", "drop "));
            }
            out.push_str("        }\n    }\n}\n\n");
        }
    }

    pub fn render_sig(&self, f: &FnSig) -> String {
        let params: Vec<String> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| match f.owner.and_then(|o| o.receiver) {
                Some(Receiver::Value) if i == 0 => {
                    format!("{}self", if p.mutable { "mut " } else { "" })
                }
                Some(Receiver::Ref { mutable }) if i == 0 => {
                    format!("&{}self", if mutable { "mut " } else { "" })
                }
                _ => format!(
                    "{}{}: {}",
                    if p.mutable { "mut " } else { "" },
                    p.name,
                    self.ty_name(p.ty)
                ),
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

/// The segments of a sub-place path: `.A`, `.0`, `[1]` or `[?]` (an index expression in
/// brackets may hold anything with balanced brackets).
pub fn path_segments(path: &str) -> Vec<&str> {
    let bytes = path.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i] == b'[' {
            let mut level = 0;
            loop {
                match bytes[i] {
                    b'[' => level += 1,
                    b']' => level -= 1,
                    _ => {}
                }
                i += 1;
                if level == 0 {
                    break;
                }
            }
        } else {
            i += 1;
            while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                i += 1;
            }
        }
        out.push(&path[start..i]);
    }
    out
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

/// A field or payload type for a new struct or enum: a primitive, an earlier struct, enum
/// or array, or a tuple of primitives (`program.tuples` holds those so far); only `Copy`
/// types when `copy`.
fn member_ty(rng: &mut Rng, program: &Program, copy: bool) -> ValTy {
    let structs: Vec<usize> = (0..program.structs.len())
        .filter(|&i| !copy || program.structs[i].copy)
        .collect();
    let enums: Vec<usize> = (0..program.enums.len())
        .filter(|&i| !copy || program.enums[i].copy)
        .collect();
    let arrays: Vec<usize> = (0..program.arrays.len())
        .filter(|&i| {
            program.arrays[i].len.is_some() && (!copy || program.is_copy(Ty::Val(ValTy::Array(i))))
        })
        .collect();
    if !arrays.is_empty() && rng.chance(18) {
        ValTy::Array(*rng.pick(&arrays))
    } else if !structs.is_empty() && rng.chance(30) {
        ValTy::Struct(*rng.pick(&structs))
    } else if !enums.is_empty() && rng.chance(20) {
        ValTy::Enum(*rng.pick(&enums))
    } else if !program.tuples.is_empty() && rng.chance(15) {
        // Tuples of primitives so far: always `Copy`.
        ValTy::Tuple(rng.index(program.tuples.len()))
    } else {
        ValTy::Prim(random_prim(rng))
    }
}

/// Generates a struct: its fields are primitives, earlier structs and enums or tuples of
/// primitives, and a `Copy` struct only contains `Copy` fields. Half of the other structs
/// implement `Drop` (those with a primitive leaf to print).
fn gen_struct(rng: &mut Rng, program: &mut Program, used: &mut Vec<String>) {
    let name = unique_name(rng, STRUCT_NAMES, used);
    let copy = rng.chance(50);
    let n_fields = rng.range(1, 4);
    let mut field_names = Vec::new();
    let mut fields = Vec::new();
    for _ in 0..n_fields {
        let fname = unique_name(rng, FIELD_NAMES, &mut field_names);
        let ty = member_ty(rng, program, copy);
        fields.push(Field { name: fname, ty });
    }
    program.structs.push(StructDef {
        name,
        copy,
        fields,
        drop: None,
    });
    if !copy && rng.chance(50) {
        let index = program.structs.len() - 1;
        let leaves = program.leaves(ValTy::Struct(index));
        if !leaves.is_empty() {
            let (path, _) = rng.pick(&leaves).clone();
            program.structs[index].drop = Some(path);
        }
    }
}

/// Generates an enum (#8): one to four variants, unit, tuple or struct; a single-variant
/// enum always has a payload (it is destructured by irrefutable patterns). A quarter of
/// the others are fieldless, half of those with explicit discriminants on some variants.
/// `Copy` enums hold only `Copy` payloads; some of the others implement `Drop`.
fn gen_enum(rng: &mut Rng, program: &mut Program, used: &mut Vec<String>) {
    let name = unique_name(rng, ENUM_NAMES, used);
    let copy = rng.chance(40);
    let n_variants = match rng.weighted(&[12, 33, 35, 20]) {
        0 => 1,
        1 => 2,
        2 => 3,
        _ => 4,
    };
    let fieldless = n_variants > 1 && rng.chance(25);
    let mut variant_names = Vec::new();
    let mut variants = Vec::new();
    for _ in 0..n_variants {
        let vname = unique_name(rng, VARIANT_NAMES, &mut variant_names);
        let kind = if fieldless {
            0
        } else if n_variants == 1 {
            1 + rng.index(2)
        } else {
            rng.weighted(&[30, 45, 25])
        };
        let kind = match kind {
            0 => VariantKind::Unit,
            1 => {
                let n = rng.range(1, 3);
                VariantKind::Tuple((0..n).map(|_| member_ty(rng, program, copy)).collect())
            }
            _ => {
                let n = rng.range(1, 3);
                let mut field_names = Vec::new();
                VariantKind::Struct(
                    (0..n)
                        .map(|_| Field {
                            name: unique_name(rng, FIELD_NAMES, &mut field_names),
                            ty: member_ty(rng, program, copy),
                        })
                        .collect(),
                )
            }
        };
        variants.push(Variant {
            name: vname,
            kind,
            discriminant: None,
        });
    }
    if fieldless && rng.chance(50) {
        // Explicit values on some variants; the others follow the previous one. A value is
        // chosen so that neither it nor its successor (the next implicit value) is taken.
        let mut next: i128 = 0;
        let mut taken: Vec<i128> = Vec::new();
        for v in &mut variants {
            let mut value = next;
            if rng.chance(50) || taken.contains(&value) {
                loop {
                    value = i128::from(rng.range(-20, 300));
                    if !taken.contains(&value) && !taken.contains(&(value + 1)) {
                        break;
                    }
                }
                v.discriminant = Some(value);
            }
            taken.push(value);
            next = value + 1;
        }
    }
    // Never on an enum whose variants all came out as unit variants: it may be cast with
    // `as`, which rustc rejects for an enum that implements `Drop`.
    let all_unit = variants.iter().all(|v| matches!(v.kind, VariantKind::Unit));
    let drop = !copy && !all_unit && rng.chance(40);
    program.enums.push(EnumDef {
        name,
        copy,
        variants,
        drop,
    });
}

/// Generates the structs and enums, interleaved, each referring only to earlier ones; now
/// and then an array of the new one follows, which later ones may hold.
fn gen_aggregates(rng: &mut Rng, knobs: &Knobs, program: &mut Program) {
    let mut structs = rng.index(knobs.max_structs + 1);
    let mut enums = rng.index(knobs.max_enums + 1);
    let mut used = Vec::new();
    while structs + enums > 0 {
        let ty = if enums == 0 || (structs > 0 && rng.chance(50)) {
            structs -= 1;
            gen_struct(rng, program, &mut used);
            ValTy::Struct(program.structs.len() - 1)
        } else {
            enums -= 1;
            gen_enum(rng, program, &mut used);
            ValTy::Enum(program.enums.len() - 1)
        };
        // Arrays of values with an `impl Drop` show the order elements are dropped in.
        let chance = if program.has_drop_impl(ty) { 55 } else { 30 };
        if sized_arrays(program) < knobs.max_arrays && rng.chance(chance) {
            gen_array(rng, program, ty);
        }
    }
}

fn sized_arrays(program: &Program) -> usize {
    program.arrays.iter().filter(|a| a.len.is_some()).count()
}

/// A length for a new array type (small, now and then 0 or 1) and its spelling: a literal,
/// a `usize` constant of that value, or one plus a literal.
fn array_len(rng: &mut Rng, program: &Program) -> (usize, String) {
    let len = rng.weighted(&[9, 18, 30, 26, 17]);
    let consts: Vec<(&str, usize)> = program
        .consts
        .iter()
        .filter_map(|c| match (c.ty, c.value) {
            (Prim::Int(IntTy::Usize), ConstValue::Int(v)) if v <= 8 => {
                Some((c.name.as_str(), usize::try_from(v).expect("small")))
            }
            _ => None,
        })
        .collect();
    let equal: Vec<&str> = consts
        .iter()
        .filter(|(_, v)| *v == len)
        .map(|(n, _)| *n)
        .collect();
    let smaller: Vec<(&str, usize)> = consts.iter().copied().filter(|(_, v)| *v < len).collect();
    let text = if !equal.is_empty() && rng.chance(50) {
        (*rng.pick(&equal)).to_owned()
    } else if !smaller.is_empty() && rng.chance(20) {
        let (name, v) = *rng.pick(&smaller);
        format!("{name} + {}", len - v)
    } else if rng.chance(15) {
        format!("{len}usize")
    } else {
        len.to_string()
    };
    (len, text)
}

/// Adds an array type of `elem` with a random length (unless the program has one of that
/// element type and length already, or its values would print too many leaves), and now
/// and then the slice type `[elem]`.
fn gen_array(rng: &mut Rng, program: &mut Program, elem: ValTy) {
    let (len, len_text) = array_len(rng, program);
    let leaves = program.leaves(elem).len().max(1);
    let nested =
        matches!(program.array_of(elem), Some((inner, _)) if program.array_of(inner).is_some());
    if leaves * len > 12 || nested || !program.is_sized(elem) {
        return;
    }
    if !program
        .arrays
        .iter()
        .any(|a| a.elem == elem && a.len == Some(len))
    {
        program.arrays.push(ArrayDef {
            elem,
            len: Some(len),
            len_text,
        });
    }
    if program.slice_of(elem).is_none() && rng.chance(85) {
        program.arrays.push(ArrayDef {
            elem,
            len: None,
            len_text: String::new(),
        });
    }
}

/// Generates up to `count` array types (and slice types of some of their element types)
/// over everything declared so far: primitives (most often), tuples, structs, enums and
/// arrays (two levels deep at most); only primitives and tuples unless `aggregates`.
fn gen_arrays(rng: &mut Rng, count: usize, aggregates: bool, program: &mut Program) {
    for _ in 0..count {
        let sized: Vec<usize> = (0..program.arrays.len())
            .filter(|&a| program.arrays[a].len.is_some())
            .collect();
        let elem = match rng.weighted(&[
            55,
            if program.tuples.is_empty() { 0 } else { 12 },
            if aggregates && !program.structs.is_empty() {
                18
            } else {
                0
            },
            if aggregates && !program.enums.is_empty() {
                12
            } else {
                0
            },
            if aggregates && !sized.is_empty() {
                22
            } else {
                0
            },
        ]) {
            0 => ValTy::Prim(random_prim(rng)),
            1 => ValTy::Tuple(rng.index(program.tuples.len())),
            2 => ValTy::Struct(rng.index(program.structs.len())),
            3 => ValTy::Enum(rng.index(program.enums.len())),
            _ => ValTy::Array(*rng.pick(&sized)),
        };
        gen_array(rng, program, elem);
    }
}

/// Generates up to `count` tuple types. With `aggregates`, elements may also be structs,
/// enums, arrays and earlier tuples (nesting); otherwise they are primitives. Now and then
/// a tuple is wider than seven elements, past `System.ValueTuple`'s direct fields.
fn gen_tuples(rng: &mut Rng, count: usize, aggregates: bool, program: &mut Program) {
    for _ in 0..count {
        let n = if rng.chance(8) {
            rng.range(8, 10)
        } else {
            rng.range(1, 4)
        };
        let mut elems = Vec::new();
        let arrays: Vec<usize> = (0..program.arrays.len())
            .filter(|&a| program.arrays[a].len.is_some())
            .collect();
        for _ in 0..n {
            if aggregates && !arrays.is_empty() && rng.chance(12) {
                elems.push(ValTy::Array(*rng.pick(&arrays)));
                continue;
            }
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
                if aggregates && !program.enums.is_empty() {
                    12
                } else {
                    0
                },
            ]) {
                0 => ValTy::Prim(random_prim(rng)),
                1 => ValTy::Struct(rng.index(program.structs.len())),
                2 => ValTy::Tuple(rng.index(program.tuples.len())),
                _ => ValTy::Enum(rng.index(program.enums.len())),
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
    let mut used = Vec::new();
    let mut consts: Vec<ConstDef> = Vec::new();
    // Small `usize` constants first, for array lengths (#9): a literal, or an earlier one
    // plus or times a literal.
    for _ in 0..rng.index(3) {
        let name = unique_name(rng, LEN_NAMES, &mut used);
        let ity = IntTy::Usize;
        let earlier: Vec<(String, i128)> = consts
            .iter()
            .filter_map(|c| match c.value {
                ConstValue::Int(v) if c.ty == Prim::Int(ity) => Some((c.name.clone(), v)),
                _ => None,
            })
            .collect();
        let mut value = i128::from(rng.range(0, 4));
        let mut init = int_literal(ity, value);
        if !earlier.is_empty() && rng.chance(50) {
            let (base, v) = rng.pick(&earlier).clone();
            if rng.chance(70) {
                let d = i128::from(rng.range(1, 3));
                value = v + d;
                init = format!("{base} + {}", int_literal(ity, d));
            } else {
                let f = i128::from(rng.range(0, 2));
                value = v * f;
                init = format!("{base} * {}", int_literal(ity, f));
            }
        }
        consts.push(ConstDef {
            name,
            ty: Prim::Int(ity),
            value: ConstValue::Int(value),
            init,
        });
    }
    let count = rng.index(knobs.max_consts + 1);
    for _ in 0..count {
        let name = unique_name(rng, CONST_NAMES, &mut used);
        let ty = random_prim(rng);
        let (mut init, mut value) = random_literal(rng, ty);
        if let Prim::Int(ity) = ty {
            // Sometimes an arithmetic expression over literals and earlier constants.
            let same_ty: Vec<&ConstDef> = consts
                .iter()
                .filter(|c| c.ty == ty && matches!(c.value, ConstValue::Int(_)))
                .collect();
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
    let has_enums = !program.enums.is_empty();
    let sized: Vec<usize> = (0..program.arrays.len())
        .filter(|&a| program.arrays[a].len.is_some())
        .collect();
    let choice = rng.weighted(&[
        55,
        if has_structs { 15 } else { 0 },
        if has_tuples { 12 } else { 0 },
        if allow_refs { 15 } else { 0 },
        if allow_refs { 15 } else { 0 },
        if has_enums { 15 } else { 0 },
        if sized.is_empty() { 0 } else { 10 },
    ]);
    let slices: Vec<usize> = (0..program.arrays.len())
        .filter(|&a| program.arrays[a].len.is_none())
        .collect();
    let inner = |rng: &mut Rng| {
        if rng.chance(20) {
            // `&str` (#10); never `&mut str` (see below).
            ValTy::Str
        } else if !slices.is_empty() && rng.chance(22) {
            // A slice, unsized behind the reference.
            ValTy::Array(*rng.pick(&slices))
        } else if !sized.is_empty() && rng.chance(15) {
            ValTy::Array(*rng.pick(&sized))
        } else if has_tuples && rng.chance(20) {
            ValTy::Tuple(rng.index(program.tuples.len()))
        } else if has_enums && rng.chance(35) {
            ValTy::Enum(rng.index(program.enums.len()))
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
        5 => Ty::Val(ValTy::Enum(rng.index(program.enums.len()))),
        6 => Ty::Val(ValTy::Array(*rng.pick(&sized))),
        3 => Ty::Ref {
            mutable: false,
            inner: inner(rng),
        },
        _ => {
            let inner = inner(rng);
            Ty::Ref {
                mutable: inner != ValTy::Str,
                inner,
            }
        }
    }
}

pub const LOCAL_NAMES: &[&str] = &[
    "a", "b", "c", "d", "n", "k", "m", "total", "sum", "acc", "delta", "flag", "tmp", "val",
    "item", "left", "right", "cur", "prev", "next",
];

/// Makes the function an associated function of a random struct or enum: a method taking
/// `self`, `mut self`, `&self` or `&mut self` (its first parameter, named `self`), or now
/// and then an associated function without one.
fn gen_owner(rng: &mut Rng, program: &Program, params: &mut Vec<Param>) -> Owner {
    let n_structs = program.structs.len();
    let i = rng.index(n_structs + program.enums.len());
    let owner_ty = if i < n_structs {
        ValTy::Struct(i)
    } else {
        ValTy::Enum(i - n_structs)
    };
    let receiver = match rng.weighted(&[35, 30, 20, 15]) {
        0 => Some(Receiver::Ref { mutable: false }),
        1 => Some(Receiver::Ref { mutable: true }),
        2 => Some(Receiver::Value),
        _ => None,
    };
    let ty = match receiver {
        Some(Receiver::Ref { mutable }) => Some(Ty::Ref {
            mutable,
            inner: owner_ty,
        }),
        Some(Receiver::Value) => Some(Ty::Val(owner_ty)),
        None => None,
    };
    if let Some(ty) = ty {
        params.push(Param {
            name: "self".to_owned(),
            mutable: receiver == Some(Receiver::Value) && rng.chance(40),
            ty,
        });
    }
    Owner {
        ty: owner_ty,
        receiver,
    }
}

fn gen_sig(rng: &mut Rng, program: &Program, name: String, is_main: bool, method: bool) -> FnSig {
    if is_main {
        return FnSig {
            name,
            params: Vec::new(),
            ret: RetTy::Unit,
            owner: None,
        };
    }
    let mut params = Vec::new();
    let owner = method.then(|| gen_owner(rng, program, &mut params));
    let n_params = rng.range(0, 3);
    let mut used = Vec::new();
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
    // Elision: a `&self`/`&mut self` method returns a reference into `self` whatever its
    // other parameters; otherwise the one reference parameter must be the only one.
    let ref_self = matches!(
        owner,
        Some(Owner {
            receiver: Some(Receiver::Ref { .. }),
            ..
        })
    );
    let constructor = owner.is_some_and(|o| o.receiver.is_none());
    let sized: Vec<usize> = (0..program.arrays.len())
        .filter(|&a| program.arrays[a].len.is_some())
        .collect();
    let ret = match rng.weighted(&[
        25,
        50,
        if program.structs.is_empty() { 0 } else { 12 },
        if ref_params.len() == 1 || ref_self {
            25
        } else {
            0
        },
        if program.tuples.is_empty() { 0 } else { 12 },
        if constructor { 60 } else { 0 },
        if program.enums.is_empty() { 0 } else { 14 },
        if sized.is_empty() { 0 } else { 10 },
    ]) {
        0 => RetTy::Unit,
        1 => RetTy::Val(ValTy::Prim(random_prim(rng))),
        2 => RetTy::Val(ValTy::Struct(rng.index(program.structs.len()))),
        4 => RetTy::Val(ValTy::Tuple(rng.index(program.tuples.len()))),
        5 => RetTy::Val(owner.expect("a constructor").ty),
        6 => RetTy::Val(ValTy::Enum(rng.index(program.enums.len()))),
        7 => RetTy::Val(ValTy::Array(*rng.pick(&sized))),
        _ => {
            let Ty::Ref { mutable, inner } = ref_params[0].ty else {
                unreachable!()
            };
            let targets = program.sub_places(inner);
            let (_, mut target) = *rng.pick(&targets);
            // A `&str` (#10): a slice of a `&str` parameter, or a literal (`'static`)
            // whatever the parameter.
            if rng.chance(25) {
                target = ValTy::Str;
            }
            // An array returned as a slice of its elements (`&[T; N]` to `&[T]`).
            if let Some((elem, Some(_))) = program.array_of(target) {
                if let Some(slice) = program.slice_of(elem) {
                    if rng.chance(45) {
                        target = slice;
                    }
                }
            }
            RetTy::Ref {
                mutable: mutable && target != ValTy::Str && rng.chance(60),
                inner: target,
            }
        }
    };
    FnSig {
        name,
        params,
        ret,
        owner,
    }
}

/// Generates the whole skeleton: constants (array lengths may name them), tuples and arrays
/// of primitives, structs and enums (which may hold them), more tuples and arrays over
/// everything so far, then the signatures.
pub fn gen_program(rng: &mut Rng, knobs: &Knobs) -> Program {
    let mut program = Program {
        structs: Vec::new(),
        enums: Vec::new(),
        tuples: Vec::new(),
        arrays: Vec::new(),
        consts: Vec::new(),
        fns: Vec::new(),
    };
    program.consts = gen_consts(rng, knobs);
    let count = rng.index(knobs.max_tuples / 2 + 1);
    gen_tuples(rng, count, false, &mut program);
    let count = rng.index(knobs.max_arrays / 2 + 1);
    gen_arrays(rng, count, false, &mut program);
    gen_aggregates(rng, knobs, &mut program);
    let count = rng.index(knobs.max_tuples.saturating_sub(program.tuples.len()) + 1);
    gen_tuples(rng, count, true, &mut program);
    let count = rng.index(knobs.max_arrays.saturating_sub(sized_arrays(&program)) + 1);
    gen_arrays(rng, count, true, &mut program);
    let n_fns = rng.range(
        i64::try_from(knobs.min_fns.max(1)).expect("fits"),
        i64::try_from(knobs.max_fns.max(knobs.min_fns).max(1)).expect("fits"),
    );
    let mut used = Vec::new();
    for i in 0..n_fns {
        let is_main = i == n_fns - 1;
        let has_owners = !program.structs.is_empty() || !program.enums.is_empty();
        let method = !is_main && has_owners && rng.chance(40);
        let name = if is_main {
            "Main".to_owned()
        } else if method {
            unique_name(rng, METHOD_NAMES, &mut used)
        } else {
            unique_name(rng, FN_NAMES, &mut used)
        };
        let sig = gen_sig(rng, &program, name, is_main, method);
        program.fns.push(sig);
    }
    program
}

#[cfg(test)]
mod tests {
    use super::{str_len, STR_ESCAPED, STR_LITERALS};

    /// The literal pool's values are distinct (patterns of one `match` pick distinct
    /// sources, which must not be equal values, or an arm would be unreachable) and the
    /// lengths of the escaped ones are counted right.
    #[test]
    fn string_literals_are_distinct_ascii_values() {
        let unescape = |src: &str| -> String {
            src.replace("\\x41", "A")
                .replace("\\x62", "b")
                .replace("\\u{61}", "a")
                .replace("\\n", "\n")
                .replace("\\t", "\t")
                .replace("\\0", "\0")
                .replace("\\'", "'")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        };
        let mut values: Vec<String> = Vec::new();
        for src in STR_LITERALS.iter().chain(STR_ESCAPED) {
            let value = unescape(src);
            assert!(value.is_ascii(), "{src}");
            assert!(
                !src.contains(['{', '}', ';']) || src.contains("\\u{"),
                "{src}"
            );
            assert_eq!(str_len(src), value.len(), "{src}");
            assert!(!values.contains(&value), "{src} repeats a value");
            values.push(value);
        }
    }
}
