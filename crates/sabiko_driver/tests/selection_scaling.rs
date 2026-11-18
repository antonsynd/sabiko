//! Trait selection stays cheap in long bodies, and terminates (decision 105). Selection
//! runs before and after every call's arguments, as rustc's does, and an obligation left
//! ambiguous until the end of the body (`i64: From<{integer}>`, waiting for the literal's
//! fallback) was once probed against every `From` block at each of those points: 1000
//! lines of `s = s + P(1);` took 56 s to type-check (debug), against 0.3 s before
//! selection ran there. An obligation is now tried again only when a variable of it
//! changes (rustc's `stalled_on`). A `Clone` obligation on a generic struct with a single
//! `impl` block then looped forever, its block's fresh parameter waking it at every
//! attempt. The bound is generous, for slow runners; the quadratic version is two orders
//! of magnitude over it, and a type check that does not end fails at the bound instead
//! of hanging the test run.

use std::fmt::Write as _;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use sabiko_driver::{compile_str, Options, Stage};

const BOUND: Duration = Duration::from_secs(20);

/// Type-checks `src`, which must compile, on a thread of its own: fails when it reports
/// a diagnostic or takes longer than [`BOUND`] (a loop is left running, detached). A
/// panic in the type checker is passed on as it is.
fn type_check_within_bound(what: &str, src: String) {
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let options = Options {
            stop_after: Stage::Typeck,
            ..Options::default()
        };
        let start = Instant::now();
        let c = compile_str("scaling.sabi", &src, options);
        let codes: Vec<String> = c
            .diagnostics
            .iter()
            .map(|d| format!("{:?}", d.code))
            .collect();
        let _ = tx.send((codes, start.elapsed()));
    });
    let (codes, elapsed) = match rx.recv_timeout(BOUND) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => panic!("{what} did not type-check within {BOUND:?}"),
        // The worker ended without sending: it panicked.
        Err(RecvTimeoutError::Disconnected) => match worker.join() {
            Err(payload) => std::panic::resume_unwind(payload),
            Ok(()) => panic!("{what}: the type checker ended without a result"),
        },
    };
    assert!(codes.is_empty(), "{what}: {codes:?}");
    assert!(elapsed < BOUND, "{what} took {elapsed:?} to type-check");
}

/// Type-checks a `Main` of `n` copies of `line` after `items` within the bound.
fn check(items: &str, line: &str, n: usize) {
    let mut src = String::from(items);
    src.push_str("fn Main() {\n    let mut s: i64 = 0;\n");
    for _ in 0..n {
        writeln!(src, "    {line}").unwrap();
    }
    src.push_str("    PrintLine(s);\n}\n");
    type_check_within_bound(&format!("{n} lines of `{line}`"), src);
}

#[test]
fn literal_arguments_through_a_blanket_block() {
    check(
        "fn P<T: Into<i64>>(t: T) -> i64 { t.into() }\n",
        "s = s + P(1);",
        2000,
    );
}

#[test]
fn literal_conversions_through_a_path() {
    check(
        "fn Id<T>(x: T) -> T { x }\n",
        "let a = 1; let b: i64 = Id(i64::from(a)); s = s + b;",
        1000,
    );
}

/// `Clone` and `Copy` on a struct with one `impl` block, its argument a literal's or an
/// unknown variable: selection unifies the block's header without confirming it.
#[test]
fn clone_and_copy_with_a_single_block_terminate() {
    let items = "struct W<T> { X: T }
impl<T: Clone> Clone for W<T> { fn clone(&self) -> W<T> { W { X: self.X.clone() } } }
struct C<T> { X: T }
impl<T: Copy> Clone for C<T> { fn clone(&self) -> C<T> { C { X: self.X } } }
impl<T: Copy> Copy for C<T> {}
struct P<A, B> { X: A, Y: B }
impl<A: Clone, B: Clone> Clone for P<A, B> {
    fn clone(&self) -> P<A, B> { P { X: self.X.clone(), Y: self.Y.clone() } }
}
fn Need<T: Clone>(t: T) -> T { t }
fn NeedC<T: Copy>(t: T) -> T { t }
fn Default<T>() -> T { loop {} }
";
    let bodies = [
        "let w = W { X: 5 }; let q = w.clone(); PrintLine(q.X);",
        "let w = W { X: 5 }; let q = Clone::clone(&w); PrintLine(q.X);",
        "let q = Need(W { X: 5 }); PrintLine(q.X);",
        "let c = NeedC(C { X: 5 }); PrintLine(c.X);",
        "let w = W { X: 2.5 }; let q = w.clone(); PrintLine(q.X);",
        "let q = Need(P { X: 5, Y: 2.0 }); PrintLine(q.X);",
        "let o = W { X: Default() }; let q = Need(o); let p: W<i32> = q; PrintLine(p.X);",
    ];
    for body in bodies {
        type_check_within_bound(body, format!("{items}fn Main() {{ {body} }}\n"));
    }
}
