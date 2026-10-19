//! Drives the compilation pipeline. Phase 1 runs lexing and parsing; later phases add
//! resolution, type checking, borrow checking and code generation behind the same entry
//! points.

use std::fmt::Write as _;
use std::path::Path;

use sabiko_diagnostics::{Diagnostic, Format, SourceMap};

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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub emit: Emit,
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
            .any(|d| d.severity == sabiko_diagnostics::Severity::Error)
    }

    #[must_use]
    pub fn render_diagnostics(&self, format: Format) -> String {
        sabiko_diagnostics::render(&self.diagnostics, &self.source_map, format)
    }
}

/// Compiles source text given under `name` (used in diagnostics).
#[must_use]
pub fn compile_str(name: &str, src: &str, options: Options) -> Compilation {
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
        Emit::None | Emit::Ast | Emit::Pretty => {
            let ast = sabiko_parser::parse_file(file, src, &mut sink);
            match options.emit {
                Emit::Ast => sabiko_ast::dump::dump(&ast),
                Emit::Pretty => sabiko_ast::pretty::pretty(&ast),
                _ => String::new(),
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

/// Reads and compiles a file from disk.
pub fn compile_file(path: &Path, options: Options) -> std::io::Result<Compilation> {
    let src = std::fs::read_to_string(path)?;
    Ok(compile_str(&path.display().to_string(), &src, options))
}
