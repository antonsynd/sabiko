//! Every generated program must be valid Sabiko up to borrow checking, borrow-clean
//! programs must pass the whole pipeline, and generation must be deterministic.

use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};
use sabiko_testgen::{generate, insert_noops, Knobs};

const SEEDS: u64 = 500;

/// Parses, resolves and type-checks `src`; returns the rendered errors.
fn front_end_errors(name: &str, src: &str) -> String {
    let mut sm = SourceMap::new();
    let file = sm.add(name, src);
    let mut sink = DiagnosticSink::new();
    let ast = sabiko_parser::parse_file(file, src, &mut sink);
    if !sink.has_errors() {
        let resolve = sabiko_resolve::resolve(&ast, &mut sink);
        if !sink.has_errors() {
            let _ = sabiko_types::typeck(&ast, &resolve, &mut sink);
        }
    }
    if sink.has_errors() {
        sabiko_diagnostics::render(&sink.into_vec(), &sm, Format::HumanPlain)
    } else {
        String::new()
    }
}

fn check_front_end(knobs: &Knobs, label: &str) {
    let mut failures = Vec::new();
    for seed in 1..=SEEDS {
        let src = generate(seed, knobs);
        let errors = front_end_errors(&format!("{label}-{seed}.sabi"), &src);
        if !errors.is_empty() {
            failures.push(format!("seed {seed}:\n{errors}\n{src}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {SEEDS} {label} programs fail the front end:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn default_programs_pass_the_front_end() {
    check_front_end(&Knobs::default(), "default");
}

#[test]
fn borrow_clean_programs_pass_the_front_end() {
    check_front_end(&Knobs::borrow_clean(), "clean");
}

#[test]
fn borrow_clean_programs_pass_the_whole_pipeline() {
    let knobs = Knobs::borrow_clean();
    let mut failures = Vec::new();
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        let options = sabiko_driver::Options {
            emit: sabiko_driver::Emit::Il,
            output_kind: sabiko_driver::OutputKind::Executable,
            ..sabiko_driver::Options::default()
        };
        let c = sabiko_driver::compile_str(&format!("clean-{seed}.sabi"), &src, options);
        if c.has_errors() || c.output.is_empty() {
            failures.push(format!(
                "seed {seed}:\n{}\n{src}",
                c.render_diagnostics(Format::HumanPlain)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {SEEDS} borrow-clean programs are rejected:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn noop_insertion_keeps_programs_valid() {
    let knobs = Knobs::default();
    for seed in 1..=200 {
        let src = insert_noops(&generate(seed, &knobs), seed);
        let errors = front_end_errors(&format!("noop-{seed}.sabi"), &src);
        assert!(errors.is_empty(), "seed {seed}:\n{errors}\n{src}");
    }
}

#[test]
fn generation_is_deterministic() {
    for seed in [0, 1, 2, 42, 1000, u64::MAX] {
        let a = generate(seed, &Knobs::default());
        let b = generate(seed, &Knobs::default());
        assert_eq!(a, b, "seed {seed}");
        assert_eq!(insert_noops(&a, seed), insert_noops(&b, seed));
    }
    assert_ne!(
        generate(1, &Knobs::default()),
        generate(2, &Knobs::default())
    );
}

#[test]
fn programs_have_the_expected_shape() {
    let knobs = Knobs::default();
    let mut total_lines = 0;
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        assert!(src.contains("fn Main() {"), "seed {seed}");
        // Output is observable: something is printed (some programs only use `Print`), or
        // the program panics (some do nothing else).
        assert!(
            src.contains("PrintLine(") || src.contains("Print(") || src.contains("Panic("),
            "seed {seed}:\n{src}"
        );
        let lines = src.lines().count();
        total_lines += lines;
        // The line budget covers the bodies; an `impl Drop` adds seven item lines.
        assert!(
            lines <= knobs.max_lines + 30 + 7 * knobs.max_structs,
            "seed {seed}: {lines} lines\n{src}"
        );
    }
    let average = total_lines / usize::try_from(SEEDS).unwrap();
    assert!(
        average >= 15,
        "programs are too small: {average} lines on average"
    );
}

/// `i128`/`u128` are part of the type universe: a fair share of programs use each, and
/// some use their extreme literals (the generator's `u128` values stop at `i128::MAX`).
#[test]
fn programs_use_128_bit_integers() {
    let knobs = Knobs::default();
    let (mut signed, mut unsigned, mut min_literal) = (0, 0, 0);
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        signed += usize::from(src.contains("i128"));
        unsigned += usize::from(src.contains("u128"));
        min_literal += usize::from(src.contains("-170141183460469231731687303715884105728i128"));
    }
    assert!(
        signed >= 100 && unsigned >= 100,
        "i128 in {signed}, u128 in {unsigned} of {SEEDS} programs"
    );
    assert!(min_literal > 0, "no program uses the `i128::MIN` literal");
}

/// Methods are part of the subset: a fair share of programs declare `impl` blocks and call
/// methods, some of them in the shapes two-phase borrows allow, an argument reading the
/// receiver (`v.Add(v.X)`).
#[test]
fn programs_use_methods() {
    let knobs = Knobs::default();
    let (mut impls, mut calls, mut reads_receiver) = (0, 0, 0);
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        impls += usize::from(
            src.lines()
                .any(|l| l.starts_with("impl ") && !l.starts_with("impl Drop for ")),
        );
        let mut called = false;
        let mut read = false;
        for line in src.lines() {
            // `name.Method(...)` with `name` a plain local: the receiver, then the rest.
            for (at, _) in line.match_indices('.') {
                let recv: String = line[..at]
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let tail = &line[at + 1..];
                let method: String = tail
                    .chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect();
                let is_call = !recv.is_empty()
                    && recv.starts_with(|c: char| c.is_ascii_lowercase())
                    && method.starts_with(|c: char| c.is_ascii_uppercase())
                    && tail[method.len()..].starts_with('(');
                if is_call {
                    called = true;
                    read |= tail.contains(&format!("{recv}."));
                }
            }
        }
        calls += usize::from(called);
        reads_receiver += usize::from(read);
    }
    assert!(
        impls >= 150 && calls >= 100 && reads_receiver >= 20,
        "impl blocks in {impls}, method calls in {calls}, arguments reading the receiver in {reads_receiver} of {SEEDS} programs"
    );
}

/// Drops (#12) are part of the subset: a fair share of programs declare structs with
/// `impl Drop` and keep values of them in locals, so drop order, moves, reassignments,
/// early exits and unwinding are all observable in the output.
#[test]
fn programs_use_drop() {
    let knobs = Knobs::default();
    let mut with_drop = 0;
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        with_drop += usize::from(src.contains("\nimpl Drop for "));
    }
    assert!(
        with_drop >= 100,
        "`impl Drop` in {with_drop} of {SEEDS} programs"
    );
}
