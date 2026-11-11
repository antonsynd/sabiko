//! Interop tests: C# programs that use Sabiko libraries. Each directory
//! `tests/interop/<name>/` holds `lib.sabi`, a Sabiko library, `Test.cs`, a C# program
//! using it, and `expected.stdout`. The harness compiles `lib.sabi` with the `sabikoc`
//! binary (`--emit=dll`) and verifies it with `ILVerify`, compiles `Test.cs` with the Roslyn
//! `csc` from `tools/csc` against the reference assemblies, `lib.dll` and
//! `Sabiko.Runtime.dll` (no .NET SDK, as `scripts/build-runtime.sh` does), runs it with
//! `dotnet` and compares its stdout with `expected.stdout` (`BLESS=1` writes it). The
//! program must exit with 0 and leave stderr empty. `lib.sabi` must also be valid Rust:
//! when rustc is installed it checks the library with the builtins' prelude. See
//! `docs/TESTING.md` (Interop tests).

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{text, Toolchain};

/// The `runtimeconfig.json` of the C# program: the one `sabikoc --emit=exe` writes
/// (`crates/sabikoc/src/toolchain.rs`), so both run on the same framework.
const RUNTIME_CONFIG: &str = r#"{
  "runtimeOptions": {
    "tfm": "net10.0",
    "framework": { "name": "Microsoft.NETCore.App", "version": "10.0.0" }
  }
}
"#;

/// The builtins a library may call, for checking `lib.sabi` with rustc.
const RUSTC_LIB_PRELUDE: &str = "#![allow(nonstandard_style, unused, dead_code)]
fn Print<T: std::fmt::Display>(x: T) { print!(\"{x}\") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!(\"{x}\") }
#[track_caller]
fn Panic(msg: &str) -> ! { panic!(\"{msg}\") }
";

/// The Roslyn compiler `scripts/setup-dotnet.sh` (and `.ps1`) installs, run on `dotnet`.
fn csc_dll() -> Result<PathBuf, String> {
    let csc = common::repo_root().join("tools/csc/csc.dll");
    if csc.is_file() {
        Ok(csc)
    } else {
        Err("tools/csc/csc.dll".to_owned())
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<std::process::Output, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {what}: {e}"))?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(format!(
            "{what} failed ({}):\n{}{}",
            out.status,
            text(&out.stdout),
            text(&out.stderr)
        ))
    }
}

/// Compiles `source` into the executable `out` with csc, referencing every reference
/// assembly and `refs`. The references go into a response file, which keeps the command
/// line short (Windows limits it to 32767 characters).
fn csc(
    tc: &Toolchain,
    csc: &Path,
    source: &Path,
    refs: &[PathBuf],
    out: &Path,
) -> Result<(), String> {
    let mut rsp = String::new();
    let mut ref_dlls: Vec<PathBuf> = std::fs::read_dir(&tc.ref_dir)
        .map_err(|e| format!("cannot read `{}`: {e}", tc.ref_dir.display()))?
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "dll"))
        .collect();
    ref_dlls.sort();
    for dll in ref_dlls.iter().chain(refs) {
        let _ = writeln!(rsp, "-r:\"{}\"", dll.display());
    }
    let _ = writeln!(rsp, "\"{}\"", source.display());
    let rsp_path = out.with_extension("rsp");
    std::fs::write(&rsp_path, rsp).unwrap();
    run(
        Command::new(&tc.dotnet)
            .arg(csc)
            .args([
                "-nologo",
                "-noconfig",
                "-nostdlib",
                "-target:exe",
                "-deterministic",
                "-langversion:latest",
                "-nullable:enable",
                "-warnaserror",
                // `lib.dll` references the `net10.0` framework (sabikoc's target), which on a
                // .NET 11 toolchain is older than the reference assemblies: CS1701 says csc
                // unifies the two, as the runtime does. MSBuild disables it by default too.
                "-nowarn:1701,1702",
            ])
            .arg(format!("-out:{}", out.display()))
            .arg(format!("@{}", rsp_path.display())),
        "csc",
    )?;
    std::fs::write(out.with_extension("runtimeconfig.json"), RUNTIME_CONFIG).unwrap();
    Ok(())
}

/// Checks that `lib` is valid Rust: rustc builds it as a library with the builtins.
fn rustc_check(rustc: &Path, lib: &Path, dir: &Path) -> Result<(), String> {
    let src = std::fs::read_to_string(lib).unwrap();
    let rs = dir.join("lib.rs");
    std::fs::write(&rs, format!("{RUSTC_LIB_PRELUDE}{src}")).unwrap();
    run(
        Command::new(rustc)
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "lib",
                "--emit=metadata",
                "-A",
                "warnings",
                "-o",
            ])
            .arg(dir.join("liblib.rmeta"))
            .arg(&rs),
        "rustc (lib.sabi must be valid Rust)",
    )
    .map(|_| ())
}

fn run_one(
    tc: &Toolchain,
    csc_path: &Path,
    rustc: Option<&Path>,
    case: &Path,
) -> Result<(), String> {
    let name = case.file_name().unwrap().to_string_lossy().into_owned();
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("interop")
        .join(&name);
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    let lib = out_dir.join("lib.dll");
    common::compile_dll(tc, &case.join("lib.sabi"), &lib)?;
    common::ilverify(tc, &lib)?;
    if let Some(rustc) = rustc {
        rustc_check(rustc, &case.join("lib.sabi"), &out_dir)?;
    }
    let test = out_dir.join("Test.dll");
    csc(
        tc,
        csc_path,
        &case.join("Test.cs"),
        &[lib, out_dir.join("Sabiko.Runtime.dll")],
        &test,
    )?;
    let out = common::run_dll(tc, &test)?;
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    let code = common::exit_code(out.status);

    let mut failures = String::new();
    if code != 0 || !stderr.is_empty() {
        let _ = writeln!(failures, "exit code {code}\nstderr:\n{stderr}");
    }
    let expected_path = case.join("expected.stdout");
    let expected = std::fs::read_to_string(&expected_path)
        .ok()
        .map(|s| s.replace("\r\n", "\n"));
    let bless = std::env::var_os("BLESS").is_some();
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
fn interop() {
    let Some(tc) = common::toolchain_or_skip("interop") else {
        return;
    };
    let csc_path = match csc_dll() {
        Ok(p) => p,
        Err(missing) => {
            common::skip_without_dotnet("interop", &missing);
            return;
        }
    };
    let rustc = common::rustc_or_skip("interop (rustc check of lib.sabi)");
    let root = common::repo_root().join("tests/interop");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read `{}`: {e}", root.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("lib.sabi").is_file())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no cases under {}", root.display());
    for case in &cases {
        assert!(
            case.join("Test.cs").is_file(),
            "{} has no Test.cs",
            case.display()
        );
    }
    let results = common::parallel_map(&cases, |case| {
        run_one(&tc, &csc_path, rustc.as_deref(), case)
    });
    let mut report = String::new();
    let mut failed = 0;
    for (case, result) in cases.iter().zip(results) {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        match result {
            Ok(()) => eprintln!("interop {name}: ok"),
            Err(e) => {
                failed += 1;
                let _ = writeln!(report, "\n=== {name}\n{e}");
            }
        }
    }
    assert!(
        failed == 0,
        "{failed} of {} interop cases failed:{report}",
        cases.len()
    );
}
