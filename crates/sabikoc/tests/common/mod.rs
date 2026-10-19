//! Shared helpers for the tests that assemble and run programs: locating the .NET
//! toolchain under `tools/` (populated by `scripts/setup-dotnet.sh`), driving `sabikoc`,
//! `ILVerify` and `dotnet`.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The tools a run needs. All paths are absolute.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub ilasm: PathBuf,
    pub dotnet: PathBuf,
    pub ilverify_dll: PathBuf,
    pub ref_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

/// The repository root (two levels above this crate).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn first_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

fn dotnet_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["dotnet", "dotnet.exe"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Locates the toolchain, or explains what is missing. With `SABIKO_REQUIRE_DOTNET=1` a
/// missing tool is a test failure instead of a skip.
pub fn toolchain() -> Result<Toolchain, String> {
    let root = repo_root();
    let tools = root.join("tools");
    let ilasm = first_file(&[tools.join("bin/ilasm"), tools.join("bin/ilasm.exe")])
        .ok_or("tools/bin/ilasm")?;
    let dotnet = first_file(&[tools.join("dotnet/dotnet"), tools.join("dotnet/dotnet.exe")])
        .or_else(dotnet_on_path)
        .ok_or("dotnet (tools/dotnet or PATH)")?;
    let ilverify_dll =
        first_file(&[tools.join("ilverify/ILVerify.dll")]).ok_or("tools/ilverify/ILVerify.dll")?;
    let ref_dir = tools.join("ref");
    if !ref_dir.join("System.Runtime.dll").is_file() {
        return Err("tools/ref/System.Runtime.dll".to_owned());
    }
    let runtime_dir = tools.join("runtime");
    if !runtime_dir.join("Sabiko.Runtime.dll").is_file() {
        return Err("tools/runtime/Sabiko.Runtime.dll (run scripts/build-runtime.sh)".to_owned());
    }
    Ok(Toolchain {
        ilasm,
        dotnet,
        ilverify_dll,
        ref_dir,
        runtime_dir,
    })
}

/// Returns the toolchain, or `None` after printing a skip notice (panics instead when
/// `SABIKO_REQUIRE_DOTNET` is set).
pub fn toolchain_or_skip(test: &str) -> Option<Toolchain> {
    match toolchain() {
        Ok(t) => Some(t),
        Err(missing) => {
            let require = std::env::var_os("SABIKO_REQUIRE_DOTNET").is_some_and(|v| v != "0");
            let msg = format!(
                "{test}: .NET toolchain not found ({missing} missing); run scripts/setup-dotnet.sh"
            );
            assert!(!require, "{msg} (SABIKO_REQUIRE_DOTNET is set)");
            eprintln!("{msg}; skipping");
            None
        }
    }
}

pub fn collect_sabi(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_sabi(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
    out.sort();
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Compiles `source` to an executable at `out_dll` with the `sabikoc` binary under test.
pub fn compile_exe(
    tc: &Toolchain,
    source: &Path,
    out_dll: &Path,
    release: bool,
) -> Result<(), String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sabikoc"));
    cmd.arg(source)
        .arg("--emit=exe")
        .arg("-o")
        .arg(out_dll)
        .arg("--ilasm")
        .arg(&tc.ilasm)
        .arg("--runtime-path")
        .arg(&tc.runtime_dir)
        .arg("--color=never");
    if release {
        cmd.arg("--release");
    }
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run sabikoc: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sabikoc failed ({}):\n{}{}",
            out.status,
            text(&out.stdout),
            text(&out.stderr)
        ))
    }
}

/// Runs `ILVerify` on `dll`. `ReturnPtrToStack` is ignored: ECMA-335 forbids returning any
/// managed pointer, which rejects every `fn ... -> &T`; the borrow checker covers that.
pub fn ilverify(tc: &Toolchain, dll: &Path) -> Result<(), String> {
    let out = Command::new(&tc.dotnet)
        .arg(&tc.ilverify_dll)
        .arg("-r")
        .arg(tc.ref_dir.join("*.dll"))
        .arg("-r")
        .arg(tc.runtime_dir.join("Sabiko.Runtime.dll"))
        .arg("-g")
        .arg("ReturnPtrToStack")
        .arg(dll)
        .output()
        .map_err(|e| format!("cannot run ILVerify: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ILVerify rejected `{}`:\n{}{}",
            dll.display(),
            text(&out.stdout),
            text(&out.stderr)
        ))
    }
}

/// Runs an assembled executable.
pub fn run_dll(tc: &Toolchain, dll: &Path) -> Result<Output, String> {
    Command::new(&tc.dotnet)
        .arg(dll)
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .output()
        .map_err(|e| format!("cannot run `{}`: {e}", dll.display()))
}

/// Runs `job` over `items` on a few threads, collecting one result per item in order.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], job: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<R>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= items.len() {
                    break;
                }
                *results[i].lock().unwrap() = Some(job(&items[i]));
            });
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().expect("job ran"))
        .collect()
}

// ----- rustc as an oracle ---------------------------------------------------------------

/// The `rustc` to compare against: `$RUSTC`, else `rustc` on `PATH`; `None` when it cannot
/// be run.
pub fn rustc() -> Option<PathBuf> {
    let candidate = std::env::var_os("RUSTC").map_or_else(|| PathBuf::from("rustc"), PathBuf::from);
    Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| candidate)
}

/// Returns rustc, or `None` after printing a skip notice (panics instead when
/// `SABIKO_REQUIRE_RUSTC` is set).
pub fn rustc_or_skip(test: &str) -> Option<PathBuf> {
    let found = rustc();
    if found.is_none() {
        let require = std::env::var_os("SABIKO_REQUIRE_RUSTC").is_some_and(|v| v != "0");
        assert!(
            !require,
            "{test}: rustc not found and SABIKO_REQUIRE_RUSTC is set"
        );
        eprintln!("{test}: rustc not found; skipping");
    }
    found
}

/// Prepended to a generated program before handing it to `rustc`: Sabiko's builtins, an
/// entry point calling `Main`, and the lints that would otherwise turn a constant-visible
/// overflow or division by zero into a compile error (both compilers panic at run time
/// instead, which is what the comparison wants to see).
pub const RUSTC_GENERATED_PRELUDE: &str = "#![allow(nonstandard_style, unused, dead_code, \
                                            arithmetic_overflow, unconditional_panic)]
fn Print<T: std::fmt::Display>(x: T) { print!(\"{x}\") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!(\"{x}\") }
fn Panic(msg: &str) -> ! { panic!(\"{msg}\") }
fn main() { Main() }
";

/// What one program did when run.
#[derive(Debug, Clone)]
pub struct Run {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Builds `prelude + src` with rustc as an executable in `dir` and runs it.
pub fn run_rust(rustc: &Path, prelude: &str, src: &str, dir: &Path) -> Result<Run, String> {
    let rs = dir.join("prog.rs");
    std::fs::write(&rs, format!("{prelude}{src}")).unwrap();
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
    // A backtrace would add lines after the panic message.
    let out = Command::new(&exe)
        .env("RUST_BACKTRACE", "0")
        .output()
        .map_err(|e| format!("cannot run `{}`: {e}", exe.display()))?;
    Ok(Run {
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
        code: out.status.code().unwrap_or(-1),
    })
}

/// Builds the Sabiko program at `path` as an executable in `dir` (debug mode), verifies it
/// and runs it.
pub fn run_sabiko(tc: &Toolchain, path: &Path, dir: &Path) -> Result<Run, String> {
    let dll = dir.join("prog.dll");
    compile_exe(tc, path, &dll, false)?;
    ilverify(tc, &dll)?;
    let out = run_dll(tc, &dll)?;
    Ok(Run {
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
        code: out.status.code().unwrap_or(-1),
    })
}

/// The panic message line from a panicking program's stderr. rustc writes
/// `thread 'main' panicked at src/main.rs:L:C:` (newer versions insert the thread id),
/// Sabiko writes `thread 'main' panicked:`; the message is the next line in both.
pub fn panic_message(stderr: &str) -> Option<String> {
    let mut lines = stderr.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("thread 'main'") && line.contains("panicked") {
            return lines.next().map(str::to_owned);
        }
    }
    None
}

/// Rust's messages for the overflow checks of `+`, `-`, `*` and unary `-`. Sabiko lowers
/// those to the CLR's `.ovf` opcodes, whose `OverflowException` does not say which
/// operator overflowed, so its generic message is accepted for any of them.
const RUST_OVERFLOW_MESSAGES: &[&str] = &[
    "attempt to add with overflow",
    "attempt to subtract with overflow",
    "attempt to multiply with overflow",
    "attempt to negate with overflow",
];
const SABIKO_GENERIC_OVERFLOW: &str = "attempt to perform arithmetic with overflow";

pub fn panic_messages_agree(rust: &str, sabiko: &str) -> bool {
    rust == sabiko || (sabiko == SABIKO_GENERIC_OVERFLOW && RUST_OVERFLOW_MESSAGES.contains(&rust))
}

/// Compares two runs: exit codes and stdout must match; when both panicked (101) the panic
/// message lines must agree (see [`panic_messages_agree`]); when both exited with 0 neither
/// may have written to stderr. Returns the differences, or a one-line summary.
pub fn compare_runs(rust: &Run, sabi: &Run) -> Result<String, String> {
    let mut failures = String::new();
    if rust.code != sabi.code {
        let _ = write!(
            failures,
            "exit codes differ: rustc {} vs sabiko {}\nrust stderr:\n{}sabiko stderr:\n{}",
            rust.code, sabi.code, rust.stderr, sabi.stderr
        );
    }
    if rust.stdout != sabi.stdout {
        let _ = write!(
            failures,
            "stdout differs\n--- rustc\n{}--- sabiko\n{}---\n",
            rust.stdout, sabi.stdout
        );
    }
    if rust.code == sabi.code {
        if rust.code == 101 {
            match (panic_message(&rust.stderr), panic_message(&sabi.stderr)) {
                (Some(r), Some(s)) if panic_messages_agree(&r, &s) => {}
                (r, s) => {
                    let _ = write!(
                        failures,
                        "panic messages differ: rustc {r:?} vs sabiko {s:?}\nrust stderr:\n{}\
                         sabiko stderr:\n{}",
                        rust.stderr, sabi.stderr
                    );
                }
            }
        } else if rust.code == 0 && !(rust.stderr.is_empty() && sabi.stderr.is_empty()) {
            let _ = write!(
                failures,
                "stderr is not empty after a clean exit\nrust stderr:\n{}sabiko stderr:\n{}",
                rust.stderr, sabi.stderr
            );
        }
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

// ----- compile-time verdicts ------------------------------------------------------------

/// How a compiler judged a program (the classification of
/// `crates/sabiko_driver/tests/differential.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    BorrowError,
    /// At least one error that is not a borrow-checker verdict: the program is not valid
    /// in the shared subset, or a compiler is wrong.
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

/// rustc error codes that belong to borrow checking.
pub const RUSTC_BORROW_CODES: &[&str] = &[
    "E0106", "E0204", "E0373", "E0381", "E0382", "E0384", "E0499", "E0502", "E0503", "E0505",
    "E0506", "E0507", "E0515", "E0521", "E0594", "E0596", "E0597", "E0623", "E0716",
];
pub const RUSTC_UNCODED_BORROW_ERRORS: &[&str] = &["lifetime may not live long enough"];

/// Sabiko's borrow-checker codes (E0400..=E0417).
pub fn sabiko_code_is_borrow(code: &str) -> bool {
    code.strip_prefix('E')
        .and_then(|n| n.parse::<u16>().ok())
        .is_some_and(|n| (400..=417).contains(&n))
}

pub fn verdict_of(codes: &[String], is_borrow: impl Fn(&str) -> bool) -> Verdict {
    if codes.is_empty() {
        Verdict::Accept
    } else if codes.iter().all(|c| is_borrow(c)) {
        Verdict::BorrowError
    } else {
        Verdict::OtherError
    }
}

fn push_unique(codes: &mut Vec<String>, code: String) {
    if !codes.contains(&code) {
        codes.push(code);
    }
}

/// The error codes (or uncoded error messages) in rustc's stderr.
pub fn rustc_error_codes(success: bool, stderr: &str) -> Vec<String> {
    if success {
        return Vec::new();
    }
    let mut codes = Vec::new();
    for line in stderr.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("error[") {
            if let Some(end) = rest.find(']') {
                push_unique(&mut codes, rest[..end].to_owned());
            }
        } else if let Some(msg) = line.strip_prefix("error: ") {
            if !msg.starts_with("aborting due to") {
                push_unique(&mut codes, msg.to_owned());
            }
        }
    }
    if codes.is_empty() {
        codes.push("rustc exited with failure and no error lines".to_owned());
    }
    codes
}

/// Type-checks `prelude + src` with rustc (`--crate-type lib --emit=metadata`) in `dir`
/// and returns its verdict with the codes behind it and its stderr.
pub fn rustc_verdict(
    rustc: &Path,
    prelude: &str,
    src: &str,
    dir: &Path,
) -> (Verdict, Vec<String>, String) {
    let rs = dir.join("prog.rs");
    std::fs::write(&rs, format!("{prelude}{src}")).unwrap();
    let out = Command::new(rustc)
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--emit=metadata",
            "-A",
            "warnings",
            "--crate-name",
            "prog",
            "--out-dir",
        ])
        .arg(dir)
        .arg(&rs)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", rustc.display()));
    let stderr = text(&out.stderr);
    let codes = rustc_error_codes(out.status.success(), &stderr);
    let verdict = verdict_of(&codes, |c| {
        RUSTC_BORROW_CODES.contains(&c) || RUSTC_UNCODED_BORROW_ERRORS.contains(&c)
    });
    (verdict, codes, stderr)
}

/// Seeds for the generated-program tests: `SABIKO_GENERATED_SEEDS` of them (default
/// `default_count`) starting at `SABIKO_GENERATED_BASE_SEED` (default 1).
pub fn generated_seeds(default_count: u64) -> Vec<u64> {
    let count = std::env::var("SABIKO_GENERATED_SEEDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default_count);
    let base = std::env::var("SABIKO_GENERATED_BASE_SEED")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1);
    (base..base.saturating_add(count)).collect()
}
