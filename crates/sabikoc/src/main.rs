//! `sabikoc`: the Sabiko compiler driver.
//!
//! Phase 0 skeleton. The pipeline crates (lexer, parser, resolver, type checker,
//! MIR, borrow checker, CIL codegen) are added in later phases; see `docs/PLAN.md`.

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

/// Dispatches on the command line and returns what should be printed to stdout.
fn run(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => Ok(format!("sabikoc {VERSION}\n")),
        Some("--help" | "-h") | None => Ok(usage()),
        Some(other) => Err(format!("unrecognised argument `{other}`\n\n{}", usage())),
    }
}

fn usage() -> String {
    format!(
        "sabikoc {VERSION}\n\
         Usage: sabikoc [OPTIONS] <INPUT.sabi>\n\n\
         Options:\n  \
           -h, --help       Print this help\n  \
           -V, --version    Print version\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_prints_version() {
        let out = run(&["--version".to_owned()]).unwrap();
        assert_eq!(out, format!("sabikoc {VERSION}\n"));
    }

    #[test]
    fn unknown_argument_is_an_error() {
        let err = run(&["--bogus".to_owned()]).unwrap_err();
        assert!(err.contains("--bogus"));
    }
}
