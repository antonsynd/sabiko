//! Diagnostics: stable error codes, structured messages with labelled spans, and
//! rendering to human-readable text (via `ariadne`) or JSON.
//!
//! Every diagnostic the compiler emits carries an [`ErrorCode`]. Codes are stable
//! identifiers that tests (`//~ ERROR E0005`) and users key on; the message text may
//! change, the code never does.

use std::fmt::{self, Write as _};

pub use sabiko_span::SourceMap;
use sabiko_span::Span;
use serde::Serialize;

/// A stable, user-facing error code such as `E0005`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct ErrorCode(pub u16);

impl ErrorCode {
    // Lexer (E00xx)
    pub const UNEXPECTED_CHARACTER: Self = Self(1);
    pub const UNTERMINATED_STRING: Self = Self(2);
    pub const UNTERMINATED_BLOCK_COMMENT: Self = Self(3);
    pub const INVALID_INTEGER_LITERAL: Self = Self(4);
    pub const INVALID_CHAR_LITERAL: Self = Self(5);
    pub const INVALID_ESCAPE: Self = Self(6);
    pub const INVALID_FLOAT_LITERAL: Self = Self(7);

    // Parser (E01xx)
    pub const UNEXPECTED_TOKEN: Self = Self(100);
    pub const EXPECTED_ITEM: Self = Self(101);
    pub const EXPECTED_EXPRESSION: Self = Self(102);
    pub const EXPECTED_TYPE: Self = Self(103);
    pub const CHAINED_COMPARISON: Self = Self(104);
    pub const UNEXPECTED_EOF: Self = Self(105);
    pub const EXPECTED_PATTERN: Self = Self(106);
    pub const UNSUPPORTED_SYNTAX: Self = Self(107);
    pub const STRUCT_LITERAL_NOT_ALLOWED: Self = Self(108);
    pub const NESTING_TOO_DEEP: Self = Self(109);

    // Name resolution (E02xx)
    pub const DUPLICATE_DEFINITION: Self = Self(200);
    pub const UNRESOLVED_TYPE: Self = Self(201);
    pub const UNRESOLVED_VALUE: Self = Self(202);
    pub const WRONG_NAMESPACE: Self = Self(203);
    pub const DUPLICATE_FIELD_DEFINITION: Self = Self(204);
    pub const DUPLICATE_PARAMETER: Self = Self(205);
    pub const RECURSIVE_TYPE: Self = Self(206);
    pub const PLACEHOLDER_IN_SIGNATURE: Self = Self(207);
    pub const UNSIZED_TYPE: Self = Self(208);
    pub const DUPLICATE_BINDING: Self = Self(209);

    // Type checking (E03xx)
    pub const TYPE_MISMATCH: Self = Self(300);
    pub const UNKNOWN_FIELD: Self = Self(301);
    pub const MISSING_FIELDS: Self = Self(302);
    pub const DUPLICATE_FIELD_INIT: Self = Self(303);
    pub const WRONG_ARG_COUNT: Self = Self(304);
    pub const NOT_CALLABLE: Self = Self(305);
    pub const INVALID_BINARY_OP: Self = Self(306);
    pub const INVALID_UNARY_OP: Self = Self(307);
    pub const INVALID_CAST: Self = Self(308);
    pub const INVALID_DEREF: Self = Self(309);
    pub const INVALID_ASSIGN_TARGET: Self = Self(310);
    pub const BREAK_OUTSIDE_LOOP: Self = Self(311);
    pub const BREAK_WITH_VALUE_OUTSIDE_LOOP: Self = Self(312);
    pub const TYPE_ANNOTATIONS_NEEDED: Self = Self(313);
    pub const LITERAL_OUT_OF_RANGE: Self = Self(314);
    pub const NON_CONSTANT_EXPRESSION: Self = Self(315);
    pub const CONST_EVAL_FAILED: Self = Self(316);
    pub const FN_ITEM_AS_VALUE: Self = Self(317);
    /// A `char` literal outside the Basic Multilingual Plane: `char` is one UTF-16 code
    /// unit (decision 31).
    pub const NON_BMP_CHAR: Self = Self(318);

    // Borrow checking (E04xx)
    pub const ASSIGN_TWICE_IMMUTABLE: Self = Self(400);
    pub const ASSIGN_TO_IMMUTABLE: Self = Self(401);
    pub const BORROW_MUT_IMMUTABLE: Self = Self(402);
    pub const ASSIGN_THROUGH_SHARED_REF: Self = Self(403);
    pub const BORROW_MUT_THROUGH_SHARED_REF: Self = Self(404);
    pub const USE_OF_UNINITIALIZED: Self = Self(405);
    pub const USE_OF_MOVED_VALUE: Self = Self(406);
    pub const MOVE_OUT_OF_BORROW: Self = Self(407);
    pub const BORROW_CONFLICT: Self = Self(408);
    pub const MULTIPLE_MUT_BORROWS: Self = Self(409);
    pub const ASSIGN_TO_BORROWED: Self = Self(410);
    pub const USE_WHILE_MUTABLY_BORROWED: Self = Self(411);
    pub const MOVE_WHILE_BORROWED: Self = Self(412);
    pub const DOES_NOT_LIVE_LONG_ENOUGH: Self = Self(413);
    pub const RETURN_REF_TO_LOCAL: Self = Self(414);
    pub const LIFETIME_TOO_SHORT: Self = Self(415);
    pub const MISSING_LIFETIME_SPECIFIER: Self = Self(416);
    pub const COPY_NOT_IMPLEMENTABLE: Self = Self(417);

    // Code generation (E05xx)
    /// A construct the CIL backend cannot lower yet.
    pub const UNSUPPORTED_CODEGEN: Self = Self(500);
    /// `Main` has parameters or a non-`()` return type, or an executable has no `Main`.
    pub const INVALID_MAIN: Self = Self(501);

    // Internal errors (E09xx)
    pub const INTERNAL_ERROR: Self = Self(900);
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "E{:04}", self.0)
    }
}

impl From<ErrorCode> for String {
    fn from(c: ErrorCode) -> Self {
        c.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

/// A labelled span inside a diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: String,
    /// The primary label is the one the diagnostic is "about"; secondary labels give context.
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: ErrorCode,
    pub message: String,
    pub labels: Vec<Label>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    #[must_use]
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            labels: vec![],
            notes: vec![],
        }
    }

    #[must_use]
    pub fn warning(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message: message.into(),
            labels: vec![],
            notes: vec![],
        }
    }

    #[must_use]
    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        let primary = self.labels.is_empty();
        self.labels.push(Label {
            span,
            message: message.into(),
            primary,
        });
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// The span of the primary label, if any.
    #[must_use]
    pub fn primary_span(&self) -> Option<Span> {
        self.labels.iter().find(|l| l.primary).map(|l| l.span)
    }
}

/// Collects diagnostics during a compilation pass.
#[derive(Debug, Default)]
pub struct DiagnosticSink {
    diagnostics: Vec<Diagnostic>,
}

impl DiagnosticSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    pub fn extend(&mut self, other: DiagnosticSink) {
        self.diagnostics.extend(other.diagnostics);
    }
}

/// Sorts diagnostics by file, then primary span start, for deterministic output.
pub fn sort(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by_key(|d| d.primary_span().map(|s| (s.file, s.start, s.end)));
}

/// How to render diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// ariadne-rendered text with ANSI colours.
    Human,
    /// ariadne-rendered text without colours (used by golden tests).
    HumanPlain,
    /// One JSON object per line.
    Json,
}

#[derive(Serialize)]
struct JsonLabel<'a> {
    file: &'a str,
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
    message: &'a str,
    primary: bool,
}

#[derive(Serialize)]
struct JsonDiagnostic<'a> {
    severity: Severity,
    code: ErrorCode,
    message: &'a str,
    labels: Vec<JsonLabel<'a>>,
    notes: &'a [String],
}

/// Renders diagnostics to a string.
#[must_use]
pub fn render(diagnostics: &[Diagnostic], sm: &SourceMap, format: Format) -> String {
    let mut out = String::new();
    match format {
        Format::Json => {
            for d in diagnostics {
                render_json(d, sm, &mut out);
            }
        }
        Format::Human | Format::HumanPlain => {
            // ariadne wants a cache of `Source`s it can borrow from; build one per render
            // call, keyed by file name, borrowing the text from the source map.
            let mut cache = ariadne::sources(sm.files().map(|f| (f.name.clone(), f.src.as_str())));
            for d in diagnostics {
                render_human(d, sm, &mut cache, format == Format::Human, &mut out);
            }
        }
    }
    out
}

fn render_json(d: &Diagnostic, sm: &SourceMap, out: &mut String) {
    let labels = d
        .labels
        .iter()
        .map(|l| {
            let f = sm.get(l.span.file);
            let s = f.line_col(l.span.start);
            let e = f.line_col(l.span.end);
            JsonLabel {
                file: &f.name,
                line: s.line,
                col: s.col,
                end_line: e.line,
                end_col: e.col,
                message: &l.message,
                primary: l.primary,
            }
        })
        .collect();
    let j = JsonDiagnostic {
        severity: d.severity,
        code: d.code,
        message: &d.message,
        labels,
        notes: &d.notes,
    };
    out.push_str(&serde_json::to_string(&j).expect("diagnostic serialises"));
    out.push('\n');
}

fn render_human(
    d: &Diagnostic,
    sm: &SourceMap,
    cache: &mut impl ariadne::Cache<String>,
    color: bool,
    out: &mut String,
) {
    use ariadne::{Color, Config, Report, ReportKind};

    let kind = match d.severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
    };
    let Some(primary) = d.primary_span() else {
        // Spanless diagnostic: a plain line.
        let _ = writeln!(out, "{}[{}]: {}", kind, d.code, d.message);
        for n in &d.notes {
            let _ = writeln!(out, "  = note: {n}");
        }
        return;
    };
    let file_name = sm.get(primary.file).name.clone();
    let mut report = Report::build(kind, (file_name.clone(), primary.range()))
        .with_code(d.code.to_string())
        .with_message(&d.message)
        .with_config(
            Config::default()
                .with_color(color)
                .with_index_type(ariadne::IndexType::Byte),
        );
    for l in &d.labels {
        let name = sm.get(l.span.file).name.clone();
        let mut label = ariadne::Label::new((name, l.span.range())).with_message(&l.message);
        if color {
            label = label.with_color(if l.primary { Color::Red } else { Color::Blue });
        }
        if !l.primary {
            label = label.with_order(1);
        }
        report = report.with_label(label);
    }
    for n in &d.notes {
        report = report.with_note(n);
    }
    let mut buf = Vec::new();
    report
        .finish()
        .write(&mut *cache, &mut buf)
        .expect("writing to a Vec cannot fail");
    out.push_str(&String::from_utf8(buf).expect("ariadne emits UTF-8"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_formats_with_four_digits() {
        assert_eq!(ErrorCode::UNEXPECTED_TOKEN.to_string(), "E0100");
        assert_eq!(ErrorCode(1).to_string(), "E0001");
    }

    #[test]
    fn renders_plain_and_json() {
        let mut sm = SourceMap::new();
        let id = sm.add("t.sabi", "let x = @;\n");
        let d = Diagnostic::error(ErrorCode::UNEXPECTED_CHARACTER, "unexpected character `@`")
            .with_label(Span::new(id, 8, 9), "not valid here")
            .with_note("Sabiko has no `@` operator");
        let plain = render(std::slice::from_ref(&d), &sm, Format::HumanPlain);
        assert!(plain.contains("E0001"), "{plain}");
        assert!(plain.contains("unexpected character"), "{plain}");
        assert!(plain.contains("not valid here"), "{plain}");
        assert!(
            !plain.contains("\u{1b}["),
            "plain output must have no ANSI escapes: {plain}"
        );
        let json = render(&[d], &sm, Format::Json);
        assert!(json.starts_with('{') && json.ends_with('\n'));
        assert!(json.contains("\"code\":\"E0001\""), "{json}");
        assert!(json.contains("\"line\":1,\"col\":9"), "{json}");
    }
}
