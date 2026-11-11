//! Every generated program must be valid Sabiko up to borrow checking, borrow-clean
//! programs must pass the whole pipeline, and generation must be deterministic.

use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};
use sabiko_testgen::{generate, insert_noops, Knobs};

const SEEDS: u64 = 500;

/// The seeds the coverage tests (`programs_*`) count over: 500 from
/// `SABIKO_COVERAGE_BASE_SEED` (default 1). `scripts/measure_coverage.py` counts over other
/// windows to set the thresholds.
fn coverage_seeds() -> std::ops::RangeInclusive<u64> {
    let base = std::env::var("SABIKO_COVERAGE_BASE_SEED")
        .ok()
        .and_then(|b| b.parse().ok())
        .unwrap_or(1);
    base..=base + SEEDS - 1
}

/// Reports the coverage counts of `test`, `(name, programs with it, least allowed)`, as
/// `coverage <test>: <name> = <count> (min <min>)` lines on stderr, and fails when a count
/// is under its threshold, unless `SABIKO_COVERAGE_MEASURE` is set (measuring, see
/// `scripts/measure_coverage.py`).
///
/// Each threshold is set from a measurement, not by hand: over the 10 windows of 500
/// seeds from 1 to 5000, a threshold is 75% of the smallest count (rounded down, at least
/// 1), so it survives the reshuffle a change of the generator's draws makes, and still
/// fails when a construct stops being generated. Re-measure with the script after a
/// generator change that moves the counts, and update the thresholds with the numbers it
/// prints (the measured numbers are in `docs/TESTING.md`).
fn coverage(test: &str, counts: &[(&str, usize, usize)]) {
    let mut low = Vec::new();
    for &(name, count, min) in counts {
        eprintln!("coverage {test}: {name} = {count} (min {min})");
        if count < min {
            low.push(format!("{name} in {count} (expected {min})"));
        }
    }
    if std::env::var_os("SABIKO_COVERAGE_MEASURE").is_none() {
        assert!(
            low.is_empty(),
            "{test}, of {SEEDS} programs: {}",
            low.join(", ")
        );
    }
}

/// Counts the programs of [`coverage_seeds`] that have each property of `checks`, and
/// reports them with [`coverage`].
fn count_checks(test: &str, knobs: &Knobs, checks: &[Check]) {
    let mut counts = vec![0usize; checks.len()];
    for seed in coverage_seeds() {
        let src = generate(seed, knobs);
        for (count, (_, check, _)) in counts.iter_mut().zip(checks) {
            *count += usize::from(check(&src));
        }
    }
    let rows: Vec<(&str, usize, usize)> = checks
        .iter()
        .zip(counts)
        .map(|((name, _, min), count)| (*name, count, *min))
        .collect();
    coverage(test, &rows);
}

/// Parses, resolves and type-checks `src`; returns the rendered errors.
fn front_end_errors(name: &str, src: &str) -> String {
    let mut sm = SourceMap::new();
    let mut sink = DiagnosticSink::new();
    if let Some((ast, resolve)) =
        sabiko_driver::parse_and_resolve(name, src, &mut sm, &mut sink).program
    {
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
            // As rustc's prelude for generated programs allows them (decision 73).
            allowed_lints: sabiko_driver::AllowedLints {
                arithmetic_overflow: true,
                unconditional_panic: true,
            },
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
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        signed += usize::from(src.contains("i128"));
        unsigned += usize::from(src.contains("u128"));
        min_literal += usize::from(src.contains("-170141183460469231731687303715884105728i128"));
    }
    coverage(
        "128_bit_integers",
        &[
            ("i128", signed, 264),
            ("u128", unsigned, 268),
            ("the `i128::MIN` literal", min_literal, 4),
        ],
    );
}

/// Methods are part of the subset: a fair share of programs declare `impl` blocks and call
/// methods, some of them in the shapes two-phase borrows allow, an argument reading the
/// receiver (`v.Add(v.X)`).
#[test]
fn programs_use_methods() {
    let knobs = Knobs::default();
    let (mut impls, mut calls, mut reads_receiver) = (0, 0, 0);
    for seed in coverage_seeds() {
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
    coverage(
        "methods",
        &[
            ("impl block", impls, 302),
            ("method call", calls, 224),
            ("argument reading the receiver", reads_receiver, 10),
        ],
    );
}

/// Drops (#12) are part of the subset: a fair share of programs declare structs with
/// `impl Drop` and keep values of them in locals, so drop order, moves, reassignments,
/// early exits and unwinding are all observable in the output.
#[test]
fn programs_use_drop() {
    let knobs = Knobs::default();
    let mut with_drop = 0;
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        with_drop += usize::from(src.contains("\nimpl Drop for "));
    }
    coverage("drop", &[("`impl Drop`", with_drop, 163)]);
}

/// Some generated patterns are unreachable (E0330) or irrefutable where a refutable one is
/// expected (E0331): the code they make unreachable is borrow-checked by both compilers
/// (#47), and a construct that silently stopped making it fails here.
#[test]
fn programs_have_unreachable_patterns() {
    let mut found = 0;
    for knobs in [Knobs::default(), Knobs::borrow_clean()] {
        for seed in coverage_seeds() {
            let src = generate(seed, &knobs);
            let mut sink = DiagnosticSink::new();
            let mut sm = SourceMap::new();
            let (ast, resolve) =
                sabiko_driver::parse_and_resolve("u.sabi", &src, &mut sm, &mut sink)
                    .program
                    .expect("generated programs parse");
            let _ = sabiko_types::typeck(&ast, &resolve, &mut sink);
            let codes: Vec<String> = sink
                .diagnostics()
                .iter()
                .map(|d| d.code.to_string())
                .collect();
            found += usize::from(codes.iter().any(|c| c == "E0330" || c == "E0331"));
        }
    }
    coverage(
        "unreachable_patterns",
        &[("unreachable pattern (default and borrow-clean)", found, 79)],
    );
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
        ("enum", |s| s.contains("\nenum "), 315),
        (
            "Copy enum",
            |s| s.contains("#[derive(Copy, Clone)]\nenum "),
            165,
        ),
        (
            "`impl Drop` for an enum",
            |s| {
                s.lines().any(|l| {
                    l.starts_with("impl Drop for ")
                        && s.contains(&format!("\nenum {} {{", &l[14..l.len() - 2]))
                })
            },
            81,
        ),
        (
            "explicit discriminant",
            |s| {
                s.lines()
                    .any(|l| l.starts_with("    ") && l.ends_with(',') && l.contains(" = "))
            },
            48,
        ),
        (
            "`match` statement",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("match ") && l.ends_with('{'))
            },
            222,
        ),
        (
            "`let` = `match`",
            |s| {
                s.lines()
                    .any(|l| l.contains(" = match ") && l.ends_with('{'))
            },
            27,
        ),
        ("inline `match`", |s| s.contains("(match "), 202),
        ("`if let`", |s| s.contains("if let "), 159),
        ("`while let`", |s| s.contains("while let "), 11),
        (
            "`let`-`else`",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("let ") && l.ends_with(" else {"))
            },
            96,
        ),
        (
            "guard",
            |s| {
                s.lines()
                    .any(|l| l.contains(" if ") && l.contains(" => ") && !l.contains("(if "))
            },
            132,
        ),
        ("`ref` binding", |s| s.contains("ref "), 126),
        ("`ref mut` binding", |s| s.contains("ref mut "), 61),
        ("`@` binding", |s| s.contains(" @ "), 48),
        ("range pattern", |s| s.contains("..="), 249),
        (
            "or-pattern",
            |s| s.lines().any(|l| l.contains(" | ") && l.contains("=>")),
            125,
        ),
        ("match on a reference", |s| s.contains("match &"), 185),
        (
            "`impl` block of an enum",
            |s| {
                s.lines().any(|l| {
                    l.starts_with("impl ")
                        && !l.starts_with("impl Drop")
                        && s.contains(&format!("\nenum {} {{", &l[5..l.len() - 2]))
                })
            },
            120,
        ),
    ];
    count_checks("enums_and_match", &knobs, &checks);
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
            312,
        ),
        ("nested array type", |s| s.contains("[["), 19),
        ("zero-length array", |s| s.contains("; 0]"), 36),
        ("one-element array", |s| s.contains("; 1]"), 85),
        (
            "length from a constant",
            |s| {
                ["LEN", "SIZE", "CAP", "SLOTS", "ROWS", "COLS"]
                    .iter()
                    .any(|n| s.contains(&format!("; {n}]")) || s.contains(&format!("; {n} + ")))
            },
            63,
        ),
        ("slice type", has_slice_type, 98),
        (
            "array field",
            |s| {
                s.lines()
                    .any(|l| l.starts_with("    ") && l.ends_with("],") && l.contains(": ["))
            },
            117,
        ),
        ("array in a tuple", has_array_in_tuple, 33),
        ("array in an enum payload", has_array_payload, 111),
        ("array of an `impl Drop` type", has_drop_array, 34),
        ("`.len()`", |s| s.contains(".len()"), 277),
        (
            "`while i < s.len()`",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("while ") && l.ends_with(".len() {"))
            },
            51,
        ),
        (
            "computed index",
            |s| s.contains("[(") && s.contains(" % "),
            270,
        ),
        ("element write", |s| writes_element(s, "="), 231),
        (
            "element compound assignment",
            |s| {
                ["+=", "-=", "*=", "^=", "|="]
                    .iter()
                    .any(|op| writes_element(s, op))
            },
            12,
        ),
        ("borrow of an element", borrows_element, 158),
        (
            "repeat expression",
            |s| s.contains("; ") && s.contains("= ["),
            222,
        ),
        (
            "out-of-bounds-prone index",
            |s| s.contains(" + 1usize)]") || s.contains(" - 1usize)]"),
            124,
        ),
    ];
    count_checks("arrays", &knobs, &checks);
}

/// The bracketed groups of a line that hold a range at their top level (`[1..]`,
/// `[..(s.len() - 1usize)]`), as `(start, end)` byte offsets of the brackets, with the
/// range's form: `a..b`, `a..`, `..b`, `..`, `a..=b` or `..=b`.
fn range_groups(line: &str) -> Vec<(usize, usize, &'static str)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut stack: Vec<(usize, Option<usize>)> = Vec::new();
    let mut parens = 0usize;
    let mut quoted = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if quoted {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                quoted = false;
            }
        } else {
            match c {
                b'"' => quoted = true,
                b'[' => stack.push((i, None)),
                b'(' => parens += 1,
                b')' => parens = parens.saturating_sub(1),
                b'.' if bytes.get(i + 1) == Some(&b'.') => {
                    if let Some(top) = stack.last_mut() {
                        top.1.get_or_insert(i);
                    }
                    i += 1;
                }
                b']' => {
                    if let Some((open, Some(dots))) = stack.pop() {
                        let lo = !line[open + 1..dots].trim().is_empty();
                        let inclusive = bytes.get(dots + 2) == Some(&b'=');
                        let hi = !line[dots + if inclusive { 3 } else { 2 }..i]
                            .trim()
                            .is_empty();
                        let form = match (lo, inclusive, hi) {
                            (true, true, _) => "a..=b",
                            (false, true, _) => "..=b",
                            (true, false, true) => "a..b",
                            (true, false, false) => "a..",
                            (false, false, true) => "..b",
                            (false, false, false) => "..",
                        };
                        out.push((open, i, form));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    let _ = parens;
    out
}

fn any_range(s: &str, pred: impl Fn(&str, usize, usize, &str) -> bool) -> bool {
    s.lines().any(|l| {
        range_groups(l)
            .into_iter()
            .any(|(open, close, form)| pred(l, open, close, form))
    })
}

/// Whether a line of `s` starts with a place that ends in an element of a range place
/// and assigns to it (`a[1..][k] = `, `p.A[..2][0].X += `).
fn writes_through_range(s: &str) -> bool {
    s.lines().any(|l| {
        let t = l.trim_start();
        let offset = l.len() - t.len();
        if !t.starts_with(|c: char| c.is_ascii_lowercase()) {
            return false;
        }
        let Some(assign) = ["= ", "+= ", "-= ", "*= ", "^= ", "|= "]
            .iter()
            .filter_map(|op| t.find(&format!("] {op}")))
            .min()
        else {
            return false;
        };
        range_groups(l)
            .iter()
            .any(|&(_, close, _)| close < offset + assign && l[close + 1..].starts_with('['))
    })
}

/// Strings and range indexing (#10) are part of the subset: a fair share of programs use
/// `&str` locals, parameters and returns, literals with escapes, every range form on
/// strings, arrays and slices, comparisons, matches on strings and the loops.
#[test]
#[allow(clippy::too_many_lines)] // one table of checks
fn programs_use_strings_and_ranges() {
    let knobs = Knobs::default();
    let checks: [Check; 28] = [
        (
            "`&str` parameter",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with("fn ") && l.contains(": &str"))
            },
            59,
        ),
        ("`-> &str`", |s| s.contains("-> &str {"), 18),
        ("`&str` local", |s| s.contains(": &str = "), 168),
        (
            "escape in a string",
            |s| {
                [
                    "\\n", "\\t", "\\\\", "\\\"", "\\'", "\\0", "\\x41", "\\u{61}",
                ]
                .iter()
                .any(|e| s.contains(e))
            },
            152,
        ),
        ("`a..b`", |s| any_range(s, |_, _, _, f| f == "a..b"), 193),
        ("`a..`", |s| any_range(s, |_, _, _, f| f == "a.."), 167),
        ("`..b`", |s| any_range(s, |_, _, _, f| f == "..b"), 142),
        ("`..`", |s| any_range(s, |_, _, _, f| f == ".."), 83),
        ("`a..=b`", |s| any_range(s, |_, _, _, f| f == "a..=b"), 157),
        ("`..=b`", |s| any_range(s, |_, _, _, f| f == "..=b"), 120),
        (
            "computed bound",
            |s| any_range(s, |l, open, close, _| l[open..close].contains(" % ")),
            171,
        ),
        (
            "bound measuring the base",
            |s| any_range(s, |l, open, close, _| l[open..close].contains(".len()")),
            162,
        ),
        (
            "slice of a string literal",
            |s| any_range(s, |l, open, _, _| l[..open].ends_with('"')),
            303,
        ),
        (
            "range of a range",
            |s| {
                s.lines().any(|l| {
                    let groups = range_groups(l);
                    groups
                        .iter()
                        .any(|&(_, close, _)| groups.iter().any(|&(open, _, _)| open == close + 1))
                })
            },
            156,
        ),
        (
            "`.len()` of a range place",
            |s| any_range(s, |l, _, close, _| l[close + 1..].starts_with(".len()")),
            192,
        ),
        (
            "`&mut` of a range place",
            |s| {
                any_range(s, |l, open, _, _| {
                    let before = &l[..open];
                    before
                        .rfind("&mut ")
                        .is_some_and(|at| !before[at + 5..].contains([' ', '(', '[']))
                })
            },
            12,
        ),
        (
            "element of a range place",
            |s| any_range(s, |l, _, close, _| l[close + 1..].starts_with('[')),
            209,
        ),
        ("write through a range place", writes_through_range, 8),
        (
            "`while i < x[..].len()`",
            |s| {
                s.lines().any(|l| {
                    l.trim_start().starts_with("while ")
                        && l.ends_with("].len() {")
                        && !range_groups(l).is_empty()
                })
            },
            12,
        ),
        (
            "string comparison",
            |s| {
                ["==", "!=", "<", "<=", ">", ">="]
                    .iter()
                    .any(|op| s.contains(&format!(" {op} \"")) || s.contains(&format!("\" {op} ")))
            },
            89,
        ),
        (
            "`match` arm of string literals",
            |s| {
                s.lines()
                    .any(|l| l.trim_start().starts_with('"') && l.contains(" => "))
            },
            30,
        ),
        (
            "string or-pattern",
            |s| {
                s.lines()
                    .any(|l| l.contains("\" | \"") && l.contains(" => "))
            },
            71,
        ),
        // One string binding arm in ten is unguarded since #47 (it was 26 before).
        (
            "guarded binding of a string",
            |s| {
                s.lines().any(|l| {
                    let t = l.trim_start();
                    let name: String = t.chars().take_while(char::is_ascii_lowercase).collect();
                    !name.is_empty()
                        && t[name.len()..].starts_with(" if ")
                        && (t.contains(&format!(" if {name}.len()"))
                            || t.contains(&format!(" if {name} ")))
                })
            },
            7,
        ),
        ("`if let` on a string", |s| s.contains("if let \""), 36),
        (
            "`let`-`else` on a string",
            |s| {
                s.lines().any(|l| {
                    let t = l.trim_start();
                    (t.starts_with("let \"") || t.starts_with("let (\"")) && t.ends_with(" else {")
                })
            },
            35,
        ),
        ("`while let` on a string", |s| s.contains("while let \""), 3),
        (
            "`while i < s.len()` over a string",
            |s| {
                let lines: Vec<&str> = s.lines().collect();
                lines.windows(2).any(|w| {
                    let t = w[0].trim_start();
                    t.starts_with("while ")
                        && t.ends_with(".len() {")
                        && (w[1].contains("..=") || w[1].contains("..") && w[1].contains('&'))
                        && w[1].contains('[')
                })
            },
            21,
        ),
        (
            "shrinking a `&str`",
            |s| {
                s.lines().any(|l| {
                    let t = l.trim_start();
                    let name: String = t.chars().take_while(char::is_ascii_lowercase).collect();
                    !name.is_empty() && t[name.len()..].starts_with(&format!(" = &{name}["))
                })
            },
            13,
        ),
    ];
    count_checks("strings_and_ranges", &knobs, &checks);
}

/// Whether a function of `s` has a parameter of a type starting with `ty`.
fn param(s: &str, ty: &str) -> bool {
    s.lines()
        .any(|l| l.trim_start().starts_with("fn ") && l.contains(&format!(": {ty}")))
}

/// Whether a line of `s` is `<lead><name><rest>`, with `<name>` a lowercase local name.
fn line_with_name(s: &str, lead: &str, rest: &str) -> bool {
    s.lines().any(|l| {
        let Some(t) = l.trim_start().strip_prefix(lead) else {
            return false;
        };
        let name: String = t.chars().take_while(char::is_ascii_lowercase).collect();
        !name.is_empty() && t[name.len()..].starts_with(rest)
    })
}

/// References to references (#11) are part of the subset: double reference locals made
/// by borrowing reference locals, reads and writes through them, retargets through `&mut
/// &T`, double reference parameters and the arguments passed to them.
#[test]
fn programs_use_references_to_references() {
    let knobs = Knobs::default();
    let checks: [Check; 11] = [
        ("`&&T` parameter", |s| param(s, "&&"), 56),
        ("`&&mut T` parameter", |s| param(s, "&&mut "), 11),
        ("`&mut &mut T` parameter", |s| param(s, "&mut &mut "), 13),
        ("`&&str` parameter", |s| param(s, "&&str"), 13),
        (
            "annotated double reference local",
            |s| {
                s.lines().any(|l| {
                    let t = l.trim_start();
                    t.starts_with("let ") && (t.contains(": &&") || t.contains(": &mut &"))
                })
            },
            78,
        ),
        (
            "`&mut` of a reference local",
            |s| line_with_name(s, "let ", ": &mut &"),
            36,
        ),
        (
            "write through `&mut &mut`",
            |s| line_with_name(s, "**", " = "),
            5,
        ),
        (
            "retarget through `&mut &T`",
            |s| line_with_name(s, "*", " = &") || line_with_name(s, "*", " = \""),
            48,
        ),
        (
            "`&&x` argument",
            |s| s.contains("(&&") || s.contains(", &&") || s.contains("(& &mut "),
            9,
        ),
        (
            "explicit dereferences of a double reference",
            |s| s.lines().any(|l| l.contains("(**")),
            39,
        ),
        (
            "`let` of a borrowed borrow",
            |s| line_with_name(s, "let ", " = &&") || line_with_name(s, "let ", " = &mut &"),
            14,
        ),
    ];
    count_checks("references_to_references", &knobs, &checks);
}

/// Whether a line indexes an array literal of references (#43): `["a", s][i]`,
/// `[&x, r][..2][k]`.
fn ref_array_element(line: &str) -> bool {
    (line.contains("[&") || line.contains("[\"")) && line.contains("][")
}

/// The generator indexes temporary arrays of references (#43, decision 77), and some of
/// them through a range (a slice of references).
#[test]
fn programs_use_arrays_of_references() {
    let knobs = Knobs::default();
    let (mut any, mut ranged) = (0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        any += usize::from(src.lines().any(ref_array_element));
        ranged += usize::from(
            src.lines()
                .any(|l| ref_array_element(l) && l.contains("..][")),
        );
    }
    coverage(
        "arrays_of_references",
        &[
            ("index an array of references", any, 70),
            ("a range of one", ranged, 11),
        ],
    );
}

/// Whether `line` binds a reference to a temporary extended through a block's tail, an
/// `if`'s branches or a `match`'s arms (#88): `let r = { &e };`, `let r = if c { &a } else
/// { &b };`, `let r = match n { 0 => &a, _ => { &b } };`.
fn extended_borrow(line: &str) -> Option<&'static str> {
    let l = line.trim_start();
    if !l.starts_with("let ") {
        return None;
    }
    let (_, init) = l.split_once(" = ")?;
    if init.starts_with("{ &") || init.starts_with("{ { &") {
        Some("block")
    } else if init.starts_with("if ") && init.contains(" { &") && init.contains("} else { &") {
        Some("if")
    } else if init.starts_with("match ") && init.contains("{ 0 => &") {
        Some("match")
    } else {
        None
    }
}

/// The generator borrows temporaries in extending positions under blocks, `if`s and
/// `match`es (#88).
#[test]
fn programs_extend_temporaries_through_blocks_and_branches() {
    let knobs = Knobs::default();
    let (mut block, mut branch, mut arm) = (0, 0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        let kinds: Vec<&str> = src.lines().filter_map(extended_borrow).collect();
        block += usize::from(kinds.contains(&"block"));
        branch += usize::from(kinds.contains(&"if"));
        arm += usize::from(kinds.contains(&"match"));
    }
    coverage(
        "extended_temporaries",
        &[
            ("extend through a block", block, 63),
            ("through an `if`", branch, 62),
            ("through a `match`", arm, 34),
        ],
    );
}

/// The kinds of let chain an `if` or `while` line holds: `let` after another operand, more
/// than one `let`, and a `bool` operand after a `let`.
fn let_chain(line: &str) -> Option<(bool, bool, bool)> {
    let line = line.trim_start();
    let cond = line
        .strip_prefix("if ")
        .or_else(|| line.strip_prefix("while "))?
        .strip_suffix(" {")?;
    let operands: Vec<&str> = cond.split(" && ").collect();
    let lets = operands.iter().filter(|o| o.starts_with("let ")).count();
    if lets == 0 || operands.len() < 2 {
        return None;
    }
    let first_let = operands.iter().position(|o| o.starts_with("let "))?;
    Some((
        first_let > 0,
        lets > 1,
        operands[first_let + 1..]
            .iter()
            .any(|o| !o.starts_with("let ")),
    ))
}

#[test]
fn programs_use_let_chains() {
    let knobs = Knobs::default();
    let (mut any, mut leading, mut two_lets, mut after) = (0, 0, 0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        let chains: Vec<(bool, bool, bool)> = src.lines().filter_map(let_chain).collect();
        any += usize::from(!chains.is_empty());
        leading += usize::from(chains.iter().any(|c| c.0));
        two_lets += usize::from(chains.iter().any(|c| c.1));
        after += usize::from(chains.iter().any(|c| c.2));
    }
    coverage(
        "let_chains",
        &[
            ("let chain", any, 26),
            ("led by a `bool`", leading, 5),
            ("two `let`s", two_lets, 15),
            ("a `bool` after a `let`", after, 18),
        ],
    );
}

#[test]
fn programs_use_if_let_guards() {
    let knobs = Knobs::default();
    let (mut any, mut chained) = (0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        let guards: Vec<&str> = src
            .lines()
            .filter_map(|l| l.split_once(" if let ").map(|(_, rest)| rest))
            .filter(|rest| rest.contains("=>"))
            .collect();
        any += usize::from(!guards.is_empty());
        chained += usize::from(
            guards
                .iter()
                .any(|g| g.split("=>").next().is_some_and(|c| c.contains(" && "))),
        );
    }
    coverage(
        "if_let_guards",
        &[("`if let` guard", any, 21), ("one with `&&`", chained, 7)],
    );
}

#[test]
fn programs_use_aggregates_of_references() {
    let knobs = Knobs::default();
    let (mut arrays, mut slices, mut tuples, mut muts) = (0, 0, 0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        arrays += usize::from(src.contains(": [&") && !src.contains(": [&mut"));
        slices += usize::from(src.contains(": &[&"));
        tuples += usize::from(src.contains(": (&"));
        muts += usize::from(src.contains(": [&mut "));
    }
    coverage(
        "aggregates_of_references",
        &[
            ("`[&T; N]` local", arrays, 38),
            ("`&[&T]` local", slices, 24),
            ("`(&T, U)` local", tuples, 51),
            ("`[&mut T; 2]` local", muts, 90),
        ],
    );
}

/// Generic functions and types (#114, #115): about half the programs have the helpers,
/// and their bodies use each of them, `GenId` now and then with a turbofish.
#[test]
fn programs_use_trait_helpers() {
    let knobs = Knobs::default();
    let (mut keeps, mut paths, mut kept, mut refs, mut signs, mut sums) = (0, 0, 0, 0, 0, 0);
    let (mut consts, mut copies) = (0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        // After the helpers' last line.
        let body = src
            .split("fn GenSigns")
            .nth(1)
            .and_then(|rest| rest.split_once('\n'))
            .map_or("", |(_, rest)| rest);
        keeps += usize::from(body.contains(").Keep()"));
        paths += usize::from(body.contains("GenKeep::Keep("));
        kept += usize::from(body.contains("GenKept("));
        refs += usize::from(body.contains(").KeepRef()") && body.contains("(&*"));
        signs += usize::from(body.contains(").Sign()"));
        sums += usize::from(body.contains("GenSigns("));
        consts += usize::from(body.contains("::UNIT") || body.contains("::BIAS"));
        copies += usize::from(
            body.contains("GenCopied(")
                || body.contains(").clone()")
                || body.contains("GenCloned("),
        );
    }
    coverage(
        "trait_helpers",
        &[
            ("`.Keep()`", keeps, 47),
            ("`GenKeep::Keep`", paths, 37),
            ("`GenKept`", kept, 36),
            ("`.KeepRef()`", refs, 15),
            ("`.Sign()`", signs, 33),
            ("`GenSigns`", sums, 33),
            ("a trait constant", consts, 17),
            ("`Copy`/`Clone` through a bound", copies, 27),
        ],
    );
}

/// The `core` prelude's `Option` and `Result` (#13, decision 90) around values and shared
/// references.
#[test]
fn programs_use_the_prelude() {
    let knobs = Knobs::default();
    let patterns = [
        (").unwrap()", 45),
        (").expect(\"generated\")", 5),
        (").unwrap_or(", 14),
        ("None::<", 12),
        ("Ok::<", 6),
        ("Err::<", 4),
        ("Some(&", 7),
        // A type written as a generic argument in an annotation (#137).
        (": Option<", 1),
        (": Result<", 1),
    ];
    let mut counts = [0usize; 9];
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        for (count, (pattern, _)) in counts.iter_mut().zip(patterns) {
            *count += usize::from(src.contains(pattern));
        }
    }
    let rows: Vec<(&str, usize, usize)> = patterns
        .iter()
        .zip(counts)
        .map(|((p, min), n)| (*p, n, *min))
        .collect();
    coverage("prelude", &rows);
}

#[test]
fn programs_use_generic_helpers() {
    let knobs = Knobs::default();
    let (mut ids, mut turbofish, mut picks, mut firsts) = (0, 0, 0, 0);
    let (mut ors, mut nones, mut boxes) = (0, 0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        let body = src.split("fn GenOr").nth(1).unwrap_or("");
        ids += usize::from(body.contains("GenId("));
        turbofish += usize::from(body.contains("GenId::<"));
        picks += usize::from(body.contains("GenPick("));
        firsts += usize::from(body.contains("GenFirst("));
        ors += usize::from(body.contains("GenOr(GenOpt::Yes("));
        nones += usize::from(body.contains("GenOr(GenOpt::No,"));
        boxes += usize::from(body.contains("GenBox { Value:"));
    }
    coverage(
        "generic_helpers",
        &[
            ("`GenId`", ids, 75),
            ("`GenId::<T>`", turbofish, 39),
            ("`GenPick`", picks, 73),
            ("`GenFirst`", firsts, 18),
            ("`GenOr(GenOpt::Yes(..), ..)`", ors, 50),
            ("`GenOr(GenOpt::No, ..)`", nones, 30),
            ("`GenBox`", boxes, 57),
        ],
    );
}

/// Whether `name` is declared a `&mut` reference on `line`: a parameter or annotated
/// `let` (`r: &mut T`), `&mut self`, or `let r = &mut ..;`. Locals initialised from a call
/// returning a `&mut` are not seen (a lower bound is enough for coverage).
fn declares_mut_ref(line: &str, name: &str) -> bool {
    if name == "self" {
        return line.contains("(&mut self");
    }
    let t = line.trim();
    line.contains(&format!("({name}: &mut "))
        || line.contains(&format!(" {name}: &mut "))
        || t.starts_with(&format!("let {name} = &mut "))
        || t.starts_with(&format!("let mut {name} = &mut "))
}

/// Whether `name` is a `&mut` reference where `src`'s line `at` is: declared so in the
/// function that line is in (the generator never shadows).
fn is_mut_ref_at(lines: &[&str], at: usize, name: &str) -> bool {
    for line in lines[..at].iter().rev() {
        if declares_mut_ref(line, name) {
            return true;
        }
        if line.trim_start().starts_with("fn ") {
            return false;
        }
    }
    false
}

/// A `&mut` reference local passed where a type parameter is expected (#132): moved by
/// `let q = GenId(r);`, `let q = GenBox { Value: r }.Value;` or `let q =
/// Some(r).unwrap();`, or reborrowed by `let q: &mut T = GenId(r);`, whose annotation
/// decides `T` first.
#[test]
fn programs_pass_mut_ref_locals_to_type_parameters() {
    let knobs = Knobs::default();
    let (mut moved, mut reborrowed) = (0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        let lines: Vec<&str> = src.lines().collect();
        let (mut m, mut r) = (false, false);
        for (at, line) in lines.iter().map(|l| l.trim()).enumerate() {
            let Some((lhs, rhs)) = line.strip_prefix("let ").and_then(|l| l.split_once(" = "))
            else {
                continue;
            };
            let arg = ["GenId(", "GenBox { Value: ", "Some("]
                .iter()
                .find_map(|p| rhs.strip_prefix(p))
                .and_then(|r| {
                    [");", " }.Value;", ").unwrap();"]
                        .iter()
                        .find_map(|s| r.strip_suffix(s))
                });
            if arg.is_some_and(|a| is_mut_ref_at(&lines, at, a)) {
                if lhs.contains(": &mut ") {
                    r |= rhs.starts_with("GenId(");
                } else {
                    m = true;
                }
            }
        }
        moved += usize::from(m);
        reborrowed += usize::from(r);
    }
    coverage(
        "mut_ref_locals_to_type_parameters",
        &[
            ("moved into a type parameter", moved, 3),
            ("reborrowed through an annotated `GenId`", reborrowed, 2),
        ],
    );
}

/// The patterns of a program's `let`s, `if let`s and `while let`s (from the `let` on),
/// and roughly those of its `match` arms, inline ones included (what precedes each ` => `
/// back to a `{ ` or `, `).
fn patterns(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in src.lines() {
        if let Some(i) = line.find("let ") {
            out.push(&line[i + 4..]);
        }
        let mut rest = line;
        while let Some((before, after)) = rest.split_once(" => ") {
            let start = [before.rfind("{ "), before.rfind(", ")]
                .into_iter()
                .flatten()
                .max()
                .map_or(0, |i| i + 2);
            out.push(before[start..].trim_start());
            rest = after;
        }
    }
    out
}

/// `&` and `&mut` patterns, half-open range patterns and float patterns (#49) are part of
/// the subset: a fair share of programs use each.
#[test]
fn programs_use_reference_open_range_and_float_patterns() {
    let knobs = Knobs::default();
    let (mut deref, mut deref_mut, mut open, mut float) = (0, 0, 0, 0);
    for seed in coverage_seeds() {
        let src = generate(seed, &knobs);
        let pats = patterns(&src);
        deref += usize::from(
            pats.iter()
                .any(|p| p.starts_with('&') && !p.starts_with("&mut ")),
        );
        deref_mut += usize::from(pats.iter().any(|p| p.starts_with("&mut ")));
        open += usize::from(pats.iter().any(|p| is_half_open(p)));
        float += usize::from(pats.iter().any(|p| is_float_pattern(p)));
    }
    coverage(
        "patterns",
        &[
            ("`&` pattern", deref, 15),
            ("`&mut` pattern", deref_mut, 5),
            ("half-open range pattern", open, 34),
            ("float pattern", float, 15),
        ],
    );
}

/// Whether a pattern has a half-open range: `..=hi` or `..hi` where a pattern starts
/// (`(`, `| `, `@ ` or the start before it), or `lo..` before where one ends.
fn is_half_open(p: &str) -> bool {
    let b = p.as_bytes();
    p.match_indices("..").any(|(i, _)| {
        let starts =
            i == 0 || p[..i].ends_with('(') || p[..i].ends_with("| ") || p[..i].ends_with("@ ");
        let next = b.get(i + 2).copied();
        let upper = matches!(next, Some(c) if c == b'=' || c == b'-' || c.is_ascii_alphanumeric());
        let lower_before = i > 0 && b[i - 1].is_ascii_alphanumeric();
        let ends = matches!(next, None | Some(b')' | b',' | b' '));
        (starts && upper) || (lower_before && ends)
    })
}

/// Whether a pattern has a float literal pattern or range: a suffixed float literal that
/// is a bound of a range (`0.5f64..`, `..=1.5f32`) or a whole (sub)pattern (`(_, 2.5f64)`,
/// `x @ 0.5f32`, before a guard).
fn is_float_pattern(p: &str) -> bool {
    let p = p.split(" if ").next().unwrap_or(p);
    let p = p.split(" = ").next().unwrap_or(p);
    ["f32", "f64"].iter().any(|suffix| {
        p.match_indices(suffix).any(|(i, _)| {
            let before = &p[..i];
            let literal =
                before.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-');
            if literal.len() == before.len() {
                return false;
            }
            let after = &p[i + 3..];
            let alone = (literal.is_empty()
                || ["(", ", ", "| ", "@ "].iter().any(|t| literal.ends_with(t)))
                && (after.is_empty() || [")", ",", " |"].iter().any(|t| after.starts_with(t)));
            after.starts_with("..") || literal.ends_with("..") || literal.ends_with("..=") || alone
        })
    })
}
