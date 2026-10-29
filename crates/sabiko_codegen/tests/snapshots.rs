//! IL golden tests: every `fixtures/*.sabi` is compiled through the whole pipeline and the
//! emitted IL is compared against `snapshots/`. Review changes with `cargo insta review`
//! (or set `INSTA_UPDATE=always` once to accept them). The run-pass tests in `sabikoc`
//! assemble, verify and execute programs; these snapshots keep the instruction selection
//! reviewable per construct.
//!
//! When the .NET toolchain is installed under `tools/` (`scripts/setup-dotnet.sh`), each
//! fixture's IL is also assembled with `ilasm` and checked with `ILVerify`, so a snapshot
//! can never be accepted with unverifiable IL in it. Without the toolchain that step is
//! skipped with a notice, unless `SABIKO_REQUIRE_DOTNET` is set.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use sabiko_codegen::{Options, OutputKind};
use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};

fn lower(path: &std::path::Path, overflow_checks: bool) -> String {
    let src = std::fs::read_to_string(path).unwrap();
    let mut sm = SourceMap::new();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let id = sm.add(name, src.clone());
    let mut sink = DiagnosticSink::new();
    let ast = sabiko_parser::parse_file(id, &src, &mut sink);
    let resolve = sabiko_resolve::resolve(&ast, &mut sink);
    let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
    let mut bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
    sabiko_mir::simplify::simplify_all(&mut bodies);
    sabiko_borrowck::check_all(&bodies, &resolve, &typeck, &mut sink);
    assert!(
        !sink.has_errors(),
        "fixture must be valid:\n{}",
        sabiko_diagnostics::render(sink.diagnostics(), &sm, Format::HumanPlain)
    );
    let has_entry = bodies
        .iter()
        .any(|b| b.name == "Main" && b.arg_count == 0 && b.return_ty().is_unit());
    let options = Options {
        assembly_name: stem,
        kind: if has_entry {
            OutputKind::Executable
        } else {
            OutputKind::Library
        },
        overflow_checks,
    };
    let il = sabiko_codegen::emit_il(&bodies, &resolve, &typeck, &options, &mut sink);
    assert!(
        !sink.has_errors(),
        "code generation reported errors:\n{}",
        sabiko_diagnostics::render(sink.diagnostics(), &sm, Format::HumanPlain)
    );
    il.expect("IL emitted")
}

// ---- assembling and verifying ----------------------------------------------------------

/// The .NET tools a verification needs, under `<repo>/tools` (`dotnet` may also come
/// from `PATH`).
#[derive(Debug)]
struct Toolchain {
    ilasm: PathBuf,
    dotnet: PathBuf,
    ilverify_dll: PathBuf,
    ref_dir: PathBuf,
    runtime_dll: PathBuf,
}

fn first_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

fn dotnet_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| [dir.join("dotnet"), dir.join("dotnet.exe")])
        .find(|p| p.is_file())
}

/// Locates the toolchain or names the first missing piece.
fn find_toolchain() -> Result<Toolchain, String> {
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools");
    let ilasm = first_file(&[tools.join("bin/ilasm"), tools.join("bin/ilasm.exe")])
        .ok_or("tools/bin/ilasm")?;
    let ilverify_dll =
        first_file(&[tools.join("ilverify/ILVerify.dll")]).ok_or("tools/ilverify/ILVerify.dll")?;
    let ref_dir = tools.join("ref");
    if !ref_dir.join("System.Runtime.dll").is_file() {
        return Err("tools/ref/System.Runtime.dll".to_owned());
    }
    let runtime_dll = tools.join("runtime/Sabiko.Runtime.dll");
    if !runtime_dll.is_file() {
        return Err("tools/runtime/Sabiko.Runtime.dll (run scripts/build-runtime.sh)".to_owned());
    }
    let dotnet = first_file(&[tools.join("dotnet/dotnet"), tools.join("dotnet/dotnet.exe")])
        .or_else(dotnet_on_path)
        .ok_or("dotnet (tools/dotnet or PATH)")?;
    Ok(Toolchain {
        ilasm,
        dotnet,
        ilverify_dll,
        ref_dir,
        runtime_dll,
    })
}

/// The toolchain, looked up once. `None` means the verification step is skipped (a notice
/// is printed); with `SABIKO_REQUIRE_DOTNET` set a missing toolchain fails instead.
fn toolchain() -> Option<&'static Toolchain> {
    static TOOLCHAIN: OnceLock<Option<Toolchain>> = OnceLock::new();
    TOOLCHAIN
        .get_or_init(|| match find_toolchain() {
            Ok(tc) => Some(tc),
            Err(missing) => {
                let require = std::env::var_os("SABIKO_REQUIRE_DOTNET").is_some_and(|v| v != "0");
                let msg = format!(
                    "snapshots: .NET toolchain not found ({missing} missing); run scripts/setup-dotnet.sh"
                );
                assert!(!require, "{msg} (SABIKO_REQUIRE_DOTNET is set)");
                eprintln!("{msg}; skipping ilasm + ILVerify");
                None
            }
        })
        .as_ref()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Assembles `il` to `<CARGO_TARGET_TMPDIR>/<name>.dll` and runs `ILVerify` over it.
/// `ReturnPtrToStack` is ignored: ECMA-335 forbids returning any managed pointer, which
/// rejects every `fn ... -> &T`; the borrow checker covers that (decision 30).
fn assemble_and_verify(name: &str, il: &str) {
    let Some(tc) = toolchain() else { return };
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(tmp).unwrap();
    let il_path = tmp.join(format!("{name}.il"));
    let dll = tmp.join(format!("{name}.dll"));
    std::fs::write(&il_path, il).unwrap();

    let out = Command::new(&tc.ilasm)
        .arg("-quiet")
        .arg("-dll")
        .arg(format!("-output:{}", dll.display()))
        .arg(&il_path)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{}`: {e}", tc.ilasm.display()));
    assert!(
        out.status.success() && dll.is_file(),
        "ilasm failed on {name} ({}):\n{}{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    );

    let out = Command::new(&tc.dotnet)
        .arg(&tc.ilverify_dll)
        .arg("-r")
        .arg(tc.ref_dir.join("*.dll"))
        .arg("-r")
        .arg(&tc.runtime_dll)
        .arg("-g")
        .arg("ReturnPtrToStack")
        .arg(&dll)
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{}`: {e}", tc.dotnet.display()));
    assert!(
        out.status.success(),
        "ILVerify rejected `{}`:\n{}{}",
        dll.display(),
        text(&out.stdout),
        text(&out.stderr)
    );
}

#[test]
fn fixtures_debug() {
    insta::glob!("fixtures/*.sabi", |path| {
        let il = lower(path, true);
        let stem = path.file_stem().unwrap().to_string_lossy();
        assemble_and_verify(&stem, &il);
        insta::assert_snapshot!(il);
    });
}

/// Release mode differs only in arithmetic; one fixture covers it.
#[test]
fn arith_release() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/arith.sabi");
    let il = lower(&path, false);
    assemble_and_verify("arith_release", &il);
    insta::assert_snapshot!("arith_release", il);
}

/// 128-bit arithmetic in release mode: the plain operator methods and masked shift amounts.
#[test]
fn wide_ints_release() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wide_ints.sabi");
    let il = lower(&path, false);
    assemble_and_verify("wide_ints_release", &il);
    insta::assert_snapshot!("wide_ints_release", il);
}
