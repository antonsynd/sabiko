//! Shared helpers for the tests that assemble and run programs: locating the .NET
//! toolchain under `tools/` (populated by `scripts/setup-dotnet.sh`), driving `sabikoc`,
//! `ILVerify` and `dotnet`.

#![allow(dead_code)]

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
