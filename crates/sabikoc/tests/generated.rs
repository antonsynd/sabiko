//! Properties over programs from `sabiko_testgen`, the seeded random generator for the
//! shared Sabiko/Rust subset (see `tests/differential/README.md`):
//!
//! 1. compile-time verdicts agree with rustc (accept, or borrow errors on both sides; any
//!    other error on either side fails);
//! 2. borrow-clean programs run identically under both compilers (stdout, exit code,
//!    panic message);
//! 3. the Sabiko pipeline is deterministic (two compilations give the same diagnostics,
//!    MIR and IL) and idempotent under no-op statement insertion (same error codes and
//!    verdict).
//!
//! `SABIKO_GENERATED_SEEDS` sets how many seeds each property runs (defaults keep the whole
//! file under about two minutes; the nightly workflow uses thousands) and
//! `SABIKO_GENERATED_BASE_SEED` the first seed. A failure prints the seed and the program,
//! so `generate(seed, &knobs)` reproduces it.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use common::Verdict;
use sabiko_diagnostics::{Format, Severity};
use sabiko_driver::{compile_str, Emit, Options};
use sabiko_testgen::{generate, insert_noops, Knobs};

fn scratch(kind: &str, seed: u64) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("generated")
        .join(format!("{kind}-{seed}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sabiko's verdict on `src`, with the error codes and the rendered diagnostics.
fn sabiko_verdict(seed: u64, src: &str) -> (Verdict, Vec<String>, String) {
    let c = compile_str(&format!("generated-{seed}.sabi"), src, Options::default());
    let mut codes: Vec<String> = Vec::new();
    for d in &c.diagnostics {
        if d.severity == Severity::Error && !codes.contains(&d.code.to_string()) {
            codes.push(d.code.to_string());
        }
    }
    let verdict = common::verdict_of(&codes, common::sabiko_code_is_borrow);
    (verdict, codes, c.render_diagnostics(Format::HumanPlain))
}

/// Runs `job` over the seeds, prints one line per seed and fails with every seed's report.
fn check_all(test: &str, seeds: &[u64], job: impl Fn(u64) -> Result<String, String> + Sync) {
    let results = common::parallel_map(seeds, |&seed| job(seed));
    let mut report = String::new();
    let mut failed = 0;
    for (seed, result) in seeds.iter().zip(results) {
        match result {
            Ok(summary) => eprintln!("{test} seed {seed}: {summary}"),
            Err(e) => {
                failed += 1;
                let _ = writeln!(report, "\n=== seed {seed}\n{e}");
            }
        }
    }
    assert!(
        failed == 0,
        "{test}: {failed} of {} seeds fail:{report}",
        seeds.len()
    );
}

#[test]
fn generated_verdicts_agree_with_rustc() {
    let Some(rustc) = common::rustc_or_skip("generated-verdict") else {
        return;
    };
    let seeds = common::generated_seeds(60);
    let knobs = Knobs::default();
    let prelude = common::RUSTC_GENERATED_PRELUDE;
    check_all("generated-verdict", &seeds, |seed| {
        let src = generate(seed, &knobs);
        let dir = scratch("verdict", seed);
        let (sv, scodes, sdetail) = sabiko_verdict(seed, &src);
        let (rv, rcodes, rdetail) = common::rustc_verdict(&rustc, prelude, &src, &dir);
        let mut failures = String::new();
        if sv == Verdict::OtherError {
            let _ = writeln!(
                failures,
                "sabiko reports non-borrow errors {scodes:?}:\n{sdetail}"
            );
        }
        if rv == Verdict::OtherError {
            let _ = writeln!(
                failures,
                "rustc reports non-borrow errors {rcodes:?}:\n{rdetail}"
            );
        }
        if sv != rv {
            let _ = writeln!(
                failures,
                "verdicts disagree: sabiko {sv} {scodes:?}, rustc {rv} {rcodes:?}\n--- sabiko\n\
                 {sdetail}--- rustc\n{rdetail}"
            );
        }
        if failures.is_empty() {
            let _ = std::fs::remove_dir_all(&dir);
            Ok(format!("{sv} {scodes:?} / rustc {rcodes:?}"))
        } else {
            let _ = writeln!(failures, "--- program ({})\n{src}", dir.display());
            Err(failures)
        }
    });
}

#[test]
fn generated_programs_run_identically() {
    let Some(tc) = common::toolchain_or_skip("generated-run") else {
        return;
    };
    let Some(rustc) = common::rustc_or_skip("generated-run") else {
        return;
    };
    let seeds = common::generated_seeds(40);
    let knobs = Knobs::borrow_clean();
    check_all("generated-run", &seeds, |seed| {
        let src = generate(seed, &knobs);
        let dir = scratch("run", seed);
        let path = dir.join("prog.sabi");
        std::fs::write(&path, &src).unwrap();
        let result = (|| {
            let rust = common::run_rust(&rustc, common::RUSTC_GENERATED_PRELUDE, &src, &dir)?;
            let sabi = common::run_sabiko(&tc, &path, &dir)?;
            common::compare_runs(&rust, &sabi)
        })();
        match result {
            Ok(summary) => {
                let _ = std::fs::remove_dir_all(&dir);
                Ok(summary)
            }
            Err(e) => Err(format!("{e}\n--- program ({})\n{src}", dir.display())),
        }
    });
}

fn compile_emit(seed: u64, src: &str, emit: Emit) -> (String, String) {
    let c = compile_str(
        &format!("generated-{seed}.sabi"),
        src,
        Options {
            emit,
            ..Options::default()
        },
    );
    (c.render_diagnostics(Format::HumanPlain), c.output)
}

#[test]
fn generated_pipeline_is_deterministic_and_idempotent() {
    let seeds = common::generated_seeds(60);
    let knobs = Knobs::default();
    check_all("generated-determinism", &seeds, |seed| {
        let src = generate(seed, &knobs);
        let mut failures = String::new();
        for emit in [Emit::Mir, Emit::Il] {
            let first = compile_emit(seed, &src, emit);
            let second = compile_emit(seed, &src, emit);
            if first.0 != second.0 {
                let _ = writeln!(
                    failures,
                    "diagnostics differ between two compilations ({emit:?}):\n--- first\n{}\
                     --- second\n{}",
                    first.0, second.0
                );
            }
            if first.1 != second.1 {
                let _ = writeln!(
                    failures,
                    "{emit:?} output differs between two compilations:\n--- first\n{}--- \
                     second\n{}",
                    first.1, second.1
                );
            }
        }
        let (verdict, codes, detail) = sabiko_verdict(seed, &src);
        let with_noops = insert_noops(&src, seed);
        let (nverdict, ncodes, ndetail) = sabiko_verdict(seed, &with_noops);
        let mut sorted = codes.clone();
        sorted.sort();
        let mut nsorted = ncodes.clone();
        nsorted.sort();
        if verdict != nverdict || sorted != nsorted {
            let _ = writeln!(
                failures,
                "no-op statements change the outcome: {verdict} {codes:?} before, {nverdict} \
                 {ncodes:?} after\n--- before\n{detail}--- after\n{ndetail}--- program with \
                 no-ops\n{with_noops}"
            );
        }
        if failures.is_empty() {
            Ok(format!("{verdict} {codes:?}, stable under no-ops"))
        } else {
            let _ = writeln!(failures, "--- program\n{src}");
            Err(failures)
        }
    });
}
