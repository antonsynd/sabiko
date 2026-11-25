//! Differential tests against `rustc` over `tests/differential/**/*.sabi`. Every program
//! in the corpus is valid Rust syntax as well as Sabiko; each is compiled by both compilers
//! and the verdicts must agree: accepted by both, or rejected by both with borrow-checker
//! errors only, or, for a `rej_` file, rejected by both with other errors. See
//! `tests/differential/README.md`.

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    "E0506", "E0507", "E0508", "E0509", "E0510", "E0515", "E0521", "E0594", "E0596", "E0597",
    "E0623", "E0716",
];

/// rustc errors without a code that are still borrow-checker verdicts.
const RUSTC_UNCODED_BORROW_ERRORS: &[&str] = &["lifetime may not live long enough"];

/// Sabiko's borrow-checker code range (E0400..=E0420).
const SABIKO_BORROW_CODES: std::ops::RangeInclusive<u16> = 400..=420;

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
    let stray = compilation.diagnostics_only_in_prelude();
    assert!(
        stray.is_empty(),
        "{rel}: diagnostics pointing only into the prelude: {stray:?}"
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

/// The rustc that judges the corpus: its path and `rustc -vV`, which names its exact
/// commit and so keys [`RustcCache`].
struct Rustc {
    path: PathBuf,
    version: String,
}

/// The arguments rustc gets for every program (with `--crate-name`, `--out-dir` and the
/// file added).
const RUSTC_ARGS: &[&str] = &[
    "--edition",
    "2024",
    "--crate-type",
    "lib",
    "--emit=metadata",
    "-A",
    "warnings",
];

fn run_rustc(rustc: &Rustc, cache: &RustcCache, tmp: &Path, rel: &str, src: &str) -> Outcome {
    // One crate per program; the crate name must be an identifier.
    let crate_name: String = rel
        .trim_end_matches(".sabi")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let crate_name = format!("d_{crate_name}");
    let program = format!("{RUSTC_PRELUDE}{src}");
    let key = format!(
        "{}\n{}\n{}\n--crate-name {crate_name}\n{program}",
        rustc.path.display(),
        rustc.version,
        RUSTC_ARGS.join(" ")
    );
    let (success, stderr) = cache.get(&key).unwrap_or_else(|| {
        let rs = tmp.join(format!("{crate_name}.rs"));
        std::fs::write(&rs, &program).unwrap();
        let output = Command::new(&rustc.path)
            .args(RUSTC_ARGS)
            .arg("--crate-name")
            .arg(&crate_name)
            .arg("--out-dir")
            .arg(tmp)
            .arg(&rs)
            .output()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", rustc.path.display()));
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        cache.put(&key, output.status.success(), &stderr);
        (output.status.success(), stderr)
    });
    classify_rustc(success, &stderr)
}

/// rustc's exit status and stderr for each program, kept under the target directory
/// (`CARGO_TARGET_TMPDIR`) so that a rerun with the same rustc only compiles the programs
/// that changed. The suite reruns once per `sabiko_types` and `sabiko_borrowck` mutant,
/// where rustc's 800 compilations took half its 20 s (#165). An entry is keyed by
/// everything rustc's answer depends on: the rustc binary's path and `rustc -vV` (its
/// exact commit), the arguments, the crate name and the program text, all stored in the
/// entry and compared in full on a hit, so a hash collision or a stale entry is a miss,
/// never a wrong verdict. A rustc wrapper that changes rustc's behaviour behind the same
/// path and `-vV` should run with the cache off: `SABIKO_DIFFERENTIAL_RUSTC_CACHE=0`.
/// The entries live in a directory per rustc (path and version), and [`RustcCache::prune`]
/// removes the other rustcs' directories and the entries this run did not use, so the
/// cache holds one entry per corpus program. The cached stderr names the scratch
/// directory of the run that filled it, which only diagnostics shown on a failure print.
struct RustcCache {
    dir: Option<PathBuf>,
    /// The entry files this run read or wrote, kept by [`RustcCache::prune`].
    used: Mutex<BTreeSet<PathBuf>>,
    /// Numbers the temporary files of [`RustcCache::put`], unique across threads.
    writes: AtomicUsize,
}

fn hash_hex(text: &str) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

impl RustcCache {
    fn new(rustc: &Rustc) -> Self {
        let off = std::env::var_os("SABIKO_DIFFERENTIAL_RUSTC_CACHE").is_some_and(|v| v == "0");
        let dir = (!off)
            .then(|| {
                let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
                    .join("differential-rustc")
                    .join(hash_hex(&format!(
                        "{}\n{}",
                        rustc.path.display(),
                        rustc.version
                    )));
                std::fs::create_dir_all(&dir).is_ok().then_some(dir)
            })
            .flatten();
        RustcCache {
            dir,
            used: Mutex::new(BTreeSet::new()),
            writes: AtomicUsize::new(0),
        }
    }

    fn file(&self, key: &str) -> Option<PathBuf> {
        let file = self.dir.as_ref()?.join(hash_hex(key));
        self.used.lock().unwrap().insert(file.clone());
        Some(file)
    }

    /// An entry is `<0|1>\n<key length in bytes>\n<key><stderr>`.
    fn get(&self, key: &str) -> Option<(bool, String)> {
        let entry = std::fs::read_to_string(self.file(key)?).ok()?;
        let (success, rest) = entry.split_once('\n')?;
        let (len, rest) = rest.split_once('\n')?;
        let stored_key = rest.get(..len.parse::<usize>().ok()?)?;
        if stored_key != key {
            return None;
        }
        let success = match success {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        Some((success, rest[stored_key.len()..].to_owned()))
    }

    /// Writes through a temporary file (unique to the process and the write) and a
    /// rename, so a reader sees the whole entry or none. A failure only loses the entry.
    fn put(&self, key: &str, success: bool, stderr: &str) {
        let Some(file) = self.file(key) else { return };
        let n = self.writes.fetch_add(1, Ordering::Relaxed);
        let tmp = file.with_extension(format!("tmp{}-{n}", std::process::id()));
        let entry = format!("{}\n{}\n{key}{stderr}", u8::from(success), key.len());
        if std::fs::write(&tmp, entry).is_err() || std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Removes what this run did not use: the directories of other rustcs, and in this
    /// one the entries of programs that are gone or changed and any temporary file left
    /// by an interrupted run. Called once every case has run.
    fn prune(&self) {
        let Some(dir) = &self.dir else { return };
        let used = self.used.lock().unwrap();
        let remove = |path: &Path| {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        };
        let siblings = dir
            .parent()
            .and_then(|parent| std::fs::read_dir(parent).ok());
        for entry in siblings.into_iter().flatten().flatten() {
            if entry.path() != *dir {
                remove(&entry.path());
            }
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if !used.contains(&entry.path()) {
                    remove(&entry.path());
                }
            }
        }
    }
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
fn find_rustc() -> Option<Rustc> {
    let path = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    let version = Command::new(&path).arg("-vV").output().ok()?;
    if !version.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    eprintln!(
        "differential: using {} ({})",
        path.display(),
        version.lines().next().unwrap_or_default()
    );
    Some(Rustc { path, version })
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
    src: String,
    sabiko: Outcome,
    rustc: Outcome,
}

/// Runs both compilers over all cases, one case per thread at a time
/// (`common::parallel_map`). A case whose run panics (Sabiko's prelude-diagnostics
/// assertion, a compiler panic) is an `Err` naming the file.
fn run_all(rustc: &Rustc, tmp: &Path, cases: &[Case]) -> Vec<Result<Result_, String>> {
    let cache = RustcCache::new(rustc);
    let results = common::parallel_map(cases, |case| {
        std::panic::catch_unwind(|| Result_ {
            rel: case.rel.clone(),
            src: case.src.clone(),
            sabiko: run_sabiko(&case.rel, &case.src),
            rustc: run_rustc(rustc, &cache, tmp, &case.rel, &case.src),
        })
        .map_err(|payload| {
            format!(
                "{}:\n  panicked: {}",
                case.rel,
                common::panic_message(&*payload)
            )
        })
    });
    cache.prune();
    results
}

/// Sabiko codes that are never the rejection a `rej_` file pins: syntax or codegen not
/// supported yet, and internal errors.
const SABIKO_UNSUPPORTED_CODES: &[&str] = &["E0107", "E0500", "E0900"];

/// The error codes a `rej_` file pins for `who` with a line `//@ rustc: E0277` or
/// `//@ sabiko: E0208` (several codes separated by spaces).
fn pinned_codes(src: &str, who: &str) -> Option<Vec<String>> {
    let prefix = format!("//@ {who}:");
    src.lines().find_map(|line| {
        line.trim()
            .strip_prefix(prefix.as_str())
            .map(|rest| rest.split_whitespace().map(str::to_owned).collect())
    })
}

/// A `rej_` file's rejections must be the ones it pins, so that it cannot pass on an
/// unrelated error (a typo, a construct not supported yet).
fn check_pinned_codes(r: &Result_, failures: &mut String) {
    for (who, outcome) in [("sabiko", &r.sabiko), ("rustc", &r.rustc)] {
        let Some(mut pinned) = pinned_codes(&r.src, who) else {
            let _ = writeln!(
                failures,
                "  a `rej_` file must pin {who}'s error codes with a `//@ {who}: ...` line"
            );
            continue;
        };
        let mut got = outcome.codes.clone();
        pinned.sort();
        got.sort();
        if pinned != got {
            let _ = writeln!(
                failures,
                "  {who} reports {got:?}, the file pins {pinned:?}:\n{}",
                indent(&outcome.detail)
            );
        }
    }
    for code in &r.sabiko.codes {
        if SABIKO_UNSUPPORTED_CODES.contains(&code.as_str()) {
            let _ = writeln!(
                failures,
                "  sabiko reports {code}, which is never the rejection a `rej_` file pins"
            );
        }
    }
}

/// Returns a failure description for one result, if any.
fn check(r: &Result_) -> Option<String> {
    let mut failures = String::new();
    let file_name = r.rel.rsplit('/').next().unwrap_or(&r.rel);
    let expected = if file_name.starts_with("ok_") {
        Some(Verdict::Accept)
    } else if file_name.starts_with("err_") {
        Some(Verdict::BorrowError)
    } else if file_name.starts_with("rej_") {
        // A program both compilers reject before borrow checking (a type error), kept
        // to pin a rejection rustc makes (decision 102).
        Some(Verdict::OtherError)
    } else {
        let _ = writeln!(
            failures,
            "  file name must start with `ok_`, `err_` or `rej_`"
        );
        None
    };
    for (who, outcome) in [("sabiko", &r.sabiko), ("rustc", &r.rustc)] {
        if outcome.verdict == Verdict::OtherError && expected != Some(Verdict::OtherError) {
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
    if expected == Some(Verdict::OtherError) {
        check_pinned_codes(r, &mut failures);
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
        let r = match r {
            Ok(r) => r,
            Err(panicked) => {
                eprintln!("FAIL {panicked}");
                failures.push(panicked.clone());
                continue;
            }
        };
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
