//! Type checking for Sabiko.
//!
//! [`typeck`] takes the AST and the resolver's side tables and produces
//! [`TypeckResults`]: item signatures, a [`BodyTypes`] table per function and constant
//! (every expression's and local's type, field indices for `e.f`), and the evaluated
//! value of every constant. Bodies that reported errors are marked so later phases skip
//! them.

pub mod check;
pub mod collect;
pub mod consts;
pub mod infer;
pub mod ty;

use std::collections::HashMap;

pub use check::{BodyTypes, FieldAccess};
pub use collect::{AdtDef, FieldDef, FnSig, ItemTypes};
pub use consts::ConstValue;
use sabiko_ast::{ItemKind, SourceFile};
use sabiko_diagnostics::DiagnosticSink;
use sabiko_resolve::{DefId, ResolveResults};
pub use ty::{Mutability, Ty, TyVid};

#[derive(Clone, Debug, Default)]
pub struct TypeckResults {
    pub items: ItemTypes,
    bodies: HashMap<DefId, BodyTypes>,
    consts: HashMap<DefId, ConstValue>,
}

impl TypeckResults {
    /// The body table of a function or constant.
    #[must_use]
    pub fn body(&self, def: DefId) -> &BodyTypes {
        &self.bodies[&def]
    }

    /// The evaluated value of a constant, if evaluation succeeded.
    #[must_use]
    pub fn const_value(&self, def: DefId) -> Option<&ConstValue> {
        self.consts.get(&def)
    }
}

/// Type-checks every item in `file`.
#[must_use]
pub fn typeck(
    file: &SourceFile,
    resolve: &ResolveResults,
    sink: &mut DiagnosticSink,
) -> TypeckResults {
    let items = collect::collect(file, resolve, sink);
    let mut bodies = HashMap::new();
    for item in &file.items {
        let def = resolve.def_of_item(item.id);
        match &item.kind {
            ItemKind::Fn(f) => {
                let ret = items.fn_sig(def).output.clone();
                let cx = check::FnCtxt::new(resolve, &items, sink, ret);
                bodies.insert(def, cx.check_fn(f, def));
            }
            ItemKind::Const(c) => {
                let ty = items.const_ty(def).clone();
                let cx = check::FnCtxt::new(resolve, &items, sink, ty);
                bodies.insert(def, cx.check_const(&c.value));
            }
            ItemKind::Struct(_) => {}
        }
    }
    let const_bodies: HashMap<DefId, (&sabiko_ast::Expr, &BodyTypes)> = file
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Const(c) => {
                let def = resolve.def_of_item(item.id);
                Some((def, (&c.value, &bodies[&def])))
            }
            _ => None,
        })
        .collect();
    let consts = consts::eval_all(resolve, &const_bodies, sink);
    TypeckResults {
        items,
        bodies,
        consts,
    }
}

#[cfg(test)]
mod tests {
    use sabiko_resolve::DefKind;
    use sabiko_span::FileId;

    use super::*;

    fn run(src: &str) -> (TypeckResults, ResolveResults, Vec<String>) {
        let mut sink = DiagnosticSink::new();
        let ast = sabiko_parser::parse_file(FileId(0), src, &mut sink);
        assert!(!sink.has_errors(), "parse errors: {:?}", sink.diagnostics());
        let resolve = sabiko_resolve::resolve(&ast, &mut sink);
        let results = typeck(&ast, &resolve, &mut sink);
        let mut diags = sink.into_vec();
        sabiko_diagnostics::sort(&mut diags);
        let msgs = diags
            .into_iter()
            .map(|d| {
                let label = d
                    .labels
                    .iter()
                    .find(|l| l.primary)
                    .map_or(String::new(), |l| l.message.clone());
                format!("{} {} [{label}]", d.code, d.message)
            })
            .collect();
        (results, resolve, msgs)
    }

    fn local_types(
        results: &TypeckResults,
        resolve: &ResolveResults,
        fn_name: &str,
    ) -> Vec<String> {
        let def = resolve
            .defs_of_kind(DefKind::Fn)
            .find(|d| resolve.def(*d).name == fn_name)
            .unwrap();
        let body = results.body(def);
        let mut locals: Vec<_> = body.local_types.iter().collect();
        locals.sort_by_key(|(l, _)| **l);
        locals
            .into_iter()
            .map(|(l, t)| format!("{}: {}", resolve.local(*l).name, t.display(resolve)))
            .collect()
    }

    #[test]
    fn inference_flows_both_ways() {
        let src = r#"
struct Point { X: i32, Y: f64 }
fn Make(x: i32) -> Point { Point { X: x, Y: 1.5 } }
fn F(p: &mut Point) -> u8 {
    let a = 1;
    let b: u8 = a;
    let c = 2;
    let d = c + 3;
    let e = 1.0;
    let f = p.X;
    let g = -3;
    let h = if b > 1 { 10 } else { return 0 };
    let i = loop { break 7u16; };
    let j = &mut *p;
    let k: &Point = j;
    let l = ();
    let m = Make(f).Y;
    let n = 'c' as u32;
    let o = f as f32;
    let s = "hi";
    let t;
    t = 5;
    p.X = d;
    b
}"#;
        let (r, resolve, msgs) = run(src);
        assert!(msgs.is_empty(), "{msgs:#?}");
        assert_eq!(
            local_types(&r, &resolve, "F"),
            [
                "p: &mut Point",
                "a: u8",
                "b: u8",
                "c: i32",
                "d: i32",
                "e: f64",
                "f: i32",
                "g: i32",
                "h: i32",
                "i: u16",
                "j: &mut Point",
                "k: &Point",
                "l: ()",
                "m: f64",
                "n: u32",
                "o: f32",
                "s: &str",
                "t: i32",
            ]
        );
    }

    #[test]
    fn mismatches_and_operators() {
        let src = r"
struct P { X: i32 }
fn G(a: i32, b: bool) -> i32 { a }
fn F() -> i32 {
    let x: i32 = true;
    let y = 1 + true;
    let z = if true { 1 } else { false };
    let w = G(1);
    let v = G(1, 2);
    let u = -1u8;
    let t = !1.0;
    let s = *3;
    let r = P { X: 1, Z: 2 };
    let q = P { };
    let p = P { X: 1, X: 2 };
    let o = r.Nope;
    let n = x.X;
    let m = x();
    let l = P;
    let k = G;
    let j = 256u8;
    let i: i8 = -129;
    let h = true as f32;
    let g = r as i32;
    let f = 1 == true;
    let e = r == r;
    1 = 2;
    while true { break 1; }
    break;
    continue;
    return;
}";
        let (_, _, msgs) = run(src);
        assert_eq!(
            msgs,
            [
                "E0300 mismatched types [expected `i32`, found `bool`]",
                "E0300 mismatched types [expected `{integer}`, found `bool`]",
                "E0300 `if` and `else` have incompatible types [expected `{integer}`, found `bool`]",
                "E0304 this function takes 2 arguments but 1 argument was supplied [expected 2 argument(s)]",
                "E0300 mismatched types [expected `bool`, found `{integer}`]",
                "E0307 cannot apply unary operator `-` to type `u8` [cannot apply unary operator `-`]",
                "E0307 cannot apply unary operator `!` to type `{float}` [cannot apply unary operator `!`]",
                "E0309 type `{integer}` cannot be dereferenced [can't be dereferenced]",
                "E0301 struct `P` has no field named `Z` [`P` does not have this field]",
                "E0302 missing field `X` in initializer of `P` [missing `X`]",
                "E0303 field `X` specified more than once [used more than once]",
                "E0301 no field `Nope` on type `P` [unknown field]",
                "E0301 no field `X` on type `i32` [unknown field]",
                "E0305 expected function, found `i32` [call expression requires function]",
                "E0203 expected value, found struct `P` [not a value]",
                "E0317 function `G` cannot be used as a value yet [function items are only callable]",
                "E0314 literal out of range for `u8` [does not fit in `u8`]",
                "E0314 literal out of range for `i8` [does not fit in `i8`]",
                "E0308 casting `bool` as `f32` is invalid [invalid cast]",
                "E0308 non-primitive cast: `P` as `i32` [invalid cast]",
                "E0300 mismatched types [expected `{integer}`, found `bool`]",
                "E0306 binary operation `==` cannot be applied to type `P` [no implementation for `P == P`]",
                "E0310 invalid left-hand side of assignment [cannot assign to this expression]",
                "E0312 can only break with a value inside `loop` [can only break with a value inside `loop`]",
                "E0311 `break` outside of a loop [cannot `break` outside of a loop]",
                "E0311 `continue` outside of a loop [cannot `continue` outside of a loop]",
                "E0300 mismatched types [expected `i32`, found `()`]",
            ]
        );
    }

    #[test]
    fn blocks_and_divergence() {
        let (_, _, msgs) = run(r"
fn A() -> i32 { 1; }
fn B() -> i32 { if true { 1 } }
fn C() -> i32 { loop { } }
fn D() -> i32 { return 1; }
fn E() -> i32 { let x = loop { break; }; 1 }
fn F() -> i32 { let x: i32 = loop { if true { break 1; } else { break; } }; x }
fn G() { let x; }
fn H() -> bool { let x = if true { return false } else { 1 }; x == 2 }
");
        assert_eq!(
            msgs,
            [
                "E0300 mismatched types [expected `i32`, found `()`]",
                "E0300 mismatched types [expected `()`, found `{integer}`]",
                "E0300 mismatched types [expected `i32`, found `()`]",
                "E0313 type annotations needed [consider giving `x` an explicit type]",
            ]
        );
    }

    #[test]
    fn constants_evaluate() {
        let src = r"
const A: i32 = 10 * 2 + B;
const B: i32 = -(3 as i32);
const C: u8 = 255;
const D: bool = A > 10 && !false;
const E: f64 = 1.5 * 2.0;
const F: i8 = 100 + 100;
const G: u32 = 1 / 0;
const H: i32 = I;
const I: i32 = H;
const J: i64 = (C as i64) << 1;
const K: char = 65u8 as char;
const L: u8 = 300i32 as u8;
const M: i32 = { 1 };
";
        let (r, resolve, msgs) = run(src);
        assert_eq!(
            msgs,
            [
                "E0316 attempt to add with overflow [in this constant]",
                "E0316 attempt to divide by zero [in this constant]",
                "E0316 cycle detected when evaluating constant `H` [cyclic reference]",
                "E0315 blocks cannot be used in constants yet [in this constant]",
            ]
        );
        let value = |name: &str| {
            let def = resolve
                .defs_of_kind(DefKind::Const)
                .find(|d| resolve.def(*d).name == name)
                .unwrap();
            r.const_value(def).cloned()
        };
        assert_eq!(value("A"), Some(ConstValue::Int(17)));
        assert_eq!(value("B"), Some(ConstValue::Int(-3)));
        assert_eq!(value("C"), Some(ConstValue::Uint(255)));
        assert_eq!(value("D"), Some(ConstValue::Bool(true)));
        assert_eq!(value("E"), Some(ConstValue::Float(3.0)));
        assert_eq!(value("F"), None);
        assert_eq!(value("G"), None);
        assert_eq!(value("H"), None);
        assert_eq!(value("I"), None);
        assert_eq!(value("J"), Some(ConstValue::Int(510)));
        assert_eq!(value("K"), Some(ConstValue::Char('A')));
        assert_eq!(value("L"), Some(ConstValue::Uint(44)));
    }

    #[test]
    fn signatures() {
        let (_, _, msgs) = run(
            "struct A { b: B }\nstruct B { a: A }\nstruct S { s: str }\nfn F(x: str) {}\nfn G() -> F {}\n",
        );
        assert_eq!(
            msgs,
            [
                "E0206 recursive types `A` and `B` have infinite size [recursive type has infinite size]",
                "E0208 the size for values of type `str` cannot be known at compilation time [doesn't have a size known at compile-time]",
                "E0208 the size for values of type `str` cannot be known at compilation time [doesn't have a size known at compile-time]",
                "E0203 expected type, found function `F` [not a type]",
            ]
        );
    }
}
