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
        // The line budget covers the bodies; a struct's `impl Drop` adds seven item lines,
        // an enum at most eight and its `impl Drop` at most eleven, and a `match` started
        // near the end of the budget may run past it by a few arms.
        assert!(
            lines <= knobs.max_lines + 40 + 7 * knobs.max_structs + 19 * knobs.max_enums,
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

/// No generated pattern is unreachable (E0330): Sabiko does not borrow-check code that
/// patterns make unreachable and rustc does (a known gap, see `body::matches`), so a
/// violation or a move there would make the verdicts differ.
#[test]
fn programs_have_no_unreachable_patterns() {
    let mut found = Vec::new();
    for knobs in [Knobs::default(), Knobs::borrow_clean()] {
        for seed in 1..=SEEDS {
            let src = generate(seed, &knobs);
            let mut sink = DiagnosticSink::new();
            let file = SourceMap::new().add("u.sabi", &src);
            let ast = sabiko_parser::parse_file(file, &src, &mut sink);
            let resolve = sabiko_resolve::resolve(&ast, &mut sink);
            let _ = sabiko_types::typeck(&ast, &resolve, &mut sink);
            if sink
                .diagnostics()
                .iter()
                .any(|d| d.code.to_string() == "E0330")
            {
                found.push(seed);
            }
        }
    }
    assert!(found.is_empty(), "unreachable patterns in seeds {found:?}");
}

/// A named property of a program's text and the least number of programs that have it.
type Check = (&'static str, fn(&str) -> bool, usize);

/// Enums and pattern matching (#8) are part of the subset: a fair share of programs
/// declare enums (some `Copy`, some with `impl Drop`, some fieldless with explicit
/// discriminants) and use every form of matching, so a construct that silently stopped
/// being generated fails here.
#[test]
fn programs_use_enums_and_match() {
    let knobs = Knobs::default();
    let checks: [Check; 18] = [
        ("enum", |s| s.contains("\nenum "), 300),
        (
            "Copy enum",
            |s| s.contains("#[derive(Copy, Clone)]\nenum "),
            120,
        ),
        (
            "`impl Drop` for an enum",
            |s| {
                s.lines().any(|l| {
                    l.starts_with("impl Drop for ")
                        && s.contains(&format!("\nenum {} {{", &l[14..l.len() - 2]))
                })
            },
            50,
        ),
        (
            "explicit discriminant",
            |s| {
                s.lines()
                    .any(|l| l.starts_with("    ") && l.ends_with(',') && l.contains(" = "))
            },
            30,
        ),
        (
            "`match` statement",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("match ") && l.ends_with('{'))
            },
            250,
        ),
        (
            "`let` = `match`",
            |s| {
                s.lines()
                    .any(|l| l.contains(" = match ") && l.ends_with('{'))
            },
            60,
        ),
        ("inline `match`", |s| s.contains("(match "), 150),
        ("`if let`", |s| s.contains("if let "), 150),
        ("`while let`", |s| s.contains("while let "), 15),
        (
            "`let`-`else`",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("let ") && l.ends_with(" else {"))
            },
            120,
        ),
        (
            "guard",
            |s| {
                s.lines()
                    .any(|l| l.contains(" if ") && l.contains(" => ") && !l.contains("(if "))
            },
            100,
        ),
        ("`ref` binding", |s| s.contains("ref "), 120),
        ("`ref mut` binding", |s| s.contains("ref mut "), 60),
        ("`@` binding", |s| s.contains(" @ "), 60),
        ("range pattern", |s| s.contains("..="), 100),
        (
            "or-pattern",
            |s| s.lines().any(|l| l.contains(" | ") && l.contains("=>")),
            100,
        ),
        ("match on a reference", |s| s.contains("match &"), 120),
        (
            "`impl` block of an enum",
            |s| {
                s.lines().any(|l| {
                    l.starts_with("impl ")
                        && !l.starts_with("impl Drop")
                        && s.contains(&format!("\nenum {} {{", &l[5..l.len() - 2]))
                })
            },
            80,
        ),
    ];
    let mut counts = [0usize; 18];
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        for (count, (_, check, _)) in counts.iter_mut().zip(&checks) {
            *count += usize::from(check(&src));
        }
    }
    let low: Vec<String> = checks
        .iter()
        .zip(counts)
        .filter(|((_, _, min), count)| count < min)
        .map(|((name, _, min), count)| format!("{name} in {count} (expected {min})"))
        .collect();
    assert!(low.is_empty(), "of {SEEDS} programs: {}", low.join(", "));
}

/// Whether `s` has a reference to a slice type of a non-array element (`&[i32]`,
/// `&mut [Point]`).
fn has_slice_type(s: &str) -> bool {
    ["&[", "&mut ["].iter().any(|open| {
        s.match_indices(open).any(|(at, _)| {
            let rest = &s[at + open.len()..];
            rest.find(']')
                .is_some_and(|end| !rest[..end].contains([';', '[', '(', ' ']))
        })
    })
}

/// Whether a type annotation or signature line of `s` has a tuple type holding an array
/// type (`(i32, [u8; 2])`).
fn has_array_in_tuple(s: &str) -> bool {
    s.lines().any(|l| {
        l.match_indices(": (").any(|(at, _)| {
            let ty = &l[at + 2..];
            let end = ty.find(" =").or_else(|| ty.find(')')).unwrap_or(ty.len());
            let ty = &ty[..end];
            (ty.contains("([") || ty.contains(", [")) && ty.contains("; ")
        })
    })
}

/// Whether a line of `s` starts with a place whose last part is an index and assigns
/// to it with `op` (`a[1] = `, `s.A[(k % 2usize)] += `).
fn writes_element(s: &str, op: &str) -> bool {
    s.lines().any(|l| {
        let t = l.trim_start();
        t.find(&format!("] {op} ")).is_some_and(|at| {
            t.starts_with(|c: char| c.is_ascii_lowercase()) && !t[..at].contains(" = ")
        })
    })
}

/// Whether `s` borrows an element of a local (`&a[1]`, `&mut p.Arr[i]`).
fn borrows_element(s: &str) -> bool {
    s.match_indices('&').any(|(at, _)| {
        let rest = &s[at + 1..];
        let rest = rest.strip_prefix("mut ").unwrap_or(rest);
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        name.starts_with(|c: char| c.is_ascii_lowercase()) && rest[name.len()..].starts_with('[')
    })
}

/// Whether an enum of `s` has a variant with an array in its payload: a variant line
/// `    Name(.., [T; N], ..),` or `    Name { F: [T; N] },`.
fn has_array_payload(s: &str) -> bool {
    s.lines().any(|l| {
        let Some(t) = l.strip_prefix("    ") else {
            return false;
        };
        let name_end = t
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(t.len());
        t.starts_with(|c: char| c.is_ascii_uppercase())
            && (t[name_end..].starts_with('(') || t[name_end..].starts_with(" { "))
            && t.ends_with(',')
            && t.contains("; ")
    })
}

/// Whether `s` has an array type of a struct or enum with an `impl Drop`.
fn has_drop_array(s: &str) -> bool {
    s.lines()
        .filter_map(|l| l.strip_prefix("impl Drop for "))
        .any(|rest| {
            let name = rest.trim_end_matches(" {");
            s.contains(&format!("[{name}; "))
        })
}

/// Arrays, slices and indexing (#9) are part of the subset: a fair share of programs
/// declare array types of every kind of element and length, and use every construct.
#[test]
fn programs_use_arrays() {
    let knobs = Knobs::default();
    let checks: [Check; 18] = [
        (
            "array type",
            |s| s.contains(": [") || s.contains("-> ["),
            250,
        ),
        ("nested array type", |s| s.contains("[["), 25),
        ("zero-length array", |s| s.contains("; 0]"), 40),
        ("one-element array", |s| s.contains("; 1]"), 60),
        (
            "length from a constant",
            |s| {
                ["LEN", "SIZE", "CAP", "SLOTS", "ROWS", "COLS"]
                    .iter()
                    .any(|n| s.contains(&format!("; {n}]")) || s.contains(&format!("; {n} + ")))
            },
            60,
        ),
        ("slice type", has_slice_type, 70),
        (
            "array field",
            |s| {
                s.lines()
                    .any(|l| l.starts_with("    ") && l.ends_with("],") && l.contains(": ["))
            },
            60,
        ),
        ("array in a tuple", has_array_in_tuple, 40),
        ("array in an enum payload", has_array_payload, 30),
        ("array of an `impl Drop` type", has_drop_array, 40),
        ("`.len()`", |s| s.contains(".len()"), 200),
        (
            "`while i < s.len()`",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("while ") && l.ends_with(".len() {"))
            },
            100,
        ),
        (
            "computed index",
            |s| s.contains("[(") && s.contains(" % "),
            150,
        ),
        ("element write", |s| writes_element(s, "="), 100),
        (
            "element compound assignment",
            |s| {
                ["+=", "-=", "*=", "^=", "|="]
                    .iter()
                    .any(|op| writes_element(s, op))
            },
            20,
        ),
        ("borrow of an element", borrows_element, 70),
        (
            "repeat expression",
            |s| s.contains("; ") && s.contains("= ["),
            100,
        ),
        (
            "out-of-bounds-prone index",
            |s| s.contains(" + 1usize)]") || s.contains(" - 1usize)]"),
            30,
        ),
    ];
    let mut counts = [0usize; 18];
    for seed in 1..=SEEDS {
        let src = generate(seed, &knobs);
        for (count, (_, check, _)) in counts.iter_mut().zip(&checks) {
            *count += usize::from(check(&src));
        }
    }
    let report: Vec<String> = checks
        .iter()
        .zip(counts)
        .map(|((name, _, min), count)| format!("{name} in {count} (expected {min})"))
        .collect();
    let low = checks
        .iter()
        .zip(counts)
        .any(|((_, _, min), count)| count < *min);
    eprintln!("of {SEEDS} programs: {}", report.join(", "));
    assert!(!low, "of {SEEDS} programs: {}", report.join(", "));
}
