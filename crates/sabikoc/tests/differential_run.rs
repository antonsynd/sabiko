//! Differential run-pass tests: every `tests/differential/run/**/*.sabi` is built as an
//! executable by both `rustc` and `sabikoc`, and the two programs must print the same
//! stdout and exit with the same code (Rust panics and Sabiko panics both exit with 101).
//! Sabiko programs are compiled in debug mode, like rustc's default, so overflow panics
//! must agree too. See `tests/differential/README.md`.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{text, Toolchain};

/// Prepended to every program before handing it to `rustc`: the builtins and an entry
/// point calling `Main`.
const RUSTC_PRELUDE: &str = "#![allow(nonstandard_style, unused, dead_code)]
fn Print<T: std::fmt::Display>(x: T) { print!(\"{x}\") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!(\"{x}\") }
fn Panic(msg: &str) -> ! { panic!(\"{msg}\") }
fn main() { Main() }
";

fn rustc() -> Option<PathBuf> {
    let candidate = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| candidate)
}

struct Run {
    stdout: String,
    code: i32,
    stderr: String,
}

fn run_rust(rustc: &Path, src: &str, dir: &Path) -> Result<Run, String> {
    let rs = dir.join("prog.rs");
    std::fs::write(&rs, format!("{RUSTC_PRELUDE}{src}")).unwrap();
    let exe = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    let out = Command::new(rustc)
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "bin",
            "-A",
            "warnings",
            "-o",
        ])
        .arg(&exe)
        .arg(&rs)
        .output()
        .map_err(|e| format!("cannot run rustc: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rustc rejected the program:\n{}",
            text(&out.stderr)
        ));
    }
    let out = Command::new(&exe)
        .output()
        .map_err(|e| format!("cannot run `{}`: {e}", exe.display()))?;
    Ok(Run {
        stdout: text(&out.stdout),
        code: out.status.code().unwrap_or(-1),
        stderr: text(&out.stderr),
    })
}

fn run_sabiko(tc: &Toolchain, path: &Path, dir: &Path) -> Result<Run, String> {
    let dll = dir.join("prog.dll");
    common::compile_exe(tc, path, &dll, false)?;
    common::ilverify(tc, &dll)?;
    let out = common::run_dll(tc, &dll)?;
    Ok(Run {
        stdout: text(&out.stdout),
        code: out.status.code().unwrap_or(-1),
        stderr: text(&out.stderr),
    })
}

fn run_one(tc: &Toolchain, rustc: &Path, root: &Path, path: &Path) -> Result<String, String> {
    let rel = path.strip_prefix(root).unwrap().display().to_string();
    let src = std::fs::read_to_string(path).unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("differential-run")
        .join(rel.trim_end_matches(".sabi"));
    std::fs::create_dir_all(&dir).unwrap();
    let rust = run_rust(rustc, &src, &dir)?;
    let sabi = run_sabiko(tc, path, &dir)?;
    let mut failures = String::new();
    if rust.code != sabi.code {
        let _ = writeln!(
            failures,
            "exit codes differ: rustc {} vs sabiko {}\nrust stderr:\n{}sabiko stderr:\n{}",
            rust.code, sabi.code, rust.stderr, sabi.stderr
        );
    }
    if rust.stdout != sabi.stdout {
        let _ = writeln!(
            failures,
            "stdout differs\n--- rustc\n{}--- sabiko\n{}---",
            rust.stdout, sabi.stdout
        );
    }
    if failures.is_empty() {
        Ok(format!(
            "exit {}, {} bytes of stdout",
            rust.code,
            rust.stdout.len()
        ))
    } else {
        Err(failures)
    }
}

#[test]
fn differential_run() {
    let Some(tc) = common::toolchain_or_skip("differential-run") else {
        return;
    };
    let Some(rustc) = rustc() else {
        let require = std::env::var_os("SABIKO_REQUIRE_RUSTC").is_some_and(|v| v != "0");
        assert!(!require, "rustc not found and SABIKO_REQUIRE_RUSTC is set");
        eprintln!("differential-run: rustc not found; skipping");
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
