//! Differential tests against `rustc` over `tests/differential/**/*.sabi`. Every program
//! in the corpus is valid Rust syntax as well as Sabiko; each is compiled by both compilers
//! and the verdicts must agree: accepted by both, or rejected by both with borrow-checker
//! errors only. See `tests/differential/README.md`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use sabiko_diagnostics::Severity;
use sabiko_driver::{compile_str, Options};

/// Prepended to every program before handing it to `rustc`: Sabiko uses .NET casing, the
/// corpus consists of library functions nothing calls, and Sabiko's builtin functions
/// (`Print`, `PrintLine`, `Panic`) must exist for rustc too. Five lines, so rustc line
/// numbers are shifted by five (the harness never compares positions). A corpus program
/// may not define its own `Print`/`PrintLine`/`Panic`: Sabiko would shadow the builtin
/// silently while rustc reports a duplicate definition.
const RUSTC_PRELUDE: &str = "#![allow(nonstandard_style, unused, dead_code)]\n\
    fn Print<T: std::fmt::Display>(x: T) { print!(\"{x}\") }\n\
    fn PrintLine<T: std::fmt::Display>(x: T) { println!(\"{x}\") }\n\
    fn Panic(msg: &str) -> ! { panic!(\"{msg}\") }\n\
    \n";

/// rustc error codes that belong to borrow checking (moves, loans, mutability,
/// initialisation, lifetimes and elision). Anything else means the program is not the
/// valid Rust the corpus promises.
const RUSTC_BORROW_CODES: &[&str] = &[
    "E0106", "E0204", "E0373", "E0381", "E0382", "E0384", "E0499", "E0502", "E0503", "E0505",
    "E0506", "E0507", "E0509", "E0515", "E0521", "E0594", "E0596", "E0597", "E0623", "E0716",
];

/// rustc errors without a code that are still borrow-checker verdicts.
const RUSTC_UNCODED_BORROW_ERRORS: &[&str] = &["lifetime may not live long enough"];

/// Sabiko's borrow-checker code range (E0400..=E0418).
const SABIKO_BORROW_CODES: std::ops::RangeInclusive<u16> = 400..=418;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// No errors.
    Accept,
    /// Errors, all of them from the borrow checker.
    BorrowError,
    /// At least one error from elsewhere (parser, resolver, type checker, or an
    /// uncoded/unknown rustc error): the corpus program itself is wrong.
    OtherError,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Accept => "accept",
            Verdict::BorrowError => "borrow-error",
            Verdict::OtherError => "OTHER-ERROR",
        })
    }
}

#[derive(Debug)]
struct Outcome {
    verdict: Verdict,
    /// The error codes (or uncoded messages) that led to the verdict, deduplicated.
    codes: Vec<String>,
    /// Full diagnostics, shown only when the verdict is `OtherError`.
    detail: String,
}

impl Outcome {
    fn from_codes(codes: Vec<String>, is_borrow: impl Fn(&str) -> bool, detail: String) -> Self {
        let verdict = if codes.is_empty() {
            Verdict::Accept
        } else if codes.iter().all(|c| is_borrow(c)) {
            Verdict::BorrowError
        } else {
            Verdict::OtherError
        };
        Outcome {
            verdict,
            codes,
            detail,
        }
    }
}

fn push_unique(codes: &mut Vec<String>, code: String) {
    if !codes.contains(&code) {
        codes.push(code);
    }
}

fn run_sabiko(rel: &str, src: &str) -> Outcome {
    let compilation = compile_str(
        &format!("tests/differential/{rel}"),
        src,
        Options::default(),
    );
    let mut codes = Vec::new();
    for d in &compilation.diagnostics {
        if d.severity == Severity::Error {
            push_unique(&mut codes, d.code.to_string());
        }
    }
    let detail = compilation.render_diagnostics(sabiko_diagnostics::Format::HumanPlain);
    Outcome::from_codes(
        codes,
        |c| {
            c.strip_prefix('E')
                .and_then(|n| n.parse::<u16>().ok())
                .is_some_and(|n| SABIKO_BORROW_CODES.contains(&n))
        },
        detail,
    )
}

/// Classifies rustc's stderr. Returns the codes found (an uncoded error line is recorded as
/// its message); `success` is the exit status.
fn classify_rustc(success: bool, stderr: &str) -> Outcome {
    let mut codes = Vec::new();
    for line in stderr.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("error[") {
            if let Some(end) = rest.find(']') {
                push_unique(&mut codes, rest[..end].to_owned());
            }
        } else if let Some(msg) = line.strip_prefix("error: ") {
            if msg.starts_with("aborting due to") {
                continue;
            }
            push_unique(&mut codes, msg.to_owned());
        }
    }
    if !success && codes.is_empty() {
        // rustc failed to run at all, or failed without a parsable error line.
        codes.push("rustc exited with failure and no error lines".to_owned());
    }
    if success {
        codes.clear();
    }
    Outcome::from_codes(
        codes,
        |c| RUSTC_BORROW_CODES.contains(&c) || RUSTC_UNCODED_BORROW_ERRORS.contains(&c),
        stderr.to_owned(),
    )
}

fn run_rustc(rustc: &Path, tmp: &Path, rel: &str, src: &str) -> Outcome {
    // One crate per program; the crate name must be an identifier.
    let crate_name: String = rel
        .trim_end_matches(".sabi")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let crate_name = format!("d_{crate_name}");
    let rs = tmp.join(format!("{crate_name}.rs"));
    std::fs::write(&rs, format!("{RUSTC_PRELUDE}{src}")).unwrap();
    let output = Command::new(rustc)
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--emit=metadata",
            "-A",
            "warnings",
        ])
        .arg("--crate-name")
        .arg(&crate_name)
        .arg("--out-dir")
        .arg(tmp)
        .arg(&rs)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", rustc.display()));
    classify_rustc(
        output.status.success(),
        &String::from_utf8_lossy(&output.stderr),
    )
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/differential")
        .canonicalize()
        .unwrap()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            // `run/` holds the run-time differential programs (`sabikoc`'s
            // `differential_run` test); they are compared by output, not by verdict.
            if path.file_name().is_some_and(|n| n == "run") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
}

/// The `rustc` to use: `$RUSTC`, else `rustc` on `PATH`. `None` when it cannot be run.
fn find_rustc() -> Option<PathBuf> {
    let rustc = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    let version = Command::new(&rustc).arg("--version").output().ok()?;
    if !version.status.success() {
        return None;
    }
    eprintln!(
        "differential: using {} ({})",
        rustc.display(),
        String::from_utf8_lossy(&version.stdout).trim()
    );
    Some(rustc)
}

/// Removes the scratch directory when dropped, so a panic mid-test does not leak it.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "sabiko-differential-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Case {
    rel: String,
    src: String,
}

struct Result_ {
    rel: String,
    sabiko: Outcome,
    rustc: Outcome,
}

/// Runs rustc over all cases on a small thread pool and Sabiko on the current thread.
fn run_all(rustc: &Path, tmp: &Path, cases: &[Case]) -> Vec<Result_> {
    let next = Mutex::new(0usize);
    let rustc_outcomes: Mutex<Vec<Option<Outcome>>> =
        Mutex::new(cases.iter().map(|_| None).collect());
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(1, 8);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = {
                    let mut n = next.lock().unwrap();
                    let i = *n;
                    *n += 1;
                    i
                };
                let Some(case) = cases.get(i) else { break };
                let outcome = run_rustc(rustc, tmp, &case.rel, &case.src);
                rustc_outcomes.lock().unwrap()[i] = Some(outcome);
            });
        }
    });
    let rustc_outcomes = rustc_outcomes.into_inner().unwrap();
    cases
        .iter()
        .zip(rustc_outcomes)
        .map(|(case, rustc)| Result_ {
            rel: case.rel.clone(),
            sabiko: run_sabiko(&case.rel, &case.src),
            rustc: rustc.expect("every case gets a rustc outcome"),
        })
        .collect()
}

/// Returns a failure description for one result, if any.
fn check(r: &Result_) -> Option<String> {
    let mut failures = String::new();
    let file_name = r.rel.rsplit('/').next().unwrap_or(&r.rel);
    let expected = if file_name.starts_with("ok_") {
        Some(Verdict::Accept)
    } else if file_name.starts_with("err_") {
        Some(Verdict::BorrowError)
    } else {
        let _ = writeln!(failures, "  file name must start with `ok_` or `err_`");
        None
    };
    for (who, outcome) in [("sabiko", &r.sabiko), ("rustc", &r.rustc)] {
        if outcome.verdict == Verdict::OtherError {
            let _ = writeln!(
                failures,
                "  {who} reports non-borrow errors {:?}; the program must be valid in both \
                 languages up to borrow checking:\n{}",
                outcome.codes,
                indent(&outcome.detail)
            );
        }
    }
    if r.sabiko.verdict != r.rustc.verdict {
        let _ = writeln!(
            failures,
            "  verdicts disagree: sabiko {} {:?}, rustc {} {:?}",
            r.sabiko.verdict, r.sabiko.codes, r.rustc.verdict, r.rustc.codes
        );
    }
    if let Some(expected) = expected {
        for (who, outcome) in [("sabiko", &r.sabiko), ("rustc", &r.rustc)] {
            if outcome.verdict != expected && outcome.verdict != Verdict::OtherError {
                let _ = writeln!(
                    failures,
                    "  file name promises {expected} but {who} says {} {:?}",
                    outcome.verdict, outcome.codes
                );
            }
        }
    }
    if failures.is_empty() {
        None
    } else {
        Some(format!("{}:\n{failures}", r.rel))
    }
}

fn indent(s: &str) -> String {
    let mut out = String::new();
    for l in s.lines() {
        let _ = writeln!(out, "      {l}");
    }
    out
}

#[test]
fn differential() {
    let Some(rustc) = find_rustc() else {
        eprintln!("differential: SKIPPED, `rustc` is not runnable (set RUSTC or put it on PATH)");
        return;
    };
    let root = corpus_root();
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no differential tests found under {}",
        root.display()
    );
    let cases: Vec<Case> = files
        .iter()
        .map(|path| Case {
            rel: path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            src: std::fs::read_to_string(path).unwrap(),
        })
        .collect();

    let tmp = TempDir::new();
    let results = run_all(&rustc, &tmp.0, &cases);
    drop(tmp);

    let mut failures = Vec::new();
    for r in &results {
        let mark = if r.sabiko.verdict == r.rustc.verdict {
            "ok  "
        } else {
            "DIFF"
        };
        eprintln!(
            "{mark} {:<50} sabiko {} {:?}  rustc {} {:?}",
            r.rel, r.sabiko.verdict, r.sabiko.codes, r.rustc.verdict, r.rustc.codes
        );
        failures.extend(check(r));
    }
    assert!(
        failures.is_empty(),
        "{} of {} differential test(s) failed:\n\n{}",
        failures.len(),
        results.len(),
        failures.join("\n")
    );
    eprintln!("{} differential tests agree with rustc", results.len());
}
