//! Parser for Sabiko: tokens in, [`SourceFile`] out.
//!
//! Hand-written recursive descent for items, statements, patterns and types; a Pratt
//! loop for binary expressions. Errors never abort the parse: each item and each
//! statement is a recovery boundary, so one mistake yields one diagnostic and the rest of
//! the file is still checked. Rust's two context rules are implemented: a block-like
//! expression (`if`, `while`, `loop`, `{}`) at statement position is a complete statement
//! without `;`, and struct literals are not allowed bare in `if`/`while` conditions.
//!
//! Nesting is limited to [`MAX_NESTING`] levels. Every later stage recurses over the
//! tree, so the limit is what keeps the whole pipeline within its stack.

mod expr;
mod item;
mod recovery;

use sabiko_ast::{NodeId, SourceFile};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_lexer::{Token, TokenKind};
use sabiko_span::{FileId, Span};

/// Marker for "a diagnostic has already been emitted; unwind to the nearest recovery
/// point". Carries no data so that `?` stays cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError;

pub type PResult<T> = Result<T, ParseError>;

/// How deeply expressions, blocks and types may nest (clang's default bracket depth).
/// One level is a nested expression (parentheses, a block's statement, a call argument,
/// an `else if`), a prefix operator, a binary operator or cast in a chain, a postfix
/// call, index or field access, or a `&` in a type; `&&` counts twice. Exceeding it is
/// fatal (E0109).
pub const MAX_NESTING: u32 = 256;

/// Parses a whole source file. Lexical and syntactic errors go to `sink`; the returned
/// tree contains everything that could be parsed.
#[must_use]
pub fn parse_file(file: FileId, src: &str, sink: &mut DiagnosticSink) -> SourceFile {
    let before = sink.diagnostics().len();
    let tokens = sabiko_lexer::lex(file, src, sink);
    // An unterminated string or comment swallows the rest of the file; parsing what is
    // left would only produce misleading follow-on errors (rustc treats these as fatal).
    let fatal = sink.diagnostics()[before..].iter().any(|d| {
        matches!(
            d.code,
            ErrorCode::UNTERMINATED_STRING | ErrorCode::UNTERMINATED_BLOCK_COMMENT
        )
    });
    if fatal {
        let end = u32::try_from(src.len()).expect("source file larger than 4 GiB");
        return SourceFile {
            docs: vec![],
            items: vec![],
            span: Span::new(file, 0, end),
        };
    }
    let mut p = Parser::new(file, src, tokens, sink);
    p.parse_source_file()
}

/// Parses a single expression (used by tests and the REPL-style `--emit=ast` on snippets).
pub fn parse_expr(file: FileId, src: &str, sink: &mut DiagnosticSink) -> PResult<sabiko_ast::Expr> {
    let tokens = sabiko_lexer::lex(file, src, sink);
    let mut p = Parser::new(file, src, tokens, sink);
    let e = p.parse_expr()?;
    p.expect(TokenKind::Eof)?;
    Ok(e)
}

pub(crate) struct Parser<'s, 'd> {
    file: FileId,
    src: &'s str,
    tokens: Vec<Token>,
    pos: usize,
    sink: &'d mut DiagnosticSink,
    next_id: u32,
    /// When set, a path followed by `{` is not a struct literal (inside `if`/`while`
    /// conditions).
    no_struct_literal: bool,
    /// Set by the statement parser before its expression: a block-like expression found
    /// first ends the statement (rustc's `STMT_EXPR` restriction), so `{ a } - 1` is two
    /// statements while `{ a }.Foo()` is one.
    stmt_start: bool,
    /// Position of the token the last error was reported at, to avoid piling several
    /// diagnostics onto one token during recovery.
    last_error_pos: Option<usize>,
    /// Current nesting level, checked against [`MAX_NESTING`].
    depth: u32,
}

impl<'s, 'd> Parser<'s, 'd> {
    fn new(file: FileId, src: &'s str, tokens: Vec<Token>, sink: &'d mut DiagnosticSink) -> Self {
        debug_assert_eq!(tokens.last().map(|t| t.kind), Some(TokenKind::Eof));
        Self {
            file,
            src,
            tokens,
            pos: 0,
            sink,
            next_id: 0,
            no_struct_literal: false,
            stmt_start: false,
            last_error_pos: None,
            depth: 0,
        }
    }

    // ---- token access -------------------------------------------------------------------

    pub(crate) fn peek(&self) -> Token {
        self.tokens[self.pos]
    }

    pub(crate) fn peek_kind(&self) -> TokenKind {
        self.tokens[self.pos].kind
    }

    pub(crate) fn nth_kind(&self, n: usize) -> TokenKind {
        self.tokens
            .get(self.pos + n)
            .map_or(TokenKind::Eof, |t| t.kind)
    }

    pub(crate) fn at(&self, kind: TokenKind) -> bool {
        self.peek_kind() == kind
    }

    pub(crate) fn at_eof(&self) -> bool {
        self.at(TokenKind::Eof)
    }

    pub(crate) fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos];
        if t.kind != TokenKind::Eof {
            self.pos += 1;
        }
        t
    }

    pub(crate) fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        if self.at(kind) {
            Some(self.bump())
        } else {
            None
        }
    }

    pub(crate) fn expect(&mut self, kind: TokenKind) -> PResult<Token> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            Err(self.error_expected(kind.describe()))
        }
    }

    pub(crate) fn text(&self, token: Token) -> &'s str {
        &self.src[token.span.range()]
    }

    /// Span of the most recently consumed token.
    pub(crate) fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.peek().span
        } else {
            self.tokens[self.pos - 1].span
        }
    }

    pub(crate) fn new_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    // ---- nesting -------------------------------------------------------------------------

    /// Enters one more level of nesting; the caller restores the depth with
    /// [`Self::keep_depth`] or [`Self::nested`].
    pub(crate) fn descend(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth <= MAX_NESTING {
            return Ok(());
        }
        let span = self.peek().span;
        let d = Diagnostic::error(
            ErrorCode::NESTING_TOO_DEEP,
            format!("nesting exceeds the limit of {MAX_NESTING} levels"),
        )
        .with_label(span, "nested too deeply here")
        .with_note("move the inner part into a `let` binding or a separate function");
        let err = self.emit(d);
        // Fatal: recovery would only re-enter the same nesting. Jump to the end so that
        // every enclosing construct stops at once, silently (the end counts as reported).
        self.pos = self.tokens.len() - 1;
        self.last_error_pos = Some(self.pos);
        Err(err)
    }

    /// Runs `f` one level deeper.
    pub(crate) fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.keep_depth(|p| {
            p.descend()?;
            f(p)
        })
    }

    /// Runs `f` and restores the depth afterwards, for the loops that build left-nested
    /// trees (`a + b + c`, `x.F().G()`) and descend once per iteration.
    pub(crate) fn keep_depth<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        let saved = self.depth;
        let r = f(self);
        self.depth = saved;
        r
    }

    // ---- diagnostics ---------------------------------------------------------------------

    /// Emits `d` unless an error was already reported at the current token.
    pub(crate) fn emit(&mut self, d: Diagnostic) -> ParseError {
        if self.last_error_pos != Some(self.pos) {
            self.last_error_pos = Some(self.pos);
            self.sink.emit(d);
        }
        ParseError
    }

    /// "expected X, found Y" at the current token.
    pub(crate) fn error_expected(&mut self, expected: &str) -> ParseError {
        let tok = self.peek();
        let d = if tok.kind == TokenKind::Eof {
            Diagnostic::error(
                ErrorCode::UNEXPECTED_EOF,
                format!("expected {expected}, found end of file"),
            )
            .with_label(tok.span, format!("expected {expected} here"))
        } else {
            Diagnostic::error(
                ErrorCode::UNEXPECTED_TOKEN,
                format!("expected {expected}, found {}", self.describe_current()),
            )
            .with_label(tok.span, format!("expected {expected} here"))
        };
        self.emit(d)
    }

    /// Reports syntax that is valid Rust but not yet part of Sabiko.
    pub(crate) fn error_unsupported(&mut self, what: &str, span: Span, phase: &str) -> ParseError {
        let d = Diagnostic::error(
            ErrorCode::UNSUPPORTED_SYNTAX,
            format!("not yet supported: {what}"),
        )
        .with_label(span, "not supported in this version of Sabiko")
        .with_note(format!("{phase}; see docs/PLAN.md"));
        self.emit(d)
    }

    /// "expected `;`, found X", pointing at the end of the previous token (where the `;`
    /// belongs) and secondarily at the token found instead.
    pub(crate) fn error_expected_after_prev(&mut self, expected: &str) -> ParseError {
        let tok = self.peek();
        let at = Span::point(self.file, self.prev_span().end);
        let d = Diagnostic::error(
            ErrorCode::UNEXPECTED_TOKEN,
            format!("expected {expected}, found {}", self.describe_current()),
        )
        .with_label(at, format!("expected {expected} here"))
        .with_label(tok.span, "unexpected token");
        self.emit(d)
    }

    /// Describes the current token for diagnostics, quoting identifiers and literals.
    fn describe_current(&self) -> String {
        let tok = self.peek();
        match tok.kind {
            TokenKind::Ident => format!("identifier `{}`", self.text(tok)),
            TokenKind::Int
            | TokenKind::Float
            | TokenKind::Str
            | TokenKind::Char
            | TokenKind::Lifetime => {
                format!("{} `{}`", tok.kind.describe(), self.text(tok))
            }
            k => k.describe().to_owned(),
        }
    }

    pub(crate) fn span_from(&self, start: Span) -> Span {
        start.to(self.prev_span())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sabiko_ast::dump::dump_expr;

    fn expr(src: &str) -> String {
        let mut sink = DiagnosticSink::new();
        let e = parse_expr(FileId(0), src, &mut sink).expect("parses");
        assert!(!sink.has_errors(), "{:?}", sink.diagnostics());
        dump_expr(&e)
    }

    #[test]
    fn precedence_climbing() {
        assert_eq!(
            expr("1 + 2 * 3"),
            "Binary +\n  Lit 1\n  Binary *\n    Lit 2\n    Lit 3\n"
        );
        assert_eq!(
            expr("(1 + 2) * 3"),
            "Binary *\n  Paren\n    Binary +\n      Lit 1\n      Lit 2\n  Lit 3\n"
        );
        assert_eq!(
            expr("a - b - c"),
            "Binary -\n  Binary -\n    Path a\n    Path b\n  Path c\n"
        );
        assert_eq!(
            expr("-x as u32"),
            "Cast\n  Unary -\n    Path x\n  Type u32\n"
        );
        assert_eq!(expr("&mut p.X"), "RefMut\n  Field X\n    Path p\n");
        assert_eq!(expr("*r = 5"), "Assign\n  Deref\n    Path r\n  Lit 5\n");
        assert_eq!(expr("a || b && c == d | e"), "Binary ||\n  Path a\n  Binary &&\n    Path b\n    Binary ==\n      Path c\n      Binary |\n        Path d\n        Path e\n");
    }

    #[test]
    fn chained_comparison_is_rejected() {
        let mut sink = DiagnosticSink::new();
        let _ = parse_expr(FileId(0), "a < b < c", &mut sink);
        assert_eq!(sink.diagnostics()[0].code, ErrorCode::CHAINED_COMPARISON);
    }

    #[test]
    fn block_like_statements_end_without_semicolon() {
        let mut sink = DiagnosticSink::new();
        let f = parse_file(
            FileId(0),
            "fn F() { if a { } - 1; { b }.Foo(); loop { } }",
            &mut sink,
        );
        // `if a {} - 1;` is a complete `if` statement followed by `-1;` (a separate statement).
        let body = match &f.items[0].kind {
            sabiko_ast::ItemKind::Fn(f) => &f.body,
            _ => unreachable!(),
        };
        assert!(!sink.has_errors(), "{:?}", sink.diagnostics());
        assert_eq!(body.stmts.len(), 3);
        assert!(body.tail.is_some());
        assert!(
            matches!(&body.stmts[1].kind, sabiko_ast::StmtKind::Expr { expr, semi: true }
            if matches!(expr.kind, sabiko_ast::ExprKind::Unary(..)))
        );
        assert!(
            matches!(&body.stmts[2].kind, sabiko_ast::StmtKind::Expr { expr, semi: true }
            if matches!(expr.kind, sabiko_ast::ExprKind::MethodCall { .. }))
        );
    }

    #[test]
    fn double_ampersand_in_prefix_position_is_two_borrows() {
        assert_eq!(expr("&&x"), "Ref\n  Ref\n    Path x\n");
    }

    /// Statement recovery used to stop at an unmatched `)`/`]` without consuming it, and
    /// the block loop then re-parsed the same (deduplicated) error forever.
    #[test]
    fn recovery_always_makes_progress_on_stray_closers() {
        let inputs = [
            "fn F() { let x = ); }",
            "fn F() { Foo(1, , 2); }",
            "fn F() { let x = 1 ]; }",
            "fn F() { foo(1 2); }",
            "fn F() { a); }",
            "fn F() { x = ]; }",
            "fn F() { ) ] ) }",
            "fn F() { let p = P { x: , }; let y = 1; } fn G() {}",
            "fn F() { Foo(P { x: , }, ); let y = 1; }",
            "fn F() { (1 2; let y = 1; }",
        ];
        for src in inputs {
            let mut sink = DiagnosticSink::new();
            let file = parse_file(FileId(0), src, &mut sink);
            assert!(sink.has_errors(), "no error reported for {src:?}");
            let has_f = file
                .items
                .iter()
                .any(|i| matches!(&i.kind, sabiko_ast::ItemKind::Fn(f) if f.name.name == "F"));
            assert!(has_f, "`fn F` missing from the AST of {src:?}");
            // Errors inside a nested delimiter must not end the enclosing block early.
            assert_eq!(
                sink.diagnostics().len(),
                1,
                "exactly one error expected for {src:?}: {:?}",
                sink.diagnostics()
            );
        }
    }

    #[test]
    fn source_file_span_covers_the_whole_text() {
        for src in ["", "  \n", "fn F() {}", "fn F() { \"unterminated"] {
            let mut sink = DiagnosticSink::new();
            let file = parse_file(FileId(0), src, &mut sink);
            assert_eq!(
                file.span,
                Span::new(FileId(0), 0, u32::try_from(src.len()).unwrap())
            );
        }
    }
}
