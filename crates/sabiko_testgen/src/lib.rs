//! Deterministic random programs in the subset shared by Sabiko and Rust, for the
//! differential and determinism tests (`crates/sabikoc/tests/generated.rs`).
//!
//! [`generate`] turns a seed and a set of [`Knobs`] into one program as source text. Every
//! program is valid in both languages up to borrow checking: names resolve, types check,
//! constants evaluate, literals fit their types, and the results are printed with
//! `Print`/`PrintLine` so that run-time behaviour is observable. Run-time panics (integer
//! overflow, division by zero, over-wide shifts, `Panic`) are possible and intended: both
//! compilers exit with 101 and the harness compares the messages.
//!
//! The subset: `fn` with parameters and return types (including a reference into the
//! single reference parameter), `struct` with primitive, struct or tuple fields, optionally
//! `#[derive(Copy, Clone)]`, tuples of primitives, structs and tuples (1 to 3 elements,
//! occasionally 8 or 9) as locals, parameters, return values and fields, `const` items,
//! `let`/`let mut` and destructuring `let (a, (b, _), mut c)` (also of a reference to a
//! tuple, binding references), `if`/`else` as statement and expression, bounded `while`
//! and `loop` (with `break`, `break value`, `continue`), `return`, struct and tuple
//! literals, nested field and element reads and writes (`p.A.0`, `t.1.X`), `&`/`&mut`
//! borrows passed to functions and held in locals, `*r`, writes through `&mut`, all
//! arithmetic, bitwise, shift, comparison and logical operators (comparisons also between
//! references to primitives, which compare the referents), unary `-`/`!`, `as` casts
//! between every integer type (`i128`/`u128` included), `f32`/`f64`, `bool` and ASCII
//! `char`, suffixed literals, compound assignment and destructuring assignment `(p, q) =
//! (e, f)` of two places (#50; a place the rules forbid by its turn is `_`). Methods (#7): functions of inherent
//! `impl` blocks (one block per function, so a struct's methods are spread over several)
//! taking `self`, `mut self`, `&self` or `&mut self`, or nothing (associated functions,
//! often constructors); called as `Type::Name(..)` or with method syntax on places,
//! reference locals, call results and temporaries, including chains through a returned
//! reference and the two-phase shapes (#15) where later arguments read the receiver
//! (`v.Add(v.Get())`) or a reborrowed `&mut` argument (`Use(m, m.X)`); `&self` methods
//! return references into `self` whatever their other parameters. Enums (#8): unit, tuple
//! and struct variants with payloads of primitives, tuples, structs and earlier enums
//! (and structs, tuples and other enums holding enums), some `#[derive(Copy, Clone)]`,
//! some with an `impl Drop` that matches on `self`, fieldless ones with explicit
//! discriminants cast with `as`, `impl` blocks of enums; `match` as a statement, as a
//! `let` initialiser and inline in expressions, `if let` (statement and expression),
//! `while let` over a state local, let chains of either (`if c && let p = s && d`),
//! `if let` match guards (`p if let 3 = x && c =>`),
//! `let`-`else` diverging with `return`, `break`,
//! `continue` or a panic, and irrefutable destructuring `let`s of structs and
//! single-variant enums. Patterns: variants (nested), literals and ranges (`a..=b`, `a..b`)
//! of integers, `bool` and `char`, or-patterns, `_`, `..`, field shorthand, bindings by
//! value, `mut`, `ref`, `ref mut` and `x @ p`, guards reading bindings and other locals,
//! on places, references (default binding modes) and temporaries; every `match` is
//! exhaustive by construction (see [`body`]'s `matches` module). Arrays and slices (#9):
//! `[T; N]` of primitives, tuples, structs (`Copy` or not, with `impl Drop` or not),
//! enums and arrays, with lengths 0 to 4 written as literals or `usize` constants (`LEN`,
//! `LEN + 1`); `&[T]`/`&mut [T]` parameters, returns and annotated `let`s, coerced from
//! references to arrays; array literals, `[]` where the type is known and `[x; N]` (only
//! `Copy` operands for N >= 2); element reads, writes, compound assignments and borrows
//! with constant indices and computed ones that may be out of bounds or overflow;
//! `.len()`; `while i < s.len()` loops; arrays as fields, tuple elements, enum payloads,
//! parameters and returns, copied or moved whole (never out of an element). Strings and
//! range indexing (#10): `&str` locals, parameters and returns (`fn F(s: &str) -> &str`,
//! by elision, or a literal from any single reference parameter), ASCII string literals
//! with escapes, range places of strings, arrays and slices in all six forms (`&s[1..k]`,
//! `&mut a[i..]`, `a[..2][k] = v`, `s[1..][..2]`, `x[a..=b].len()`) with constant and
//! computed bounds that are sometimes out of range or overflow, `.len()` of strings, the
//! six comparisons of `&str` operands, `match` (literals, or-patterns, guarded bindings, a
//! catch-all), `if let` and `let`-`else` on strings, and loops walking a string or range
//! place by index or shrinking a `mut` `&str` (see [`body`]'s `strings` and `arrays`
//! modules). References to references (#11): `&&T`, `&&mut T`, `&mut &T` and `&mut &mut
//! T` locals borrowing reference locals and parameters (`let rr = &r;`, `let m = &mut
//! r;`), `&&str` and `&&[T]` among them; reads through them (`**rr`, auto-deref field
//! access, indexing, `.len()` and method calls, `(*rr)` as a `&T`), writes through `&mut
//! &mut T` (`**m = v`), retargets through `&mut &T` and `&mut &mut T` (`*m = &y;`, `*m =
//! "lit";`) followed by reads of the retargeted local, `match` through both references,
//! comparisons of `&&str` and `&&p` operands, and `&&T`, `&&mut T`, `&mut &mut T` and
//! `&&str` parameters passed `&r`, `&mut r`, a double reference local or `&&x` (see
//! [`body`]'s `refs` module). Elements of temporary arrays of references, `&str` or
//! shared (`["a", s][i]`, `[&x, r][i]`), now and then through a range (`[a, b, c][1..][k]`,
//! a slice of references; #43, see [`body`]'s `arrays` module). Generic functions (#114):
//! values wrapped in the helpers `GenId(e)`/`GenId::<T>(e)` and `GenPick(c, a, b)`, shared
//! references in `GenFirst(r)` ([`program::GENERIC_HELPERS`]). Not generated: slice
//! patterns, ranges as values, array comparison, arrays of references as parameters or
//! as locals outliving a block of their own (`stmt_ref_aggregate` writes `[&T; N]`,
//! `&[&T]`, `(&T, U)` and `[&mut T; 2]` locals of primitive referents used in one block;
//! TODO(#92)), generic types and bounds (#115, #116), closures, returned double references and
//! three levels of references, `&mut str`, comparisons of `str` places, non-ASCII text,
//! references (`&str` and `&&T` included, #92) in struct fields, tuples, arrays or enum
//! payloads, `()` values, comparisons of tuples or enums, `&` patterns, half-open range
//! patterns, float patterns, recursion, shadowing, unbounded loops, and reads of
//! uninitialised locals.
//!
//! With [`Knobs::default`] a small fraction of the scope-model checks are skipped on
//! purpose, so some programs contain a use after move, a conflicting borrow, a write to an
//! immutable binding or a move out of a reference; both compilers must then reject them
//! with borrow-checker errors. [`Knobs::borrow_clean`] never skips a check, for the
//! run-time comparison.
//!
//! Two transformations derive equivalent programs from a generated one: [`insert_noops`]
//! adds no-op statements, and [`reorder::reorder`] swaps adjacent independent statements
//! (an analysis on Sabiko's syntax tree and local types; see the module).

pub mod body;
pub mod program;
pub mod reorder;
pub mod rng;
pub mod ty;

use std::fmt::Write as _;

use rng::Rng;

/// Size and shape of a generated program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knobs {
    /// Number of functions, `Main` included.
    pub min_fns: usize,
    pub max_fns: usize,
    pub max_structs: usize,
    pub max_enums: usize,
    /// Number of distinct tuple types.
    pub max_tuples: usize,
    /// Number of distinct array types (`[T; N]`); slice types come on top.
    pub max_arrays: usize,
    pub max_consts: usize,
    /// A soft limit on the total line count: bodies stop generating statements once it
    /// is reached, so a program exceeds it by at most a few closing lines.
    pub max_lines: usize,
    /// Nesting depth of expressions.
    pub max_expr_depth: u32,
    /// Nesting depth of blocks (`if`, `while`, `loop`) inside a body.
    pub max_block_depth: usize,
    /// The chance, in percent, that a borrow-model check is skipped so that the program
    /// contains a borrow error. `0` makes every program borrow-clean.
    pub violation_percent: u8,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            min_fns: 2,
            max_fns: 6,
            max_structs: 3,
            max_enums: 3,
            max_tuples: 3,
            max_arrays: 4,
            max_consts: 3,
            max_lines: 60,
            max_expr_depth: 3,
            max_block_depth: 3,
            violation_percent: 8,
        }
    }
}

impl Knobs {
    /// The default shape, but never a borrow error: every program is accepted by both
    /// borrow checkers (and so can be run).
    pub fn borrow_clean() -> Self {
        Knobs {
            violation_percent: 0,
            ..Self::default()
        }
    }

    pub fn is_borrow_clean(&self) -> bool {
        self.violation_percent == 0
    }
}

/// Generates one program. The same `seed` and `knobs` always give the same text.
pub fn generate(seed: u64, knobs: &Knobs) -> String {
    let mut rng = Rng::new(seed);
    let prog = program::gen_program(&mut rng, knobs);
    let mut out = format!(
        "// Generated by sabiko_testgen, seed {seed}{}.\n",
        if knobs.is_borrow_clean() {
            " (borrow-clean)"
        } else {
            ""
        }
    );
    out.push_str(&prog.render_items());
    let item_lines = out.lines().count();
    let budget = knobs
        .max_lines
        .saturating_sub(item_lines)
        .checked_div(prog.fns.len())
        .unwrap_or(0)
        .max(5);
    for i in 0..prog.fns.len() {
        // A function of an `impl` block gets a block of its own, so a struct's methods are
        // spread over several blocks between the free functions.
        let indent = match prog.fns[i].owner {
            Some(owner) => {
                let _ = writeln!(out, "impl {} {{", prog.val_name(owner.ty));
                "    "
            }
            None => "",
        };
        out.push_str(indent);
        out.push_str(&prog.render_sig(&prog.fns[i]));
        out.push('\n');
        let body = body::BodyGen::new(&prog, knobs, &mut rng, i, budget).generate();
        for l in body {
            out.push_str(indent);
            out.push_str(&l);
            out.push('\n');
        }
        out.push_str(indent);
        out.push_str("}\n");
        if !indent.is_empty() {
            out.push_str("}\n");
        }
        if i + 1 < prog.fns.len() {
            out.push('\n');
        }
    }
    out
}

/// The no-op statements [`insert_noops`] chooses from.
pub const NOOPS: &[&str] = &["let _ = ();", "{}", "();"];

/// Line indices after which a statement can be inserted. Generated programs put every
/// statement on its own line, indented by at least four spaces: a line ending in `;` ends
/// a statement, one ending in `{` opens a block (`if`, `else`, `while`, `loop`, a match
/// arm's block, `let .. else`), and both are followed by a statement position. Items, struct
/// fields and enum variants are never indented that way (they end with `,`), struct
/// literals are written on one line, and a `match` header (`match x {`, `let v = match x
/// {`) is followed by an arm, so it is not a boundary; an arm ends with `,` or `}`.
pub fn statement_boundaries(program: &str) -> Vec<usize> {
    program
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let match_header = l.ends_with('{')
                && (l.trim_start().starts_with("match ") || l.contains(" = match "));
            l.starts_with("    ") && (l.ends_with(';') || l.ends_with('{')) && !match_header
        })
        .map(|(i, _)| i)
        .collect()
}

/// Inserts no-op statements (`let _ = ();`, `{}`, `();`) at a random subset of the
/// statement boundaries of a generated program, at least one when there is a boundary.
/// The result must behave exactly like `program` in both compilers.
pub fn insert_noops(program: &str, seed: u64) -> String {
    let mut rng = Rng::new(seed ^ 0x5EED_0FA5_11AB);
    let boundaries = statement_boundaries(program);
    let forced = (!boundaries.is_empty()).then(|| boundaries[rng.index(boundaries.len())]);
    let mut out = String::new();
    for (i, line) in program.lines().enumerate() {
        out.push_str(line);
        out.push('\n');
        if boundaries.contains(&i) && (forced == Some(i) || rng.chance(35)) {
            let indent = line.len() - line.trim_start().len();
            let indent = if line.ends_with('{') {
                indent + 4
            } else {
                indent
            };
            out.push_str(&" ".repeat(indent));
            out.push_str(rng.pick(NOOPS));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_skip_items_and_fields() {
        let src = "struct S {\n    A: i32,\n}\n\nconst X: i32 = 1i32;\n\nfn Main() {\n    let a = 1i32;\n    if true {\n        PrintLine(a);\n    }\n}\n";
        assert_eq!(statement_boundaries(src), vec![7, 8, 9]);
        let with = insert_noops(src, 1);
        assert!(with.lines().count() > src.lines().count());
    }

    #[test]
    fn boundaries_skip_variants_and_match_headers() {
        // An arm follows a `match` header, so no statement goes after it; an arm's block
        // and a `let`-`else` block open statement positions.
        let src = "enum E {\n    A(i32),\n    B,\n}\n\nfn Main() {\n    let e = E::B;\n    match e {\n        E::A(n) => {\n            PrintLine(n);\n        }\n        E::B => PrintLine(0i32),\n    }\n    let v = match e {\n        _ => 1i32,\n    };\n    let E::A(m) = e else {\n        return;\n    };\n}\n";
        assert_eq!(statement_boundaries(src), vec![6, 8, 9, 15, 16, 17, 18]);
        for seed in 0..20 {
            let with = insert_noops(src, seed);
            assert!(
                !with.contains("match e {\n        let _")
                    && !with.contains("match e {\n        {}")
                    && !with.contains("match e {\n        ();"),
                "{with}"
            );
        }
    }
}
