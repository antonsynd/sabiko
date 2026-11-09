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
    /// A `self` parameter outside an `impl` block, or not first in the parameter list.
    pub const MISPLACED_SELF_PARAM: Self = Self(110);
    /// `_` as an expression anywhere but the left of an assignment (rustc reports it
    /// without a code).
    pub const MISPLACED_UNDERSCORE: Self = Self(111);
    /// A struct literal ending in `..` with no base expression, other than on the left
    /// of an assignment (rustc's E0797).
    pub const MISSING_STRUCT_BASE: Self = Self(112);
    /// A second `..` in a tuple or tuple-variant pattern (rustc reports it without a code).
    pub const MULTIPLE_REST_PATTERNS: Self = Self(113);

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
    /// Two associated items of one name for the same type, in one `impl` block or in two
    /// (rustc's E0592).
    pub const DUPLICATE_ASSOC_ITEM: Self = Self(210);
    /// An inherent `impl` for something other than a struct of this file (rustc's E0390
    /// for primitives).
    pub const INVALID_IMPL_TARGET: Self = Self(211);
    /// `Type::Name` where the type has no associated function `Name` (rustc's E0599 for
    /// paths).
    pub const UNKNOWN_ASSOC_ITEM: Self = Self(212);
    /// `self` used as a value outside a method with a `self` parameter (rustc's E0424).
    pub const SELF_OUTSIDE_METHOD: Self = Self(213);
    /// `impl Drop` for something other than a struct of the file (rustc's E0120, E0117).
    pub const INVALID_DROP_TARGET: Self = Self(214);
    /// Two implementations of one trait whose types overlap: a second `impl Drop` for the
    /// same struct, `impl Show for i32` twice, `impl<T> Show for T` beside `impl Show for
    /// i32` (rustc's E0119).
    pub const CONFLICTING_IMPLS: Self = Self(215);
    /// A trait impl without every required method of its trait, `impl Drop` without `fn
    /// drop` (rustc's E0046).
    pub const MISSING_TRAIT_ITEMS: Self = Self(216);
    /// An item of a trait impl its trait does not declare, of an `impl Drop` other than
    /// `drop` (rustc's E0407, E0438).
    pub const NOT_A_TRAIT_MEMBER: Self = Self(217);
    /// A name bound in one alternative of an or-pattern but not in another (rustc's
    /// E0408).
    pub const OR_PATTERN_BINDING_MISSING: Self = Self(218);
    /// A name bound by reference in one alternative of an or-pattern and by value (or with
    /// another mutability) in another (rustc's E0409).
    pub const OR_PATTERN_BINDING_MODE: Self = Self(219);
    /// A local or parameter used in an array length or repeat count (rustc's E0435).
    pub const NON_CONSTANT_VALUE: Self = Self(220);
    /// Two generic parameters of one name (rustc's E0403).
    pub const DUPLICATE_GENERIC_PARAM: Self = Self(221);
    /// The wrong number of generic arguments for a generic function (rustc's E0107).
    pub const WRONG_GENERIC_ARG_COUNT: Self = Self(222);
    /// Generic arguments on a path segment that takes none: a primitive type, a type
    /// parameter, a local, a non-generic item (rustc's E0109).
    pub const GENERIC_ARGS_NOT_ALLOWED: Self = Self(223);

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
    /// `e.Name(..)` where the receiver's type has no method `Name` (rustc's E0599).
    pub const UNKNOWN_METHOD: Self = Self(319);
    /// `e.Name` naming a method, not a field (rustc's E0615).
    pub const METHOD_AS_FIELD: Self = Self(320);
    /// A method of a trait impl with another receiver, parameter or return type than its
    /// trait declares, `drop` other than `fn drop(&mut self)` (rustc's E0053, E0185,
    /// E0186).
    pub const IMPL_SIGNATURE_MISMATCH: Self = Self(321);
    /// A method of a trait impl with another number of parameters than its trait declares,
    /// `drop` with parameters besides `&mut self` (rustc's E0050).
    pub const IMPL_PARAM_COUNT: Self = Self(322);
    /// `d.drop()` or `D::drop(&mut d)` (rustc's E0040).
    pub const EXPLICIT_DROP_CALL: Self = Self(323);
    /// `#[derive(Copy)]` on a type that implements `Drop` (rustc's E0184).
    pub const COPY_WITH_DESTRUCTOR: Self = Self(324);
    /// A binding named like a variant of the enum it matches (rustc's deny-by-default
    /// E0170): it binds instead of comparing.
    pub const BINDING_NAMED_LIKE_VARIANT: Self = Self(325);
    /// A pattern or constructor of the wrong kind for its variant or struct: `V(x)` for a
    /// unit or struct variant, `V` for a tuple variant, `V()` for a unit variant (rustc's
    /// E0532, E0164, E0533, E0618).
    pub const VARIANT_SHAPE_MISMATCH: Self = Self(326);
    /// A tuple-variant pattern with the wrong number of fields (rustc's E0023).
    pub const PATTERN_ARITY: Self = Self(327);
    /// A `match` that does not cover every value of its scrutinee (rustc's E0004).
    pub const NON_EXHAUSTIVE_MATCH: Self = Self(328);
    /// A refutable pattern in a `let` or a parameter (rustc's E0005).
    pub const REFUTABLE_PATTERN: Self = Self(329);
    /// Warning: a pattern no value can reach (rustc's `unreachable_patterns` lint).
    pub const UNREACHABLE_PATTERN: Self = Self(330);
    /// Warning: an `if let`, `while let` or `let`-`else` whose pattern always matches
    /// (rustc's `irrefutable_let_patterns` lint).
    pub const IRREFUTABLE_LET_PATTERN: Self = Self(331);
    /// A range pattern whose lower bound exceeds its upper bound (rustc's E0030), or an
    /// empty exclusive range (E0579).
    pub const INVALID_RANGE_PATTERN: Self = Self(332);
    /// Two variants with the same discriminant (rustc's E0081), an implicit discriminant
    /// that overflows (E0370), or an explicit discriminant on an enum with a field (E0732).
    pub const INVALID_DISCRIMINANT: Self = Self(333);
    /// The operand of `[x; N]` (`N` neither 0 nor 1) is not `Copy` (rustc's E0277).
    pub const REPEAT_NEEDS_COPY: Self = Self(334);
    /// An index of a type other than `usize` (rustc's E0277, "the type `[T]` cannot be
    /// indexed by `i32`").
    pub const INVALID_INDEX_TYPE: Self = Self(335);
    /// Indexing a value that is not an array or a slice (rustc's E0608).
    pub const NOT_INDEXABLE: Self = Self(336);
    /// `mut`, `ref` or `ref mut` on a binding whose default binding mode is a reference
    /// (edition 2024; rustc's uncoded "cannot mutably bind by value / cannot explicitly
    /// borrow within an implicitly-borrowing pattern").
    pub const BINDING_MODIFIER_UNDER_REF: Self = Self(337);
    /// A struct's or enum's type parameter that no field uses (rustc's E0392).
    pub const UNUSED_TYPE_PARAMETER: Self = Self(340);
    /// A type that does not implement a trait a bound requires: a generic argument, a
    /// receiver, the type of a trait impl with a supertrait (rustc's E0277).
    pub const UNSATISFIED_BOUND: Self = Self(341);
    /// A method call or `Type::Name` path that more than one trait's method matches
    /// (rustc's E0034).
    pub const AMBIGUOUS_METHOD: Self = Self(342);
    /// A method of a trait impl with another number of generic parameters than its trait
    /// declares (rustc's E0049).
    pub const IMPL_GENERIC_COUNT: Self = Self(343);
    /// A method of a trait impl whose generic parameters have bounds its trait's do not
    /// (rustc's E0276).
    pub const IMPL_STRICTER_BOUNDS: Self = Self(344);
    /// An array length or repeat count that depends on a type parameter, through a trait's
    /// associated constant (rustc's uncoded "generic parameters may not be used in const
    /// operations").
    pub const GENERIC_CONST_EXPR: Self = Self(345);
    /// A constant pattern that depends on a type parameter, through a trait's associated
    /// constant (rustc's E0158).
    pub const GENERIC_CONST_PATTERN: Self = Self(346);
    /// `Trait::N` naming a trait's associated constant without the type implementing it
    /// (rustc's E0790).
    pub const TRAIT_CONST_WITHOUT_TYPE: Self = Self(347);
    /// An `impl Drop` block for a type other than its struct's or enum's own parameters
    /// in order (rustc's E0366, which allows any order).
    pub const DROP_IMPL_SPECIALIZED: Self = Self(348);
    /// An `impl Drop` block with bounds (rustc's E0367).
    pub const DROP_IMPL_BOUNDS: Self = Self(349);

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
    /// A non-`Copy` field moved out of a value whose type implements `Drop` (rustc's E0509).
    pub const MOVE_OUT_OF_DROP: Self = Self(418);
    /// Assigning or mutably borrowing a scrutinee inside a match guard (rustc's E0510).
    pub const MUTATE_IN_GUARD: Self = Self(419);
    /// Moving a non-`Copy` value out of an array or slice element (rustc's E0508).
    pub const MOVE_OUT_OF_INDEX: Self = Self(420);

    // Code generation (E05xx)
    /// A construct the CIL backend cannot lower yet.
    pub const UNSUPPORTED_CODEGEN: Self = Self(500);
    /// `Main` has parameters or a non-`()` return type, or an executable has no `Main`.
    pub const INVALID_MAIN: Self = Self(501);
    /// Monomorphisation instantiated a generic function at ever deeper types (rustc's
    /// "reached the recursion limit while instantiating").
    pub const INSTANTIATION_RECURSION_LIMIT: Self = Self(502);
    /// Evaluating a trait's constant failed at the type an instance of a generic function
    /// uses it at (rustc's E0080, reported when rustc generates code): E0316, found by
    /// monomorphisation (decision 85).
    pub const INSTANCE_CONST_EVAL_FAILED: Self = Self(503);

    // Lints on MIR (E06xx): rustc's deny-by-default constant-propagation lints, errors here
    // since Sabiko has no `#[allow]` (decision 73); `sabikoc -A <lint>` turns them off.
    /// An arithmetic operation on values known at compile time overflows (rustc's
    /// `arithmetic_overflow` lint).
    pub const ARITHMETIC_OVERFLOW: Self = Self(600);
    /// An operation on values known at compile time panics: an index out of bounds, a
    /// division or remainder by zero, `MIN / -1` (rustc's `unconditional_panic` lint).
    pub const UNCONDITIONAL_PANIC: Self = Self(601);

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
        // A secondary label at a definition without source text (a builtin, a language
        // trait or its method, decision 86) points nowhere: leave it out.
        if span.is_dummy() && !self.labels.is_empty() {
            return self;
        }
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
