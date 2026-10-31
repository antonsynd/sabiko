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

pub use check::{BodyTypes, FieldAccess, MethodCallee};
pub use collect::{is_printable, AdtDef, FieldDef, FnSig, ItemTypes};
pub use consts::ConstValue;
pub use sabiko_ast::SelfKind;
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
    for item in file.all_items() {
        if let ItemKind::Impl(_) = item.kind {
            continue;
        }
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
            ItemKind::Struct(_) | ItemKind::Impl(_) => {}
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
    use std::fmt::Write as _;

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
                "E0304 this function takes 2 arguments but 1 argument was supplied [expected 2 arguments]",
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
                "E0300 `if` may be missing an `else` clause [expected `i32`, found `()`]",
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

    /// Evaluates `const C<i>: <ty> = <expr>;` for each case and returns the values in a
    /// plain form (`-2`, `1.75`, `'a'`), `None` where evaluation failed, and the messages.
    fn eval_consts(cases: &[(&str, String)]) -> (Vec<Option<String>>, Vec<String>) {
        let mut src = String::new();
        for (i, (ty, expr)) in cases.iter().enumerate() {
            let _ = writeln!(src, "const C{i}: {ty} = {expr};");
        }
        let (r, resolve, msgs) = run(&src);
        let values = (0..cases.len())
            .map(|i| {
                let def = resolve
                    .defs_of_kind(DefKind::Const)
                    .find(|d| resolve.def(*d).name == format!("C{i}"))
                    .unwrap();
                r.const_value(def).map(|v| match v {
                    ConstValue::Int(v) => v.to_string(),
                    ConstValue::Uint(v) => v.to_string(),
                    ConstValue::Float(v) => format!("{v:?}"),
                    ConstValue::Bool(v) => v.to_string(),
                    ConstValue::Char(v) => format!("{v:?}"),
                    ConstValue::Str(v) => format!("{v:?}"),
                    ConstValue::Unit => "()".to_owned(),
                })
            })
            .collect();
        (values, msgs)
    }

    /// `2^127` is a valid `i128` literal only under a negation, which gives `i128::MIN`
    /// (it was evaluated to `i128::MIN + 1` once, by saturating the literal).
    #[test]
    fn negated_minimum_literals_evaluate_exactly() {
        let cases = [
            ("i128", "-170141183460469231731687303715884105728"),
            ("i128", "-(170141183460469231731687303715884105728)"),
            ("i128", "-170141183460469231731687303715884105728i128 + 1"),
            ("i64", "-9223372036854775808"),
        ];
        let input: Vec<_> = cases.iter().map(|(t, e)| (*t, (*e).to_owned())).collect();
        let (values, msgs) = eval_consts(&input);
        assert!(msgs.is_empty(), "{msgs:?}");
        assert_eq!(
            values,
            [
                Some("-170141183460469231731687303715884105728".to_owned()),
                Some("-170141183460469231731687303715884105728".to_owned()),
                Some("-170141183460469231731687303715884105727".to_owned()),
                Some("-9223372036854775808".to_owned()),
            ]
        );
    }

    /// Every operator, operand kind and cast the evaluator implements, at the edges that
    /// matter (sign bits, saturation, truncation, NaN). The expected values are what rustc
    /// computes for the same constants.
    #[test]
    fn constants_evaluate_like_rustc() {
        let cases: &[(&str, &str, &str)] = &[
            ("i32", "5 - 7", "-2"),
            ("u8", "200 + 55", "255"),
            ("u16", "2 + 3", "5"),
            ("u32", "6 * 7", "42"),
            ("u64", "7 / 2", "3"),
            ("u64", "7 % 3", "1"),
            ("i32", "-6 & 13", "8"),
            ("i32", "-6 | 3", "-5"),
            ("i32", "-6 ^ 3", "-7"),
            ("u8", "12 & 10", "8"),
            ("u8", "12 | 10", "14"),
            ("u8", "12 ^ 10", "6"),
            ("bool", "true & false", "false"),
            ("bool", "true && false", "false"),
            ("bool", "false | true", "true"),
            ("bool", "true | true", "true"),
            ("bool", "false || true", "true"),
            ("bool", "true ^ true", "false"),
            ("bool", "true ^ false", "true"),
            ("i8", "1 << 6", "64"),
            ("i8", "1 << 7", "-128"),
            ("i16", "3 << 15", "-32768"),
            ("i32", "1 << 31", "-2147483648"),
            (
                "i128",
                "1 << 127",
                "-170141183460469231731687303715884105728",
            ),
            ("i32", "1 << 3u8", "8"),
            ("i32", "-16 >> 2", "-4"),
            ("i32", "-16 >> 2u64", "-4"),
            ("u8", "3 << 7", "128"),
            ("u32", "256 >> 4", "16"),
            ("u32", "256 >> 4u8", "16"),
            ("i32", "!5", "-6"),
            ("u8", "!5", "250"),
            ("u128", "!0", "340282366920938463463374607431768211455"),
            ("f64", "-2.5", "-2.5"),
            ("f64", "-(1.0 + 0.5)", "-1.5"),
            ("f64", "1.5 + 0.25", "1.75"),
            ("f64", "1.5 - 0.25", "1.25"),
            ("f64", "1.5 * 0.25", "0.375"),
            ("f64", "1.5 / 0.25", "6.0"),
            ("f64", "7.5 % 2.0", "1.5"),
            ("f32", "0.5 + 0.25", "0.75"),
            ("f32", "16777217.0", "16777216.0"),
            ("f32", "16777216.0 + 1.0", "16777216.0"),
            ("f64", "(16777216.0f32 + 1.0) as f64", "16777216.0"),
            ("f32", "-16777217.0", "-16777216.0"),
            ("i8", "200u8 as i8", "-56"),
            ("i16", "-1i64 as i16", "-1"),
            ("i64", "-1i8 as i64", "-1"),
            ("u16", "-1i8 as u16", "65535"),
            ("f32", "-1i32 as f32", "-1.0"),
            ("f32", "4294967295u32 as f32", "4294967296.0"),
            ("f64", "-1i32 as f64", "-1.0"),
            ("f64", "3000000000u32 as f64", "3000000000.0"),
            (
                "f64",
                "340282366920938463463374607431768211455u128 as f64",
                "3.402823669209385e38",
            ),
            (
                "f32",
                "340282366920938463463374607431768211455u128 as f32",
                "inf",
            ),
            ("f32", "0.1 as f32", "0.10000000149011612"),
            ("f64", "0.1f32 as f64", "0.10000000149011612"),
            ("f64", "0.5 as f64", "0.5"),
            ("i8", "300.0 as i8", "127"),
            ("i8", "-300.0 as i8", "-128"),
            ("i8", "2.7 as i8", "2"),
            ("i8", "-2.7 as i8", "-2"),
            ("i32", "(0.0 / 0.0) as i32", "0"),
            ("u8", "300.0 as u8", "255"),
            ("u8", "2.7 as u8", "2"),
            ("u8", "-5.0 as u8", "0"),
            ("u8", "(0.0 / 0.0) as u8", "0"),
        ];
        let input: Vec<_> = cases
            .iter()
            .map(|(t, e, _)| (*t, (*e).to_owned()))
            .collect();
        let (values, msgs) = eval_consts(&input);
        assert!(msgs.is_empty(), "{msgs:#?}");
        for ((ty, expr, expected), got) in cases.iter().zip(values) {
            assert_eq!(got.as_deref(), Some(*expected), "const: {ty} = {expr}");
        }
    }

    /// `==`, `!=`, `<`, `<=`, `>`, `>=` (in that order) on less, equal and greater
    /// operands of every comparable kind; NaN compares unequal to everything. Expected
    /// results are rustc's, except `()` operands, which rustc does not allow in constants.
    #[test]
    fn constant_comparisons_like_rustc() {
        const OPS: [&str; 6] = ["==", "!=", "<", "<=", ">", ">="];
        let cases: &[(&str, &str, [bool; 6])] = &[
            ("-2", "1", [false, true, true, true, false, false]),
            ("1", "1", [true, false, false, true, false, true]),
            ("1", "-2", [false, true, false, false, true, true]),
            ("1u8", "2u8", [false, true, true, true, false, false]),
            ("2u8", "2u8", [true, false, false, true, false, true]),
            ("3u8", "2u8", [false, true, false, false, true, true]),
            ("false", "true", [false, true, true, true, false, false]),
            ("true", "true", [true, false, false, true, false, true]),
            ("true", "false", [false, true, false, false, true, true]),
            ("'a'", "'b'", [false, true, true, true, false, false]),
            ("'b'", "'b'", [true, false, false, true, false, true]),
            ("'c'", "'b'", [false, true, false, false, true, true]),
            ("0.5", "1.5", [false, true, true, true, false, false]),
            ("1.5", "1.5", [true, false, false, true, false, true]),
            ("2.5", "1.5", [false, true, false, false, true, true]),
            (
                "(0.0 / 0.0)",
                "1.0",
                [false, true, false, false, false, false],
            ),
            (
                "(0.0 / 0.0)",
                "(0.0 / 0.0)",
                [false, true, false, false, false, false],
            ),
            ("()", "()", [true, false, false, true, false, true]),
        ];
        let input: Vec<_> = cases
            .iter()
            .flat_map(|(a, b, _)| OPS.iter().map(move |op| ("bool", format!("{a} {op} {b}"))))
            .collect();
        let (values, msgs) = eval_consts(&input);
        assert!(msgs.is_empty(), "{msgs:#?}");
        let expected = cases
            .iter()
            .flat_map(|(_, _, r)| r.iter().map(ToString::to_string));
        for (((_, expr), got), expected) in input.iter().zip(values).zip(expected) {
            assert_eq!(
                got.as_deref(),
                Some(expected.as_str()),
                "const: bool = {expr}"
            );
        }
    }

    #[test]
    fn unsigned_overflow_is_reported() {
        let input: Vec<_> = [("u8", "255 + 1"), ("u8", "16 * 16"), ("u8", "254 + 1")]
            .iter()
            .map(|(t, e)| (*t, (*e).to_owned()))
            .collect();
        let (values, msgs) = eval_consts(&input);
        assert_eq!(
            msgs,
            [
                "E0316 attempt to add with overflow [in this constant]",
                "E0316 attempt to multiply with overflow [in this constant]",
            ]
        );
        assert_eq!(values, [None, None, Some("255".to_owned())]);
    }

    #[test]
    fn builtins() {
        let src = r#"
struct S { a: i32 }
fn Ok(s: &str, b: u8, f: f32, c: char) -> i32 {
    Print(1);
    PrintLine(2.5);
    PrintLine("text");
    Print(s);
    PrintLine(b);
    Print(f);
    PrintLine(c);
    PrintLine(true);
    if b > 0 { 1 } else { Panic("negative") }
}
fn Diverges() -> i32 { Panic("x"); }
fn Bad(s: S) {
    PrintLine(s);
    PrintLine(1, 2);
    PrintLine();
    Panic(1);
    Panic("a", "b");
    let x: i32 = PrintLine(1);
    let y;
    PrintLine(y);
    PrintLine(S { a: 1 });
}"#;
        let (r, resolve, msgs) = run(src);
        assert_eq!(
            msgs,
            [
                "E0300 `PrintLine` cannot print a value of type `S` [expected a printable value, found `S`]",
                "E0304 this function takes 1 argument but 2 arguments were supplied [expected 1 argument]",
                "E0304 this function takes 1 argument but 0 arguments were supplied [expected 1 argument]",
                "E0300 mismatched types [expected `&str`, found `{integer}`]",
                "E0304 this function takes 1 argument but 2 arguments were supplied [expected 1 argument]",
                "E0300 mismatched types [expected `i32`, found `()`]",
                "E0313 type annotations needed [cannot infer the type of this argument to `PrintLine`]",
                "E0300 `PrintLine` cannot print a value of type `S` [expected a printable value, found `S`]",
            ]
        );
        let panic = resolve
            .defs_of_kind(DefKind::Fn)
            .find(|d| resolve.builtin(*d) == Some(sabiko_resolve::Builtin::Panic))
            .unwrap();
        assert_eq!(r.items.fn_sig(panic).output, Ty::Never);
        assert_eq!(r.items.builtin(panic), Some(sabiko_resolve::Builtin::Panic));
        assert!(
            !r.body(resolve.defs_of_kind(DefKind::Fn).next().unwrap())
                .has_errors
        );
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

    /// Tuple patterns and tuple types inside bodies: default binding modes through `& &mut`,
    /// `()` patterns, a tuple pattern without an initialiser, `_` inside a tuple
    /// annotation, and a diverging element without an expectation.
    #[test]
    fn tuple_patterns_and_annotations() {
        let src = r"
fn F(m: &mut (i32, bool)) {
    let (a, b) = &&mut *m;
    let () = ();
    let (c, d);
    c = 1u8;
    d = true;
    let t: (_, i32) = (2u16, 3);
    let u = (return, 4i64);
}";
        let (r, resolve, msgs) = run(src);
        assert!(msgs.is_empty(), "{msgs:#?}");
        assert_eq!(
            local_types(&r, &resolve, "F"),
            [
                "m: &mut (i32, bool)",
                "a: &i32",
                "b: &bool",
                "c: u8",
                "d: bool",
                "t: (u16, i32)",
                "u: ((), i64)",
            ]
        );
    }

    /// Each error is reported once: a method call on a receiver whose type is already an
    /// error reports nothing more.
    #[test]
    fn tuple_pattern_and_receiver_errors() {
        let src = r"
struct S { X: i32 }
fn F(s: S) {
    let (a, b) = ();
    let (c, d) = 5;
    let x;
    Print((x, 1i32));
    s.Missing.Foo();
    let e = 1i32;
    let f = e[0];
}";
        let (_, _, msgs) = run(src);
        assert_eq!(
            msgs,
            [
                "E0300 mismatched types [expected `()`, found a tuple with 2 elements]",
                "E0300 mismatched types [expected `{integer}`, found a tuple with 2 elements]",
                "E0313 type annotations needed [cannot infer the type of this argument to `Print`]",
                "E0301 no field `Missing` on type `S` [unknown field]",
                "E0107 not yet supported: indexing [indexing are not yet supported]",
            ]
        );
    }
}
