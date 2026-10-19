//! `sabikoc`: command-line front door to the Sabiko compiler.
//!
//! `--emit=il` prints the CIL module; `--emit=dll`/`--emit=exe` assemble it with `ilasm`
//! into `<stem>.dll` (plus `<stem>.runtimeconfig.json` for executables) and place
//! `Sabiko.Runtime.dll` next to it, so `dotnet <stem>.dll` runs the program.

mod toolchain;

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, ValueEnum};
use sabiko_diagnostics::Format;
use sabiko_driver::{Compilation, Emit, Options, OutputKind, Stage};

#[derive(Parser, Debug)]
#[command(name = "sabikoc", version, about = "The Sabiko compiler", long_about = None)]
struct Cli {
    /// Source file to compile (`.sabi`).
    input: PathBuf,

    /// What to produce.
    #[arg(long, value_enum, default_value_t = EmitArg::None)]
    emit: EmitArg,

    /// Output path. Defaults to stdout for text outputs and `<stem>.dll` for assemblies.
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,

    /// Wrap on integer overflow instead of panicking.
    #[arg(long)]
    release: bool,

    /// Directory containing `Sabiko.Runtime.dll` (default: `SABIKO_RUNTIME_DIR`, then the
    /// checkout's `tools/runtime`).
    #[arg(long)]
    runtime_path: Option<PathBuf>,

    /// The `ilasm` executable (default: `SABIKO_ILASM`, `PATH`, then `tools/bin/ilasm`).
    #[arg(long)]
    ilasm: Option<PathBuf>,

    /// Stop the pipeline after this stage (diagnostics of later stages are not reported).
    #[arg(long, value_enum, default_value_t = StageArg::Full)]
    stop_after: StageArg,

    /// How to print diagnostics.
    #[arg(long, value_enum, default_value_t = ErrorFormat::Human)]
    error_format: ErrorFormat,

    /// Colour output.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum EmitArg {
    /// Diagnostics only.
    None,
    Tokens,
    Ast,
    Pretty,
    Mir,
    /// The textual CIL module.
    Il,
    /// A library assembly.
    Dll,
    /// An executable (`Main` becomes the entry point).
    Exe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum StageArg {
    Parse,
    Resolve,
    Typeck,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ErrorFormat {
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Color {
    Auto,
    Always,
    Never,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let emit = match cli.emit {
        EmitArg::None => Emit::None,
        EmitArg::Tokens => Emit::Tokens,
        EmitArg::Ast => Emit::Ast,
        EmitArg::Pretty => Emit::Pretty,
        EmitArg::Mir => Emit::Mir,
        EmitArg::Il | EmitArg::Dll | EmitArg::Exe => Emit::Il,
    };
    let stop_after = match cli.stop_after {
        StageArg::Parse => Stage::Parse,
        StageArg::Resolve => Stage::Resolve,
        StageArg::Typeck => Stage::Typeck,
        StageArg::Full => Stage::Full,
    };
    let options = Options {
        emit,
        stop_after,
        output_kind: if cli.emit == EmitArg::Exe {
            OutputKind::Executable
        } else {
            OutputKind::Library
        },
        overflow_checks: !cli.release,
    };
    // Resolve the toolchain before compiling so a missing tool is reported first.
    let tools = if matches!(cli.emit, EmitArg::Dll | EmitArg::Exe) {
        let ilasm = toolchain::find_ilasm(cli.ilasm.as_deref());
        let runtime = toolchain::find_runtime_dir(cli.runtime_path.as_deref());
        match (ilasm, runtime) {
            (Ok(i), Ok(r)) => Some((i, r)),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    let compilation = match sabiko_driver::compile_file(&cli.input, options) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read `{}`: {e}", cli.input.display());
            return ExitCode::from(2);
        }
    };
    let color = match cli.color {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    let format = match (cli.error_format, color) {
        (ErrorFormat::Json, _) => Format::Json,
        (ErrorFormat::Human, true) => Format::Human,
        (ErrorFormat::Human, false) => Format::HumanPlain,
    };
    let rendered = compilation.render_diagnostics(format);
    let _ = std::io::stderr().write_all(rendered.as_bytes());
    if compilation.has_errors() {
        let n = compilation.error_count();
        if format != Format::Json {
            eprintln!(
                "error: could not compile `{}` due to {n} previous error{}",
                cli.input.display(),
                if n == 1 { "" } else { "s" }
            );
        }
        return ExitCode::from(1);
    }
    let result = match (cli.emit, tools) {
        (EmitArg::Dll | EmitArg::Exe, Some((ilasm, runtime_dir))) => assemble(
            &cli,
            &compilation,
            &ilasm,
            &runtime_dir,
            cli.emit == EmitArg::Exe,
        ),
        _ => write_text(cli.output.as_deref(), &compilation.output),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn write_text(output: Option<&Path>, text: &str) -> Result<(), String> {
    if let Some(path) = output {
        std::fs::write(path, text).map_err(|e| format!("cannot write `{}`: {e}", path.display()))
    } else {
        let _ = std::io::stdout().write_all(text.as_bytes());
        Ok(())
    }
}

/// Runs `ilasm` on the emitted IL and lays out the output directory.
fn assemble(
    cli: &Cli,
    compilation: &Compilation,
    ilasm: &Path,
    runtime_dir: &Path,
    exe: bool,
) -> Result<(), String> {
    let stem = sabiko_driver::assembly_name(&cli.input.display().to_string());
    let output = cli
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{stem}.dll")));
    let out_dir = match output.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("cannot create `{}`: {e}", out_dir.display()))?;
    let il_path = output.with_extension("il");
    std::fs::write(&il_path, &compilation.output)
        .map_err(|e| format!("cannot write `{}`: {e}", il_path.display()))?;
    let status = Command::new(ilasm)
        .arg("-quiet")
        .arg(if exe { "-exe" } else { "-dll" })
        .arg(format!("-output:{}", output.display()))
        .arg(&il_path)
        .output()
        .map_err(|e| format!("cannot run `{}`: {e}", ilasm.display()))?;
    if !status.status.success() {
        let _ = std::io::stderr().write_all(&status.stdout);
        let _ = std::io::stderr().write_all(&status.stderr);
        return Err(format!(
            "ilasm failed on `{}` (the IL is left in place for inspection)",
            il_path.display()
        ));
    }
    let _ = std::fs::remove_file(&il_path);
    let runtime = runtime_dir.join(toolchain::RUNTIME_DLL);
    let target = out_dir.join(toolchain::RUNTIME_DLL);
    if !same_file(&runtime, &target) {
        std::fs::copy(&runtime, &target).map_err(|e| {
            format!(
                "cannot copy `{}` to `{}`: {e}",
                runtime.display(),
                target.display()
            )
        })?;
    }
    if exe {
        let config = output.with_extension("runtimeconfig.json");
        std::fs::write(&config, toolchain::RUNTIME_CONFIG)
            .map_err(|e| format!("cannot write `{}`: {e}", config.display()))?;
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
