//! `sabikoc`: command-line front door to the Sabiko compiler.

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use sabiko_diagnostics::Format;
use sabiko_driver::{Emit, Options};

#[derive(Parser, Debug)]
#[command(name = "sabikoc", version, about = "The Sabiko compiler", long_about = None)]
struct Cli {
    /// Source file to compile (`.sabi`).
    input: PathBuf,

    /// What to print after compiling.
    #[arg(long, value_enum, default_value_t = EmitArg::None)]
    emit: EmitArg,

    /// How to print diagnostics.
    #[arg(long, value_enum, default_value_t = ErrorFormat::Human)]
    error_format: ErrorFormat,

    /// Colour output.
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum EmitArg {
    None,
    Tokens,
    Ast,
    Pretty,
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
    };
    let compilation = match sabiko_driver::compile_file(&cli.input, Options { emit }) {
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
    let _ = std::io::stdout().write_all(compilation.output.as_bytes());
    if compilation.has_errors() {
        let n = compilation.diagnostics.len();
        if format != Format::Json {
            eprintln!(
                "error: could not compile `{}` due to {n} previous error{}",
                cli.input.display(),
                if n == 1 { "" } else { "s" }
            );
        }
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
