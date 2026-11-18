//! Trait selection stays cheap in long bodies (decision 105). Selection runs before and
//! after every call's arguments, as rustc's does, and an obligation left ambiguous until
//! the end of the body (`i64: From<{integer}>`, waiting for the literal's fallback) was
//! once probed against every `From` block at each of those points: 1000 lines of
//! `s = s + P(1);` took 56 s to type-check (debug), against 0.3 s before selection ran
//! there. An obligation is now tried again only when a variable of it changes (rustc's
//! `stalled_on`). The bound is generous, for slow runners; the quadratic version is two
//! orders of magnitude over it.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use sabiko_driver::{compile_str, Options, Stage};

/// Type-checks a `Main` of `n` copies of `line` after `items`, which must compile, within
/// the bound.
fn check(items: &str, line: &str, n: usize) {
    let mut src = String::from(items);
    src.push_str("fn Main() {\n    let mut s: i64 = 0;\n");
    for _ in 0..n {
        writeln!(src, "    {line}").unwrap();
    }
    src.push_str("    PrintLine(s);\n}\n");
    let options = Options {
        stop_after: Stage::Typeck,
        ..Options::default()
    };
    let start = Instant::now();
    let c = compile_str("scaling.sabi", &src, options);
    let elapsed = start.elapsed();
    assert!(
        c.diagnostics.is_empty(),
        "{line}: {:?}",
        c.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>()
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "{n} lines of `{line}` took {elapsed:?} to type-check"
    );
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
