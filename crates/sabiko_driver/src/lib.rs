//! Drives the compilation pipeline. Phase 1 runs lexing and parsing; later phases add
//! resolution, type checking, borrow checking and code generation behind the same entry
//! points.

use std::fmt::Write as _;
use std::path::Path;

pub use sabiko_codegen::OutputKind;
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode, Format, Severity, SourceMap};

/// What the driver should print after running the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Emit {
    /// Nothing; only diagnostics.
    #[default]
    None,
    /// The token stream, one per line.
    Tokens,
    /// The structural AST dump.
    Ast,
    /// The AST printed back as canonical Sabiko source.
    Pretty,
    /// The MIR of every function, after simplification. Empty when an earlier stage
    /// reported errors or the pipeline stopped before MIR building.
    Mir,
    /// The textual CIL module (`ilasm` input). Empty when any stage reported errors.
    Il,
}

/// How far down the pipeline to run. Diagnostics from later stages are never reported
/// when an earlier stage failed, so stopping early is also a way to test one stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Stage {
    Parse,
    Resolve,
    Typeck,
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub emit: Emit,
    pub stop_after: Stage,
    /// Library or executable (`Emit::Il` only). An executable needs `fn Main()`.
    pub output_kind: OutputKind,
    /// Checked integer arithmetic (debug) or wrapping (release).
    pub overflow_checks: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            emit: Emit::None,
            stop_after: Stage::Full,
            output_kind: OutputKind::Library,
            overflow_checks: true,
        }
    }
}

/// The result of running the pipeline on one source file.
#[derive(Debug)]
pub struct Compilation {
    pub source_map: SourceMap,
    /// Sorted by position.
    pub diagnostics: Vec<Diagnostic>,
    /// Whatever `Options::emit` asked for (empty for `Emit::None`).
    pub output: String,
}

impl Compilation {
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// The number of error-severity diagnostics (warnings are not counted).
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    #[must_use]
    pub fn render_diagnostics(&self, format: Format) -> String {
        sabiko_diagnostics::render(&self.diagnostics, &self.source_map, format)
    }
}

/// Stack of the thread [`compile_str`] runs the pipeline on. Every stage recurses over the
/// syntax tree, whose depth [`sabiko_parser::MAX_NESTING`] bounds; at that depth a debug
/// build needs up to 8 MiB, more than a test thread's 2 MiB. The stack is reserved, not
/// committed, so the margin costs nothing.
const PIPELINE_STACK_SIZE: usize = 64 << 20;

/// Compiles source text given under `name` (used in diagnostics). The pipeline runs on a
/// thread of its own with a stack sized for the deepest program the parser accepts, so
/// the caller's stack size does not matter; a panic in the pipeline is resumed here.
#[must_use]
pub fn compile_str(name: &str, src: &str, options: Options) -> Compilation {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("sabiko-pipeline".to_owned())
            .stack_size(PIPELINE_STACK_SIZE)
            .spawn_scoped(scope, || run_pipeline(name, src, options))
            .expect("failed to spawn the compiler thread")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

fn run_pipeline(name: &str, src: &str, options: Options) -> Compilation {
    let mut source_map = SourceMap::new();
    let file = source_map.add(name, src);
    let mut sink = sabiko_diagnostics::DiagnosticSink::new();
    let output = match options.emit {
        Emit::Tokens => {
            let tokens = sabiko_lexer::lex(file, src, &mut sink);
            let mut out = String::new();
            for t in &tokens {
                let text = &src[t.span.range()];
                let _ = writeln!(
                    out,
                    "{:?} {}..{} {}",
                    t.kind,
                    t.span.start,
                    t.span.end,
                    text.escape_debug()
                );
            }
            out
        }
        Emit::None | Emit::Ast | Emit::Pretty | Emit::Mir | Emit::Il => {
            let ast = sabiko_parser::parse_file(file, src, &mut sink);
            match options.emit {
                Emit::Ast => sabiko_ast::dump::dump(&ast),
                Emit::Pretty => sabiko_ast::pretty::pretty(&ast),
                _ => {
                    // Semantic analysis only runs on a syntactically complete file:
                    // recovery drops broken constructs, and reporting "cannot find value"
                    // for names defined inside them would only add noise.
                    let mut output = String::new();
                    if options.stop_after > Stage::Parse && !sink.has_errors() {
                        let resolve = sabiko_resolve::resolve(&ast, &mut sink);
                        if options.stop_after > Stage::Resolve {
                            let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
                            // Signature elision (E0416) depends on signatures alone, so it
                            // is checked even when a body has errors and never reaches
                            // MIR, as rustc does; its errors do not keep MIR from being
                            // built and borrow-checked for the other bodies.
                            let body_errors = sink.has_errors();
                            if options.stop_after == Stage::Full {
                                sabiko_borrowck::check_signatures(&resolve, &typeck, &mut sink);
                            }
                            if options.stop_after == Stage::Full && !body_errors {
                                let mut bodies = sabiko_mir::build::build_bodies(
                                    &ast, &resolve, &typeck, &mut sink,
                                );
                                check_mir(&bodies, &typeck, &mut sink);
                                sabiko_mir::simplify::simplify_all(&mut bodies);
                                check_mir(&bodies, &typeck, &mut sink);
                                sabiko_borrowck::check_all(&bodies, &resolve, &typeck, &mut sink);
                                if options.emit == Emit::Mir {
                                    output = sabiko_mir::pretty::print_bodies(
                                        &bodies, &resolve, &typeck,
                                    );
                                }
                                if options.emit == Emit::Il && !sink.has_errors() {
                                    let codegen = sabiko_codegen::Options {
                                        assembly_name: assembly_name(name),
                                        kind: options.output_kind,
                                        overflow_checks: options.overflow_checks,
                                    };
                                    output = sabiko_codegen::emit_il(
                                        &bodies,
                                        &resolve,
                                        &typeck,
                                        &source_map,
                                        &codegen,
                                        &mut sink,
                                    )
                                    .unwrap_or_default();
                                }
                            }
                        }
                    }
                    output
                }
            }
        }
    };
    let mut diagnostics = sink.into_vec();
    sabiko_diagnostics::sort(&mut diagnostics);
    Compilation {
        source_map,
        diagnostics,
        output,
    }
}

/// The assembly name for a source file: its file stem (`dir/hello.sabi` gives `hello`).
#[must_use]
pub fn assembly_name(file_name: &str) -> String {
    let stem = Path::new(file_name).file_stem().map_or_else(
        || file_name.to_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    if stem.is_empty() {
        "Sabiko".to_owned()
    } else {
        stem
    }
}

/// Runs the MIR validator and reports any violation as an internal compiler error. Debug
/// builds panic instead, so a broken pass fails its tests loudly.
fn check_mir(
    bodies: &[sabiko_mir::Body],
    typeck: &sabiko_types::TypeckResults,
    sink: &mut DiagnosticSink,
) {
    let failures = sabiko_mir::validate::validate_all(bodies, typeck);
    debug_assert!(
        failures.is_empty(),
        "MIR validation failed:\n{}",
        failures.join("\n")
    );
    if failures.is_empty() {
        return;
    }
    let mut d = Diagnostic::error(
        ErrorCode::INTERNAL_ERROR,
        "internal compiler error: MIR validation failed",
    );
    for failure in failures {
        d = d.with_note(failure);
    }
    sink.emit(d);
}

/// Reads and compiles a file from disk.
pub fn compile_file(path: &Path, options: Options) -> std::io::Result<Compilation> {
    let src = std::fs::read_to_string(path)?;
    Ok(compile_str(&path.display().to_string(), &src, options))
}

#[cfg(test)]
mod tests {
    use sabiko_diagnostics::ErrorCode;

    use super::*;

    #[test]
    fn error_count_ignores_warnings() {
        let c = Compilation {
            source_map: SourceMap::new(),
            diagnostics: vec![
                Diagnostic::warning(ErrorCode::UNSUPPORTED_SYNTAX, "w1"),
                Diagnostic::error(ErrorCode::UNEXPECTED_TOKEN, "e"),
                Diagnostic::warning(ErrorCode::UNSUPPORTED_SYNTAX, "w2"),
            ],
            output: String::new(),
        };
        assert_eq!(c.diagnostics.len(), 3);
        assert_eq!(c.error_count(), 1);
        assert!(c.has_errors());

        let clean = Compilation {
            source_map: SourceMap::new(),
            diagnostics: vec![Diagnostic::warning(ErrorCode::UNSUPPORTED_SYNTAX, "w")],
            output: String::new(),
        };
        assert_eq!(clean.error_count(), 0);
        assert!(!clean.has_errors());
    }
}
