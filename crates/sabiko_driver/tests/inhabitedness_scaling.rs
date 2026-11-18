//! Exhaustiveness checking decides inhabitedness at each generic instance (decision
//! 106), and a valid program can make those instances exponentially many: `E0<T>` with
//! variants holding `E1<(T, T)>` and `E1<(T, i32)>`, and so on. Each check decides a
//! bounded number of instances, of bounded size, and takes the rest as inhabited
//! (E0328, as before decision 106), so such a program compiles in well under a second,
//! while a deep nesting of one generic struct around an empty enum is still empty.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use sabiko_diagnostics::ErrorCode;
use sabiko_driver::{compile_str, Compilation, Options};

/// How long one program may take, generously above what it needs in a debug build.
const LIMIT: Duration = Duration::from_secs(5);

fn compile_timed(name: &str, src: &str) -> Compilation {
    let start = Instant::now();
    let c = compile_str(name, src, Options::default());
    let elapsed = start.elapsed();
    assert!(elapsed < LIMIT, "{name} took {elapsed:?}:\n{src}");
    let codes: Vec<ErrorCode> = c.diagnostics.iter().map(|d| d.code).collect();
    assert!(
        !codes.contains(&ErrorCode::INTERNAL_ERROR),
        "{name}: {codes:?}"
    );
    c
}

/// `n` levels of enums, each variant of level `k` holding level `k + 1` at a new
/// argument, with `Void` at the leaves of every instance; only `None` is matched.
fn branching(n: usize) -> String {
    let mut src = String::from("enum Void {}\n");
    for k in 0..n {
        let _ = writeln!(
            src,
            "enum E{k}<T> {{ A(E{next}<(T, T)>), B(E{next}<(T, i32)>) }}",
            next = k + 1
        );
    }
    let _ = writeln!(src, "enum E{n}<T> {{ A(T) }}");
    src.push_str("fn F(o: Option<E0<Void>>) -> i32 { match o { None => 1 } }\n");
    src.push_str("fn Main() { PrintLine(F(None)); }\n");
    src
}

/// `Option` of `n` `W`s around `Void`; only `None` is matched.
fn nested(n: usize) -> String {
    format!(
        "enum Void {{}}\nstruct W<T> {{ X: T }}\nfn F(o: Option<{}Void{}>) -> i32 {{ match o {{ None => 1 }} }}\nfn Main() {{ PrintLine(F(None)); }}\n",
        "W<".repeat(n),
        ">".repeat(n)
    )
}

#[test]
fn exponentially_many_instances_are_bounded() {
    // Deeper ones are slow before exhaustiveness checking (typeck of the enums alone).
    for n in [8, 12, 16] {
        compile_timed(&format!("branching{n}.sabi"), &branching(n));
    }
}

#[test]
fn deep_nesting_is_decided_in_full() {
    for n in [9, 64, 127] {
        let c = compile_timed(&format!("nested{n}.sabi"), &nested(n));
        let codes: Vec<ErrorCode> = c.diagnostics.iter().map(|d| d.code).collect();
        assert!(codes.is_empty(), "nested{n}: {codes:?}");
    }
}
