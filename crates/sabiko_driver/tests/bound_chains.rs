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
/// 128 only `G`'s bound, at 130 also the bounds of the blocks for `L129` and `L130`.
#[test]
fn a_chain_as_deep_as_the_limit_is_an_overflow_per_item() {
    for (n, errors) in [(128, 1), (130, 3)] {
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
