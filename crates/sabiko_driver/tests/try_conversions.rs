//! The error conversions of `?` (decision 93) on every pair of a table of error types:
//! `fn F<T>(r: Result<i32, FROM>) -> Result<i32, TO> { Ok(r?) }` for `FROM != TO`. rustc
//! accepts exactly the pairs in [`ACCEPTED`] (an impl `From<FROM> for TO` in its standard
//! library); Sabiko, which has no `From` yet (#117), must report each of them as not yet
//! supported (E0107) and every other pair as rustc's E0277, Sabiko's E0351. When `rustc`
//! can be run, the table itself is checked against it (`SABIKO_REQUIRE_RUSTC=1` makes a
//! missing `rustc` an error), so the split stays exact.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use sabiko_diagnostics::ErrorCode;
use sabiko_driver::{compile_str, Options, Stage};

/// The error types, valid in both languages. `S` is a struct, `E` an enum and `T` the
/// function's type parameter.
const TYPES: &[&str] = &[
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "f32",
    "f64",
    "bool",
    "char",
    "()",
    "&str",
    "&u8",
    "&mut u8",
    "(u8, bool)",
    "(u8, u8)",
    "(u8,)",
    // TODO(#137): `[u8; 2]`, `[u8; 1]` and `Option<[u8; 2]>`, once an array as a generic
    // argument lowers; rustc converts `[T; N]` and `(T, .., T)` both ways, and `T` to
    // `Option<T>` for them (entries to add to `ACCEPTED` then).
    "Option<u8>",
    "Option<S>",
    "S",
    "E",
    "T",
    "Result<u8, u8>",
    "Option<&u8>",
    "&Option<u8>",
    "Option<&mut u8>",
    "&mut Option<u8>",
    "Option<Option<u8>>",
    "Option<(u8, u8)>",
];

/// The pairs rustc accepts (rustc 1.99.1), `FROM -> TO`.
const ACCEPTED: &[(&str, &str)] = &[
    ("&Option<u8>", "Option<&u8>"),
    ("&mut Option<u8>", "Option<&mut u8>"),
    ("&mut u8", "Option<&mut u8>"),
    ("&u8", "Option<&u8>"),
    ("(u8, u8)", "Option<(u8, u8)>"),
    ("Option<u8>", "Option<Option<u8>>"),
    ("S", "Option<S>"),
    ("bool", "f32"),
    ("bool", "f64"),
    ("bool", "i128"),
    ("bool", "i16"),
    ("bool", "i32"),
    ("bool", "i64"),
    ("bool", "i8"),
    ("bool", "isize"),
    ("bool", "u128"),
    ("bool", "u16"),
    ("bool", "u32"),
    ("bool", "u64"),
    ("bool", "u8"),
    ("bool", "usize"),
    ("char", "u128"),
    ("char", "u32"),
    ("char", "u64"),
    ("f32", "f64"),
    ("i16", "f32"),
    ("i16", "f64"),
    ("i16", "i128"),
    ("i16", "i32"),
    ("i16", "i64"),
    ("i16", "isize"),
    ("i32", "f64"),
    ("i32", "i128"),
    ("i32", "i64"),
    ("i64", "i128"),
    ("i8", "f32"),
    ("i8", "f64"),
    ("i8", "i128"),
    ("i8", "i16"),
    ("i8", "i32"),
    ("i8", "i64"),
    ("i8", "isize"),
    ("u16", "f32"),
    ("u16", "f64"),
    ("u16", "i128"),
    ("u16", "i32"),
    ("u16", "i64"),
    ("u16", "u128"),
    ("u16", "u32"),
    ("u16", "u64"),
    ("u16", "usize"),
    ("u32", "f64"),
    ("u32", "i128"),
    ("u32", "i64"),
    ("u32", "u128"),
    ("u32", "u64"),
    ("u64", "i128"),
    ("u64", "u128"),
    ("u8", "Option<u8>"),
    ("u8", "char"),
    ("u8", "f32"),
    ("u8", "f64"),
    ("u8", "i128"),
    ("u8", "i16"),
    ("u8", "i32"),
    ("u8", "i64"),
    ("u8", "isize"),
    ("u8", "u128"),
    ("u8", "u16"),
    ("u8", "u32"),
    ("u8", "u64"),
    ("u8", "usize"),
];

const HEADER: &str = "struct S;\nenum E { A }\n";

/// The program, and the pair of each of its lines (by 1-based line number).
fn program() -> (String, BTreeMap<usize, (&'static str, &'static str)>) {
    let mut src = String::from(HEADER);
    let mut lines = BTreeMap::new();
    let mut line = HEADER.lines().count();
    for &from in TYPES {
        for &to in TYPES {
            if from == to {
                continue;
            }
            // An output reference needs an input one to take its lifetime from.
            let anchor = if to.contains('&') && !from.contains('&') {
                "_a: &u8, "
            } else {
                ""
            };
            line += 1;
            let _ = writeln!(
                src,
                "fn F{line}<T>({anchor}r: Result<i32, {from}>) -> Result<i32, {to}> {{ Ok(r?) }}"
            );
            lines.insert(line, (from, to));
        }
    }
    (src, lines)
}

fn line_of(src: &str, offset: u32) -> usize {
    1 + src[..offset as usize].matches('\n').count()
}

#[test]
fn sabiko_splits_the_pairs_as_rustc_does() {
    let (src, lines) = program();
    let options = Options {
        stop_after: Stage::Typeck,
        ..Options::default()
    };
    let c = compile_str("pairs.sabi", &src, options);
    let mut codes: BTreeMap<usize, Vec<ErrorCode>> = BTreeMap::new();
    for d in &c.diagnostics {
        let primary = d
            .labels
            .iter()
            .find(|l| l.primary)
            .expect("a primary label");
        codes
            .entry(line_of(&src, primary.span.start))
            .or_default()
            .push(d.code);
    }
    let accepted: BTreeSet<(&str, &str)> = ACCEPTED.iter().copied().collect();
    let mut wrong = Vec::new();
    for (line, pair) in &lines {
        let want = if accepted.contains(pair) {
            ErrorCode::UNSUPPORTED_SYNTAX
        } else {
            ErrorCode::TRY_ERROR_CONVERSION
        };
        let got = codes.remove(line).unwrap_or_default();
        if got != [want] {
            wrong.push(format!(
                "{} -> {}: {got:?}, expected {want:?}",
                pair.0, pair.1
            ));
        }
    }
    for (line, got) in codes {
        wrong.push(format!("line {line} (no pair): {got:?}"));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_table_is_rustcs() {
    let rustc = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    let runs = Command::new(&rustc)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !runs {
        let require = std::env::var_os("SABIKO_REQUIRE_RUSTC").is_some_and(|v| v != "0");
        assert!(
            !require,
            "SABIKO_REQUIRE_RUSTC is set but `rustc` cannot be run"
        );
        eprintln!("try_conversions: no rustc, skipping");
        return;
    }
    let (src, lines) = program();
    let dir = std::env::temp_dir().join(format!("sabiko-try-conversions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rs = dir.join("pairs.rs");
    // The same line numbers: the attribute goes on the first line, before the header.
    std::fs::write(&rs, format!("#![allow(warnings)] {src}")).unwrap();
    let output = Command::new(&rustc)
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "--emit=metadata",
        ])
        .args(["--error-format=short", "--crate-name", "pairs", "--out-dir"])
        .arg(&dir)
        .arg(&rs)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut rejected = BTreeSet::new();
    for l in stderr.lines() {
        let mut parts = l.splitn(3, ':');
        if let (Some(_), Some(line)) = (parts.next(), parts.next()) {
            if let Ok(line) = line.parse::<usize>() {
                assert!(l.contains("E0277"), "unexpected rustc error: {l}");
                rejected.insert(line);
            }
        }
    }
    let accepted: BTreeSet<(&str, &str)> = lines
        .iter()
        .filter(|(line, _)| !rejected.contains(line))
        .map(|(_, &pair)| pair)
        .collect();
    let table: BTreeSet<(&str, &str)> = ACCEPTED.iter().copied().collect();
    assert_eq!(
        accepted,
        table,
        "rustc accepts other pairs than `ACCEPTED` lists; rustc's:\n{}",
        accepted
            .iter()
            .map(|(a, b)| format!("    ({a:?}, {b:?}),"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
