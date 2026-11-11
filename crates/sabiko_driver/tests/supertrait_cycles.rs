//! Generated supertrait graphs with overlapping cycles (decision 95): every one compiles
//! in well under a second, with E0354 exactly when the graph has a cycle. Overlapping
//! cycles once made elaboration run without end (`trait B<T>: A<T> + C<(T,)>` with `A`
//! and `C` both leading back to `B`): a bound on any trait of such a graph must still
//! elaborate.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use sabiko_diagnostics::ErrorCode;
use sabiko_driver::{compile_str, Options};

/// How many traits a generated graph has.
const TRAITS: usize = 5;

/// A deterministic pseudo-random sequence (a linear congruential generator).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from((self.0 >> 33) % below as u64).expect("small")
    }
}

/// The supertraits of each trait of graph `seed`: up to three, each at the trait's own
/// argument or at a tuple of it (so a cycle through both grows the arguments), mostly
/// later traits, one in six any trait (a back edge or a self-loop, making cycles that
/// often overlap).
fn graph(seed: u64) -> Vec<Vec<(usize, bool)>> {
    let mut rng = Lcg(seed);
    (0..TRAITS)
        .map(|i| {
            (0..rng.next(4))
                .map(|_| {
                    let target = if i + 1 == TRAITS || rng.next(6) == 0 {
                        rng.next(TRAITS)
                    } else {
                        i + 1 + rng.next(TRAITS - i - 1)
                    };
                    (target, rng.next(2) == 1)
                })
                .collect()
        })
        .collect()
}

/// Whether some trait of `g` leads back to itself.
fn has_cycle(g: &[Vec<(usize, bool)>]) -> bool {
    (0..g.len()).any(|start| {
        let mut seen = vec![false; g.len()];
        let mut work: Vec<usize> = g[start].iter().map(|&(s, _)| s).collect();
        while let Some(t) = work.pop() {
            if t == start {
                return true;
            }
            if !std::mem::replace(&mut seen[t], true) {
                work.extend(g[t].iter().map(|&(s, _)| s));
            }
        }
        false
    })
}

/// The program: the traits (`T0` has a provided method), for each trait a function with
/// a bound on it that calls another with the same bound (proved by elaborating the bound
/// in scope), and an empty `Main`.
fn program(g: &[Vec<(usize, bool)>]) -> String {
    let mut src = String::new();
    for (i, sups) in g.iter().enumerate() {
        let list: Vec<String> = sups
            .iter()
            .map(|&(s, tuple)| {
                if tuple {
                    format!("T{s}<(T,)>")
                } else {
                    format!("T{s}<T>")
                }
            })
            .collect();
        if list.is_empty() {
            let _ = write!(src, "trait T{i}<T> {{");
        } else {
            let _ = write!(src, "trait T{i}<T>: {} {{", list.join(" + "));
        }
        if i == 0 {
            src.push_str("\n    fn M(&self) -> i32 {\n        0\n    }\n");
        }
        src.push_str("}\n\n");
    }
    for i in 0..g.len() {
        let _ = write!(
            src,
            "fn H{i}<Y: T{i}<i32>>(y: Y) {{}}\n\nfn F{i}<X: T{i}<i32>>(x: X) {{\n    H{i}(x);\n}}\n\n"
        );
    }
    src.push_str("fn Main() {}\n");
    src
}

#[test]
fn overlapping_supertrait_cycles_are_reported_and_end() {
    let mut cyclic = 0;
    for seed in 0..300 {
        let g = graph(seed);
        let src = program(&g);
        let start = Instant::now();
        let c = compile_str("cycles.sabi", &src, Options::default());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "seed {seed} took {elapsed:?}:\n{src}"
        );
        let codes: Vec<ErrorCode> = c.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&ErrorCode::INTERNAL_ERROR),
            "seed {seed}: {codes:?}\n{src}"
        );
        let reported = codes.contains(&ErrorCode::SUPERTRAIT_CYCLE);
        assert_eq!(reported, has_cycle(&g), "seed {seed}: {codes:?}\n{src}");
        cyclic += usize::from(reported);
    }
    // The generator draws both kinds.
    assert!(cyclic > 50 && cyclic < 250, "{cyclic} cyclic graphs of 300");
}
