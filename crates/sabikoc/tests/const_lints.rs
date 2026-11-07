//! rustc's constant-propagation lints against Sabiko's E0600/E0601 (decision 73) over the
//! hand-written corpus: `tests/differential/lints/*.sabi` and `tests/ui/lints/*.sabi`. Every
//! program is valid Rust; rustc builds it to MIR with `arithmetic_overflow` and
//! `unconditional_panic` denied (`--emit=metadata` does not run them), and both compilers
//! must report the same lints on the same lines with the same labels. (On generated
//! programs, where Sabiko's lowering and rustc's part ways more often, Sabiko may miss one
//! of rustc's: see `generated.rs`.)

mod common;

use std::fmt::Write as _;

use std::path::Path;

/// Sabiko's builtins for rustc and an empty `main`; the lints under test are not allowed.
const PRELUDE: &str = "#![allow(nonstandard_style, unused, dead_code, unreachable_patterns, \
                       irrefutable_let_patterns, unused_comparisons)]
fn Print<T: std::fmt::Display>(x: T) { print!(\"{x}\") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!(\"{x}\") }
fn Panic(msg: &str) -> ! { panic!(\"{msg}\") }
fn main() {}
";

#[test]
fn constant_lints_agree_with_rustc() {
    let Some(rustc) = common::rustc_or_skip("const-lints") else {
        return;
    };
    let root = common::repo_root();
    let mut files = Vec::new();
    common::collect_sabi(&root.join("tests/differential/lints"), &mut files);
    common::collect_sabi(&root.join("tests/ui/lints"), &mut files);
    assert!(files.len() >= 8, "found only {} lint programs", files.len());
    let results = common::parallel_map(&files, |path| check(&rustc, &root, path));
    let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    assert!(
        failures.is_empty(),
        "{} of {} lint programs disagree with rustc:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}

fn check(rustc: &Path, root: &Path, path: &Path) -> Result<(), String> {
    let rel = path.strip_prefix(root).unwrap().display().to_string();
    let src = std::fs::read_to_string(path).unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("const-lints")
        .join(rel.replace(['/', '\\'], "_"));
    std::fs::create_dir_all(&dir).unwrap();
    let rust = common::rustc_const_lints(rustc, PRELUDE, &src, &dir)
        .map_err(|e| format!("=== {rel}\n{e}"))?
        .ok_or_else(|| format!("=== {rel}\nrustc rejects it for another reason"))?;
    let sabi = common::sabiko_const_lints(&rel, &src).map_err(|e| format!("=== {rel}\n{e}"))?;
    if rust == sabi {
        return Ok(());
    }
    let show = |set: &common::LintSet| {
        let mut out = String::new();
        for (line, m, l) in set {
            let _ = writeln!(out, "  line {line}: {m}: {l}");
        }
        out
    };
    Err(format!(
        "=== {rel}\n--- rustc only\n{}--- Sabiko only\n{}",
        show(&rust.difference(&sabi).cloned().collect()),
        show(&sabi.difference(&rust).cloned().collect())
    ))
}
