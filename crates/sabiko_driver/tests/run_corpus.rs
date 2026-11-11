//! Every run-time program (`tests/differential/run/**/*.sabi`, `tests/run-pass/**/*.sabi`)
//! compiles to IL without errors, in process, honouring `//@ release` and `//@ allow:`.
//!
//! `sabikoc`'s `differential_run` and `run_pass` tests build and run these programs with
//! the .NET toolchain and compare their output, but mutation testing of the front end does
//! not run `sabikoc`'s suites: too slow per mutant (`scripts/mutants.sh`). This test needs
//! neither rustc nor .NET and takes seconds, so a `sabiko_types` or `sabiko_mir` mutant that
//! rejects a valid program (a trait impl no longer selected, an inference no longer made)
//! is caught here (#125).

use std::path::{Path, PathBuf};

use sabiko_driver::{compile_str, AllowedLints, Emit, Lint, Options, OutputKind};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
}

/// The options `sabikoc` gets for this program from the run harnesses' directives:
/// `//@ release` (no overflow checks) and `//@ allow: LINT`. `//@ exit-code` only concerns
/// running it.
fn options(src: &str) -> Options {
    let mut options = Options {
        emit: Emit::Il,
        output_kind: OutputKind::Executable,
        ..Options::default()
    };
    let mut allowed = AllowedLints::default();
    for line in src.lines() {
        let Some(rest) = line.trim().strip_prefix("//@") else {
            continue;
        };
        let rest = rest.trim();
        if rest == "release" {
            options.overflow_checks = false;
        } else if let Some(lint) = rest.strip_prefix("allow:") {
            let lint = lint.trim();
            allowed
                .insert(Lint::from_name(lint).unwrap_or_else(|| panic!("unknown lint `{lint}`")));
        }
    }
    options.allowed_lints = allowed;
    options
}

#[test]
fn run_time_programs_compile() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    collect(&root.join("tests/differential/run"), &mut files);
    collect(&root.join("tests/run-pass"), &mut files);
    files.sort();
    assert!(files.len() > 400, "found only {} programs", files.len());
    let mut failures = Vec::new();
    for path in &files {
        let rel = path.strip_prefix(&root).unwrap().display().to_string();
        let src = std::fs::read_to_string(path).unwrap();
        let compilation = compile_str(&rel, &src, options(&src));
        let stray = compilation.diagnostics_only_in_prelude();
        assert!(
            stray.is_empty(),
            "{rel}: diagnostics pointing only into the prelude: {stray:?}"
        );
        if compilation.has_errors() || compilation.output.is_empty() {
            failures.push(format!(
                "{rel}:\n{}",
                compilation.render_diagnostics(sabiko_diagnostics::Format::HumanPlain)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} run-time programs no longer compile:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}
