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
    "[u8; 2]",
    "[u8; 1]",
    "Option<[u8; 2]>",
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
    ("(u8, u8)", "[u8; 2]"),
    ("(u8,)", "[u8; 1]"),
    ("[u8; 1]", "(u8,)"),
    ("[u8; 2]", "(u8, u8)"),
    ("[u8; 2]", "Option<[u8; 2]>"),
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
    let rejected = rejected_lines(&stderr);
    // An environment problem (no output directory, a crash) must fail the test, never
    // read as "rustc accepts everything".
    assert_eq!(
        output.status.success(),
        rejected.is_empty(),
        "rustc's exit status disagrees with its diagnostics:\n{stderr}"
    );
    assert!(
        rejected.len() < lines.len() && rejected.keys().all(|l| lines.contains_key(l)),
        "rustc's errors are not one per pair:\n{stderr}"
    );
    let accepted: BTreeSet<(&str, &str)> = lines
        .iter()
        .filter(|(line, _)| !rejected.contains_key(line))
        .map(|(_, &pair)| pair)
        .collect();
    assert!(
        !accepted.is_empty() && !rejected.is_empty(),
        "rustc accepted {} and rejected {} pairs: not a real run:\n{stderr}",
        accepted.len(),
        rejected.len()
    );
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

/// The lines rustc reported an error on (`--error-format=short`: `PATH:LINE:COL: error[CODE]:
/// ..`), each of which must be E0277. The path is whatever the platform makes it (a
/// drive letter's `:` included), so the line and column are read from the end of the
/// part before `: error`. Every error rustc reports must be such a line, and their
/// number must be the one rustc's summary gives.
fn rejected_lines(stderr: &str) -> BTreeMap<usize, String> {
    let mut rejected = BTreeMap::new();
    let mut count = 0;
    let mut summary = None;
    for l in stderr.lines().map(str::trim_end) {
        if let Some(rest) = l.strip_prefix("error: aborting due to ") {
            let n = rest.split_whitespace().next().unwrap_or("");
            summary = Some(if n == "previous" {
                1
            } else {
                n.parse::<usize>().unwrap()
            });
            continue;
        }
        let Some(at) = l.find(": error") else {
            assert!(!l.starts_with("error"), "unexpected rustc output: {l}");
            continue;
        };
        let mut place = l[..at].rsplitn(3, ':');
        let (_col, line) = (place.next(), place.next());
        let line: usize = line
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("an error without a line: {l}"));
        assert!(
            l[at..].starts_with(": error[E0277]"),
            "unexpected rustc error: {l}"
        );
        count += 1;
        rejected.insert(line, l.to_owned());
    }
    assert_eq!(
        summary.unwrap_or(0),
        count,
        "rustc's summary disagrees with the errors read:\n{stderr}"
    );
    rejected
}

#[test]
fn rustc_errors_are_read_on_every_platform() {
    let unix = "/tmp/x/pairs.rs:7:52: error[E0277]: `?` couldn't convert the error to `u8`\n\
                error: aborting due to 1 previous error\n";
    let windows = "C:\\Users\\x\\AppData\\Local\\Temp\\pairs.rs:7:52: error[E0277]: `?` couldn't convert the error to `u8`\r\n\
                   error: aborting due to 1 previous error\r\n\r\n\
                   For more information about this error, try `rustc --explain E0277`.\r\n";
    for stderr in [unix, windows] {
        let lines: Vec<usize> = rejected_lines(stderr).into_keys().collect();
        assert_eq!(lines, [7]);
    }
    assert!(rejected_lines("").is_empty());
}
