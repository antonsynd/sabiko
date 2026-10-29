//! Differential run-pass tests: every `tests/differential/run/**/*.sabi` is built as an
//! executable by both `rustc` and `sabikoc`, and the two programs must print the same
//! stdout and exit with the same code (Rust panics and Sabiko panics both exit with 101).
//! Sabiko programs are compiled in debug mode, like rustc's default, so overflow panics
//! must agree too: when both exit with 101 the panic message lines must match (Sabiko's
//! generic "attempt to perform arithmetic with overflow" is accepted for rustc's
//! per-operator `+`/`-`/`*`/negation messages, since the CLR's `.ovf` opcodes do not say
//! which operator overflowed), and after a clean exit neither may have written to stderr.
//! A `//@ release` line builds both in release mode instead (`sabikoc --release`, rustc
//! with `-C overflow-checks=off`): wrapping arithmetic, with division and shift checks
//! intact. See `tests/differential/README.md`.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use common::Toolchain;

/// Prepended to every program before handing it to `rustc`: the builtins and an entry
/// point calling `Main`.
const RUSTC_PRELUDE: &str = "#![allow(nonstandard_style, unused, dead_code)]
fn Print<T: std::fmt::Display>(x: T) { print!(\"{x}\") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!(\"{x}\") }
fn Panic(msg: &str) -> ! { panic!(\"{msg}\") }
fn main() { Main() }
";

/// Whether the program asks for a release build (`//@ release`); any other `//@` directive
/// is an error.
fn release_directive(src: &str) -> bool {
    let mut release = false;
    for line in src.lines() {
        if let Some(rest) = line.trim().strip_prefix("//@") {
            assert_eq!(rest.trim(), "release", "unknown directive `{line}`");
            release = true;
        }
    }
    release
}

fn run_one(tc: &Toolchain, rustc: &Path, root: &Path, path: &Path) -> Result<String, String> {
    let rel = path.strip_prefix(root).unwrap().display().to_string();
    let src = std::fs::read_to_string(path).unwrap();
    let release = release_directive(&src);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("differential-run")
        .join(rel.trim_end_matches(".sabi"));
    std::fs::create_dir_all(&dir).unwrap();
    let rust = common::run_rust_with(rustc, RUSTC_PRELUDE, &src, &dir, release)?;
    let sabi = common::run_sabiko_with(tc, path, &dir, release)?;
    let summary = common::compare_runs(&rust, &sabi)?;
    Ok(if release {
        format!("{summary}, release")
    } else {
        summary
    })
}

#[test]
fn differential_run() {
    let Some(tc) = common::toolchain_or_skip("differential-run") else {
        return;
    };
    let Some(rustc) = common::rustc_or_skip("differential-run") else {
        return;
    };
    let root = common::repo_root().join("tests/differential/run");
    let mut files: Vec<PathBuf> = Vec::new();
    common::collect_sabi(&root, &mut files);
    assert!(!files.is_empty(), "no programs under {}", root.display());
    let results = common::parallel_map(&files, |path| run_one(&tc, &rustc, &root, path));
    let mut report = String::new();
    let mut failed = 0;
    for (path, result) in files.iter().zip(results) {
        let rel = path.strip_prefix(&root).unwrap().display().to_string();
        match result {
            Ok(summary) => eprintln!("differential-run {rel}: agree ({summary})"),
            Err(e) => {
                failed += 1;
                let _ = writeln!(report, "\n=== {rel}\n{e}");
            }
        }
    }
    assert!(
        failed == 0,
        "{failed} of {} programs disagree:{report}",
        files.len()
    );
}
