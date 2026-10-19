//! Run-pass tests: every `tests/run-pass/**/*.sabi` is compiled to an executable with the
//! `sabikoc` binary, verified with `ILVerify`, run with `dotnet`, and its stdout and exit
//! code are compared with expectations. See `tests/run-pass/README.md`.
//!
//! Directives in `//@` comments at the top of a program:
//! - `//@ exit-code: N` — the expected exit code (default 0; panics exit with 101);
//! - `//@ release` — compile with `--release` (wrapping arithmetic).
//!
//! Expected stdout lives in `<name>.stdout` next to the program; `BLESS=1` writes it.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use common::{text, Toolchain};

struct Directives {
    exit_code: i32,
    release: bool,
}

fn directives(src: &str) -> Directives {
    let mut d = Directives {
        exit_code: 0,
        release: false,
    };
    for line in src.lines() {
        let Some(rest) = line.trim().strip_prefix("//@") else {
            continue;
        };
        let rest = rest.trim();
        if let Some(code) = rest.strip_prefix("exit-code:") {
            d.exit_code = code.trim().parse().expect("exit code");
        } else if rest == "release" {
            d.release = true;
        } else {
            panic!("unknown directive `{line}`");
        }
    }
    d
}

fn run_one(tc: &Toolchain, root: &Path, path: &Path) -> Result<(), String> {
    let rel = path.strip_prefix(root).unwrap().display().to_string();
    let src = std::fs::read_to_string(path).unwrap();
    let d = directives(&src);
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("run-pass")
        .join(rel.trim_end_matches(".sabi"));
    std::fs::create_dir_all(&out_dir).unwrap();
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let dll = out_dir.join(format!("{stem}.dll"));
    common::compile_exe(tc, path, &dll, d.release)?;
    common::ilverify(tc, &dll)?;
    let out = common::run_dll(tc, &dll)?;
    let stdout = text(&out.stdout);
    let code = out.status.code().unwrap_or(-1);
    let expected_path = path.with_extension("stdout");
    let bless = std::env::var_os("BLESS").is_some();
    let mut failures = String::new();
    if code != d.exit_code {
        let _ = writeln!(
            failures,
            "exit code {code}, expected {}\nstderr:\n{}",
            d.exit_code,
            text(&out.stderr)
        );
    }
    let expected = std::fs::read_to_string(&expected_path)
        .ok()
        .map(|s| s.replace("\r\n", "\n"));
    match expected {
        Some(e) if e == stdout => {}
        _ if bless && failures.is_empty() => {
            std::fs::write(&expected_path, &stdout).unwrap();
            eprintln!("blessed {}", expected_path.display());
        }
        Some(e) => {
            let _ = writeln!(
                failures,
                "stdout differs from {}\n--- expected\n{e}--- actual\n{stdout}---",
                expected_path.display()
            );
        }
        None => {
            let _ = writeln!(
                failures,
                "missing {} (run with BLESS=1 to create it)\n--- actual stdout\n{stdout}---",
                expected_path.display()
            );
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

#[test]
fn run_pass() {
    let Some(tc) = common::toolchain_or_skip("run-pass") else {
        return;
    };
    let root = common::repo_root().join("tests/run-pass");
    let mut files: Vec<PathBuf> = Vec::new();
    common::collect_sabi(&root, &mut files);
    assert!(!files.is_empty(), "no programs under {}", root.display());
    let results = common::parallel_map(&files, |path| run_one(&tc, &root, path));
    let mut report = String::new();
    let mut failed = 0;
    for (path, result) in files.iter().zip(results) {
        let rel = path.strip_prefix(&root).unwrap().display().to_string();
        match result {
            Ok(()) => eprintln!("run-pass {rel}: ok"),
            Err(e) => {
                failed += 1;
                let _ = writeln!(report, "\n=== {rel}\n{e}");
            }
        }
    }
    assert!(
        failed == 0,
        "{failed} of {} run-pass programs failed:{report}",
        files.len()
    );
}
