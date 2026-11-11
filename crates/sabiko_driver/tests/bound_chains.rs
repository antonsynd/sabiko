//! Chains of bounds through blanket `impl` blocks (decision 95), generated at depths on
//! both sides of every limit: the borrow checker must relate the regions of a call
//! through every block of a chain the type checker proved (`bound_region_links` follows
//! proofs exactly as deep as the solver), and a chain as deep as rustc's recursion limit
//! (128) is an overflow, one error, as rustc's E0275.
//!
//! The chain: `impl<T> L0<T> for B<T>`, then `impl<T, U: L{i-1}<T>> Li<T> for U` up to
//! `N`, and `fn G<X: LN<T>, T>(x: X, t: T) -> X`, whose bound is proved at `X = B<&i32>`,
//! `T = &i32` through all `N + 1` blocks, the last of which names `T` twice: the result
//! holds the loan of `&y`. rustc 1.99 gives E0506 for `N` up to 127 and E0275 from 128;
//! with supertraits instead (`trait Li<T>: L{i-1}<T>`), a method of `L0` called inside `G`
//! stores `t` in `x`, and a result outliving `y` is E0597.
//!
//! The nesting: `impl<T: Show> Show for W<T>` proved at `W<W<..i32..>>` `n` deep, where
//! the innermost type is an inference variable while the body is checked (an unsuffixed
//! literal, a type a later statement decides, a struct literal passed to a method), so
//! inference confirms the blocks one level at a time and the rest of the proof is finished
//! once the body is done: the recursion limit counts from the bound either way. rustc 1.99
//! accepts `n` up to 127 and gives E0275 from 128; with `bool` innermost, E0277 up to 127
//! (#155) and E0275 from 128.

use std::fmt::Write as _;

use sabiko_diagnostics::ErrorCode;
use sabiko_driver::{compile_str, Options};

/// The bound chain of depth `n`; `mutate` writes `y` while the result is live.
fn bound_chain(n: usize, mutate: bool) -> String {
    let mut src = String::from(
        "struct B<T> {\n    T: T,\n}\n\ntrait L0<T> {\n    fn Touch(&self) -> i32 {\n        0\n    }\n}\n\nimpl<T> L0<T> for B<T> {}\n",
    );
    for i in 1..=n {
        let _ = write!(
            src,
            "\ntrait L{i}<T> {{}}\n\nimpl<T, U: L{}<T>> L{i}<T> for U {{}}\n",
            i - 1
        );
    }
    let _ = write!(
        src,
        "\nfn G<X: L{n}<T>, T>(x: X, t: T) -> X {{\n    x\n}}\n\nfn Main() {{\n    let x = 1;\n    let mut y = 2;\n    let w = G(B {{ T: &x }}, &y);\n"
    );
    if mutate {
        src.push_str("    y = 3;\n");
    }
    src.push_str("    PrintLine(w.Touch() + y);\n}\n");
    src
}

/// The supertrait chain of depth `n`: `G` stores `t` in `x` through `L0`'s method, found
/// through `n` supertraits; `dangle` lets the result outlive the borrowed local.
fn supertrait_chain(n: usize, dangle: bool) -> String {
    let mut src = String::from(
        "struct B<T> {\n    T: T,\n}\n\ntrait L0<T> {\n    fn Set(&mut self, t: T);\n}\n\nimpl<T> L0<T> for B<T> {\n    fn Set(&mut self, t: T) {\n        self.T = t;\n    }\n}\n",
    );
    for i in 1..=n {
        let p = i - 1;
        let _ = write!(
            src,
            "\ntrait L{i}<T>: L{p}<T> {{}}\n\nimpl<T, U: L{p}<T>> L{i}<T> for U {{}}\n"
        );
    }
    let _ = write!(
        src,
        "\nfn G<X: L{n}<T>, T>(mut x: X, t: T) -> X {{\n    x.Set(t);\n    x\n}}\n\nfn Main() {{\n    let a = 1;\n    let mut b = B {{ T: &a }};\n"
    );
    if dangle {
        src.push_str("    {\n        let y = 2;\n        b = G(b, &y);\n    }\n");
    } else {
        src.push_str("    let y = 2;\n    b = G(b, &y);\n");
    }
    src.push_str("    PrintLine(*b.T);\n}\n");
    src
}

/// How the innermost type of a nesting is given.
#[derive(Clone, Copy, Debug)]
enum Inner {
    /// `G(W { F: .. 5 .. })`.
    Literal,
    /// `G(W { F: .. z .. })`, `z`'s type decided by a later statement.
    Later,
    /// `H.Take(W { F: .. 5 .. })`, a generic method.
    Method,
    /// `G(W { F: .. true .. })`: `bool: Show` does not hold.
    Unsatisfied,
    /// `fn Deep(w: W<..bool..>) -> i32 { G(w) }`: written types, `bool: Show` does not
    /// hold.
    UnsatisfiedParam,
}

/// The nesting of `n` `W`s around the innermost type `inner`.
fn nesting(n: usize, inner: Inner) -> String {
    let mut src = String::from(
        "trait Show {\n    fn Get(&self) -> i32;\n}\n\nimpl Show for i32 {\n    fn Get(&self) -> i32 {\n        *self\n    }\n}\n\nstruct W<T> {\n    F: T,\n}\n\nimpl<T: Show> Show for W<T> {\n    fn Get(&self) -> i32 {\n        self.F.Get() + 1\n    }\n}\n\nstruct H;\n\nimpl H {\n    fn Take<X: Show>(&self, x: X) -> i32 {\n        x.Get()\n    }\n}\n\nfn G<X: Show>(x: X) -> i32 {\n    x.Get()\n}\n\nfn Default<T: From<i32>>() -> T {\n    T::from(5)\n}\n",
    );
    let wrap = |e: &str| {
        let mut e = e.to_owned();
        for _ in 0..n {
            e = format!("W {{ F: {e} }}");
        }
        e
    };
    let body = match inner {
        Inner::Literal => format!("    PrintLine(G({}));\n", wrap("5")),
        Inner::Later => format!(
            "    let z = Default();\n    PrintLine(G({}));\n    let k: i32 = z;\n    PrintLine(k);\n",
            wrap("z")
        ),
        Inner::Method => format!("    PrintLine(H.Take({}));\n", wrap("5")),
        Inner::Unsatisfied => format!("    PrintLine(G({}));\n", wrap("true")),
        Inner::UnsatisfiedParam => {
            let mut t = String::from("bool");
            for _ in 0..n {
                t = format!("W<{t}>");
            }
            let _ = write!(src, "\nfn Deep(w: {t}) -> i32 {{\n    G(w)\n}}\n");
            String::new()
        }
    };
    let _ = write!(src, "\nfn Main() {{\n{body}}}\n");
    src
}

fn codes(src: &str) -> Vec<ErrorCode> {
    let c = compile_str("chain.sabi", src, Options::default());
    let mut codes: Vec<ErrorCode> = c.diagnostics.iter().map(|d| d.code).collect();
    codes.dedup();
    codes
}

#[test]
fn bound_chains_keep_loans_up_to_the_recursion_limit() {
    for n in [1, 32, 33, 64, 65, 127] {
        assert_eq!(
            codes(&bound_chain(n, true)),
            [ErrorCode::ASSIGN_TO_BORROWED],
            "depth {n}: the loan of `y` must reach the result"
        );
        assert_eq!(codes(&bound_chain(n, false)), [], "depth {n}");
    }
}

/// One overflow per item whose bound reaches the limit, as rustc reports them: at depth
/// 128 `G`'s bound, at 130 also the bounds of the blocks for `L129` and `L130`; and the
/// call in `Main`, whose proof is as deep. (rustc 1.99 reports no overflow in a body once
/// one in a signature was reported, so it shows only the items' errors; with `G`'s bound
/// at 127 and the call at 128 it reports the call's.)
#[test]
fn a_chain_as_deep_as_the_limit_is_an_overflow_per_item() {
    for (n, errors) in [(128, 2), (130, 4)] {
        let c = compile_str("chain.sabi", &bound_chain(n, true), Options::default());
        let codes: Vec<ErrorCode> = c.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(
            codes,
            vec![ErrorCode::OVERFLOW_EVALUATING; errors],
            "depth {n}"
        );
    }
}

#[test]
fn supertrait_chains_keep_loans() {
    for n in [1, 33, 40, 70] {
        assert_eq!(
            codes(&supertrait_chain(n, true)),
            [ErrorCode::DOES_NOT_LIVE_LONG_ENOUGH],
            "depth {n}"
        );
        assert_eq!(codes(&supertrait_chain(n, false)), [], "depth {n}");
    }
}

/// The recursion limit counts from the bound a use requires, however much of the proof
/// inference confirmed while checking the body, and the borrow checker follows the same
/// proof: never an internal error.
#[test]
fn nestings_inferred_late_overflow_at_the_recursion_limit() {
    for inner in [Inner::Literal, Inner::Later, Inner::Method] {
        for n in [1, 127] {
            assert_eq!(codes(&nesting(n, inner)), [], "{inner:?} at depth {n}");
        }
        for n in [128, 129, 160, 200] {
            assert_eq!(
                codes(&nesting(n, inner)),
                [ErrorCode::OVERFLOW_EVALUATING],
                "{inner:?} at depth {n}"
            );
        }
    }
}

/// An unsatisfied bound at the bottom of a nesting is an error at every depth (#155): the
/// bound itself below the limit, an overflow from it on.
#[test]
fn unsatisfied_nestings_are_errors_at_every_depth() {
    for inner in [Inner::Unsatisfied, Inner::UnsatisfiedParam] {
        for n in [1, 65, 127] {
            assert_eq!(
                codes(&nesting(n, inner)),
                [ErrorCode::UNSATISFIED_BOUND],
                "{inner:?} at depth {n}"
            );
        }
        for n in [128, 129, 200] {
            assert_eq!(
                codes(&nesting(n, inner)),
                [ErrorCode::OVERFLOW_EVALUATING],
                "{inner:?} at depth {n}"
            );
        }
    }
}
