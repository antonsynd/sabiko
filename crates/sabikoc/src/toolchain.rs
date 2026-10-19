//! Locating the .NET tools `sabikoc` drives: `ilasm` and the runtime library.
//!
//! Lookup order for each tool: the command-line flag, then an environment variable, then
//! `PATH` (for `ilasm`), then the `tools/` directory `scripts/setup-dotnet.sh` populates,
//! searched upwards from the compiler executable and from the working directory.

use std::path::{Path, PathBuf};

/// The runtime library's file name.
pub const RUNTIME_DLL: &str = "Sabiko.Runtime.dll";

/// Finds `ilasm`.
pub fn find_ilasm(flag: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = flag {
        return existing(p, "--ilasm");
    }
    if let Some(p) = std::env::var_os("SABIKO_ILASM") {
        return existing(Path::new(&p), "SABIKO_ILASM");
    }
    if let Some(p) = on_path("ilasm") {
        return Ok(p);
    }
    for root in search_roots() {
        for name in ["ilasm", "ilasm.exe"] {
            let candidate = root.join("tools").join("bin").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(
        "cannot find `ilasm`: pass `--ilasm <path>`, set `SABIKO_ILASM`, put it on `PATH`, \
         or run `scripts/setup-dotnet.sh`"
            .to_owned(),
    )
}

/// Finds the directory holding `Sabiko.Runtime.dll`.
pub fn find_runtime_dir(flag: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = flag {
        let dir = if p.is_file() {
            p.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            p.to_path_buf()
        };
        return if dir.join(RUNTIME_DLL).is_file() {
            Ok(dir)
        } else {
            Err(format!(
                "`--runtime-path`: `{}` does not contain `{RUNTIME_DLL}`",
                dir.display()
            ))
        };
    }
    if let Some(p) = std::env::var_os("SABIKO_RUNTIME_DIR") {
        let dir = PathBuf::from(p);
        return if dir.join(RUNTIME_DLL).is_file() {
            Ok(dir)
        } else {
            Err(format!(
                "`SABIKO_RUNTIME_DIR`: `{}` does not contain `{RUNTIME_DLL}`",
                dir.display()
            ))
        };
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        if dir.join(RUNTIME_DLL).is_file() {
            return Ok(dir);
        }
    }
    for root in search_roots() {
        let candidate = root.join("tools").join("runtime");
        if candidate.join(RUNTIME_DLL).is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "cannot find `{RUNTIME_DLL}`: pass `--runtime-path <dir>`, set `SABIKO_RUNTIME_DIR`, \
         or run `scripts/setup-dotnet.sh`"
    ))
}

fn existing(p: &Path, what: &str) -> Result<PathBuf, String> {
    if p.is_file() {
        Ok(p.to_path_buf())
    } else {
        Err(format!("{what}: `{}` is not a file", p.display()))
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in [dir.join(name), dir.join(format!("{name}.exe"))] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Directories that may be a Sabiko checkout: ancestors of the executable (a `target/`
/// build sits two levels below the root) and of the working directory.
fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push_ancestors = |start: PathBuf| {
        let mut dir = Some(start.as_path());
        for _ in 0..5 {
            match dir {
                Some(d) => {
                    roots.push(d.to_path_buf());
                    dir = d.parent();
                }
                None => break,
            }
        }
    };
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        push_ancestors(exe_dir);
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_ancestors(cwd);
    }
    roots
}

/// The `runtimeconfig.json` an executable needs next to it.
pub const RUNTIME_CONFIG: &str = r#"{
  "runtimeOptions": {
    "tfm": "net10.0",
    "framework": { "name": "Microsoft.NETCore.App", "version": "10.0.0" }
  }
}
"#;
