//! Drives the compilation pipeline. Phase 1 runs lexing and parsing; later phases add
//! resolution, type checking, borrow checking and code generation behind the same entry
//! points.

use std::fmt::Write as _;
use std::path::Path;

pub use sabiko_codegen::OutputKind;
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode, Format, Severity, SourceMap};
pub use sabiko_mir::lint::{AllowedLints, Lint};

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
    /// The MIR of every function, after simplification: what the borrow checker sees,
    /// before copy forwarding. Empty when an earlier stage reported errors or the
    /// pipeline stopped before MIR building.
    Mir,
    /// The MIR after drop elaboration and simplification (decision 55), before copy
    /// forwarding. Empty when any stage reported errors.
    MirElaborated,
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
    /// rustc's constant-propagation lints to leave unreported (`sabikoc -A <lint>`); they
    /// run only when code is generated, as in rustc (decision 73).
    pub allowed_lints: AllowedLints,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            emit: Emit::None,
            stop_after: Stage::Full,
            output_kind: OutputKind::Library,
            overflow_checks: true,
            allowed_lints: AllowedLints::default(),
        }
    }
}

/// The result of running the pipeline on one source file.
#[derive(Debug)]
pub struct Compilation {
    pub source_map: SourceMap,
    /// The user's file in `source_map` (the `core` prelude's files follow it).
    pub file: sabiko_span::FileId,
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

    /// The diagnostics that have labels, all inside the `core` prelude's source (decision
    /// 90). A diagnostic about the user's program must point at the program, since the
    /// prelude's code means nothing to the user; the test harnesses assert this is empty
    /// for every compilation they make, where a check in the compiler itself could only
    /// crash it on the user's input.
    #[must_use]
    pub fn diagnostics_only_in_prelude(&self) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| !d.labels.is_empty() && d.labels.iter().all(|l| l.span.file != self.file))
            .collect()
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
        Emit::Tokens => tokens_output(file, src, &mut sink),
        Emit::None | Emit::Ast | Emit::Pretty | Emit::Mir | Emit::MirElaborated | Emit::Il => {
            let (ast, next_id) = parse_user_file(file, src, &mut sink);
            match options.emit {
                Emit::Ast => sabiko_ast::dump::dump(&ast),
                Emit::Pretty => sabiko_ast::pretty::pretty(&ast),
                _ => {
                    let mut output = String::new();
                    let program = (options.stop_after > Stage::Parse)
                        .then(|| resolve_parsed(ast, next_id, &mut source_map, &mut sink))
                        .flatten();
                    if let Some((ast, resolve)) = program {
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
                                check_user_mir(&bodies, &resolve, &typeck, &mut sink);
                                sabiko_mir::simplify::simplify_all(&mut bodies);
                                check_user_mir(&bodies, &resolve, &typeck, &mut sink);
                                for body in checked_bodies(&bodies, &resolve) {
                                    sabiko_borrowck::check_body(body, &resolve, &typeck, &mut sink);
                                }
                                if options.emit == Emit::Mir {
                                    output = sabiko_mir::pretty::print_user_bodies(
                                        &bodies, &resolve, &typeck,
                                    );
                                }
                                if options.emit == Emit::MirElaborated && !sink.has_errors() {
                                    // After borrow checking, which sees the drops as built
                                    // (decision 55), and on the instances of generic
                                    // functions (decision 82).
                                    bodies = sabiko_mir::mono::monomorphize(
                                        bodies, &resolve, &typeck, &mut sink,
                                    );
                                    sabiko_mir::elaborate::elaborate_all(&mut bodies, &typeck);
                                    check_mir(&bodies, &typeck, &mut sink);
                                    sabiko_mir::simplify::simplify_all(&mut bodies);
                                    check_mir(&bodies, &typeck, &mut sink);
                                    output = sabiko_mir::pretty::print_user_bodies(
                                        &bodies, &resolve, &typeck,
                                    );
                                }
                                if options.emit == Emit::Il && !sink.has_errors() {
                                    output = lint_and_emit_il(
                                        bodies,
                                        &resolve,
                                        &typeck,
                                        &source_map,
                                        name,
                                        options,
                                        &mut sink,
                                    );
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
        file,
        diagnostics,
        output,
    }
}

/// The result of the front end up to name resolution ([`parse_and_resolve`]).
#[derive(Debug)]
pub struct FrontEnd {
    pub file: sabiko_span::FileId,
    /// The user's file with the `core` prelude's items appended (decision 90), and its
    /// resolution; `None` when parsing reported errors.
    pub program: Option<(sabiko_ast::SourceFile, sabiko_resolve::ResolveResults)>,
}

/// Adds `src` to `source_map` under `name` and runs the front end on it up to name
/// resolution, exactly as compiling does: parses it, and only if that reported no errors
/// appends the `core` prelude and resolves both. Every error is in `sink`. For the
/// harnesses that drive the later stages themselves.
pub fn parse_and_resolve(
    name: &str,
    src: &str,
    source_map: &mut SourceMap,
    sink: &mut DiagnosticSink,
) -> FrontEnd {
    let file = source_map.add(name, src);
    let (ast, next_id) = parse_user_file(file, src, sink);
    let program = resolve_parsed(ast, next_id, source_map, sink);
    FrontEnd { file, program }
}

/// Parses the user's file; returns it with the first node id it did not use.
fn parse_user_file(
    file: sabiko_span::FileId,
    src: &str,
    sink: &mut DiagnosticSink,
) -> (sabiko_ast::SourceFile, u32) {
    sabiko_parser::parse_file_with(file, src, sabiko_parser::ParseOptions::default(), sink)
}

/// Appends the `core` prelude to the parsed user's file `ast` and resolves the program.
/// Semantic analysis only runs on a syntactically complete file (`None` when `sink` holds
/// errors): recovery drops broken constructs, and reporting "cannot find value" for names
/// defined inside them would only add noise.
fn resolve_parsed(
    mut ast: sabiko_ast::SourceFile,
    next_id: u32,
    source_map: &mut SourceMap,
    sink: &mut DiagnosticSink,
) -> Option<(sabiko_ast::SourceFile, sabiko_resolve::ResolveResults)> {
    if sink.has_errors() {
        return None;
    }
    let start = sabiko_parser::prelude::add_core_prelude(&mut ast, next_id, source_map, sink);
    let resolve = sabiko_resolve::resolve_program(&mut ast, start, sink);
    Some((ast, resolve))
}

/// The token stream for `--emit=tokens`, one per line.
fn tokens_output(file: sabiko_span::FileId, src: &str, sink: &mut DiagnosticSink) -> String {
    let tokens = sabiko_lexer::lex(file, src, sink);
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

/// The bodies borrow checking and the constant-propagation lints run on: the user's
/// program's, not the `core` prelude's (decision 90), as rustc never checks `core` again
/// when it compiles a program. The prelude's generic bodies are the same in every
/// compilation, and checking them would cost about as much as the rest of a
/// hello-world's compilation; [`check_core_prelude`] checks them once, in a test.
fn checked_bodies<'a>(
    bodies: &'a [sabiko_mir::Body],
    resolve: &'a sabiko_resolve::ResolveResults,
) -> impl Iterator<Item = &'a sabiko_mir::Body> + 'a {
    bodies.iter().filter(move |b| !resolve.is_prelude(b.def))
}

/// Borrow-checks and lints the `core` prelude's own bodies, which compiling a program
/// skips ([`checked_bodies`]): the pipeline up to the constant-propagation lints on an
/// empty program, keeping only the prelude's bodies, with the MIR validator after every
/// pass as usual. The prelude is the compiler's own code, so any diagnostic is a bug; a
/// test asserts there is none.
#[must_use]
pub fn check_core_prelude() -> Compilation {
    let mut source_map = SourceMap::new();
    let mut sink = DiagnosticSink::new();
    let front = parse_and_resolve("empty.sabi", "", &mut source_map, &mut sink);
    if let Some((ast, resolve)) = front.program {
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        let mut bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        check_mir(&bodies, &typeck, &mut sink);
        sabiko_mir::simplify::simplify_all(&mut bodies);
        check_mir(&bodies, &typeck, &mut sink);
        let lints = sabiko_mir::lint::LintOptions {
            overflow_checks: true,
            allowed: AllowedLints::default(),
        };
        for body in bodies.iter().filter(|b| resolve.is_prelude(b.def)) {
            sabiko_borrowck::check_body(body, &resolve, &typeck, &mut sink);
            sabiko_mir::lint::check_body(body, &typeck, &lints, &mut sink);
        }
    }
    let mut diagnostics = sink.into_vec();
    sabiko_diagnostics::sort(&mut diagnostics);
    Compilation {
        source_map,
        file: front.file,
        diagnostics,
        output: String::new(),
    }
}

/// The end of the pipeline for `--emit=il` (and assemblies): rustc's constant-propagation
/// lints, which rustc runs only when it generates code, then monomorphisation (decision
/// 82), the lowering passes and code generation. Empty when a step reports errors.
fn lint_and_emit_il(
    mut bodies: Vec<sabiko_mir::Body>,
    resolve: &sabiko_resolve::ResolveResults,
    typeck: &sabiko_types::TypeckResults,
    source_map: &SourceMap,
    name: &str,
    options: Options,
    sink: &mut DiagnosticSink,
) -> String {
    let lints = sabiko_mir::lint::LintOptions {
        overflow_checks: options.overflow_checks,
        allowed: options.allowed_lints,
    };
    for body in checked_bodies(&bodies, resolve) {
        sabiko_mir::lint::check_body(body, typeck, &lints, sink);
    }
    if sink.has_errors() {
        return String::new();
    }
    // Code is generated for the instances of generic functions, not for their generic
    // bodies (decision 82).
    bodies = sabiko_mir::mono::monomorphize(bodies, resolve, typeck, sink);
    check_mir(&bodies, typeck, sink);
    if sink.has_errors() {
        return String::new();
    }
    // After borrow checking, which sees the drops as built (decision 55) and needs every
    // copy MIR building made (decisions 36 and 51).
    let failures = sabiko_mir::lower_for_codegen(&mut bodies, typeck);
    report_invalid_mir(failures, sink);
    let codegen = sabiko_codegen::Options {
        assembly_name: assembly_name(name),
        kind: options.output_kind,
        overflow_checks: options.overflow_checks,
    };
    sabiko_codegen::emit_il(&bodies, resolve, typeck, source_map, &codegen, sink)
        .unwrap_or_default()
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

/// [`check_mir`] on the program's own bodies: the `core` prelude's are validated by
/// [`check_core_prelude`] (as they are borrow-checked, [`checked_bodies`]), and the
/// instances monomorphisation makes of them are validated with every other body.
fn check_user_mir(
    bodies: &[sabiko_mir::Body],
    resolve: &sabiko_resolve::ResolveResults,
    typeck: &sabiko_types::TypeckResults,
    sink: &mut DiagnosticSink,
) {
    let failures = checked_bodies(bodies, resolve)
        .flat_map(|body| {
            sabiko_mir::validate::validate_body(body, typeck)
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(move |m| format!("{}: {m}", body.name))
        })
        .collect();
    report_invalid_mir(failures, sink);
}

/// Runs the MIR validator and reports any violation as an internal compiler error. Debug
/// builds panic instead, so a broken pass fails its tests loudly.
fn check_mir(
    bodies: &[sabiko_mir::Body],
    typeck: &sabiko_types::TypeckResults,
    sink: &mut DiagnosticSink,
) {
    report_invalid_mir(sabiko_mir::validate::validate_all(bodies, typeck), sink);
}

/// Reports the validator's complaints as an internal compiler error (debug builds panic).
fn report_invalid_mir(failures: Vec<String>, sink: &mut DiagnosticSink) {
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

    /// The prelude's bodies pass borrow checking and the lints, which compiling a program
    /// skips (decision 90).
    #[test]
    fn core_prelude_borrow_checks_and_lints_clean() {
        let c = check_core_prelude();
        assert!(
            c.diagnostics.is_empty(),
            "{}",
            c.render_diagnostics(Format::HumanPlain)
        );
    }

    #[test]
    fn error_count_ignores_warnings() {
        let c = Compilation {
            source_map: SourceMap::new(),
            file: sabiko_span::FileId(0),
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
            file: sabiko_span::FileId(0),
            diagnostics: vec![Diagnostic::warning(ErrorCode::UNSUPPORTED_SYNTAX, "w")],
            output: String::new(),
        };
        assert_eq!(clean.error_count(), 0);
        assert!(!clean.has_errors());
    }
}
