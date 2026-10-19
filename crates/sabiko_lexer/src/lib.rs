//! Tokenizer for Sabiko.
//!
//! [`lex`] turns a source file into a flat `Vec<Token>` terminated by [`TokenKind::Eof`].
//! Comments and whitespace are dropped, except doc comments (`///`, `//!`), which the
//! parser attaches to items. Lexical errors are reported to the [`DiagnosticSink`] and
//! the offending text is skipped so the parser still sees a usable token stream.
//!
//! Literal *values* are not decoded here; see [`literal`] for the helpers the parser uses.

pub mod literal;

use logos::{Lexer, Logos};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_span::{FileId, Span};

/// Lexical error carried through logos; converted to a diagnostic by [`lex`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LexError {
    #[default]
    UnexpectedCharacter,
    UnterminatedBlockComment,
    UnterminatedString,
    UnterminatedChar,
}

#[derive(Logos, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[logos(error = LexError)]
#[logos(skip r"[ \t\r\n\x0C]+")]
#[logos(skip r"//[^\n]*")]
#[allow(clippy::upper_case_acronyms)]
pub enum TokenKind {
    // ---- comments that survive ------------------------------------------------
    /// `/// ...` (but `////...` is an ordinary comment, as in Rust).
    #[regex(r"///[^\n]*", doc_comment)]
    DocComment,
    /// `//! ...`
    #[regex(r"//![^\n]*")]
    InnerDocComment,
    #[token("/*", block_comment)]
    BlockComment,

    // ---- literals -------------------------------------------------------------
    #[regex(r"[0-9][0-9_]*([iu](8|16|32|64|128|size))?")]
    #[regex(r"0x[0-9a-fA-F_]+([iu](8|16|32|64|128|size))?")]
    #[regex(r"0o[0-7_]+([iu](8|16|32|64|128|size))?")]
    #[regex(r"0b[01_]+([iu](8|16|32|64|128|size))?")]
    Int,
    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?(f32|f64)?")]
    #[regex(r"[0-9][0-9_]*[eE][+-]?[0-9_]+(f32|f64)?")]
    #[regex(r"[0-9][0-9_]*(f32|f64)")]
    Float,
    #[regex(r#""([^"\\]|\\.|\\\n)*""#)]
    Str,
    #[regex(r#""([^"\\]|\\.|\\\n)*"#, unterminated_string)]
    UnterminatedStr,
    #[regex(r"'([^'\\\n]|\\x[0-9a-fA-F]{2}|\\u\{[0-9a-fA-F_]{1,6}\}|\\.)'")]
    Char,
    #[regex(r"'[A-Za-z_][A-Za-z0-9_]*")]
    Lifetime,

    // ---- identifiers and keywords -----------------------------------------------
    #[regex(r"[A-Za-z][A-Za-z0-9_]*|_[A-Za-z0-9_]+")]
    Ident,
    #[token("as")]
    As,
    #[token("break")]
    Break,
    #[token("const")]
    Const,
    #[token("continue")]
    Continue,
    #[token("crate")]
    Crate,
    #[token("dyn")]
    Dyn,
    #[token("else")]
    Else,
    #[token("enum")]
    Enum,
    #[token("extern")]
    Extern,
    #[token("false")]
    False,
    #[token("fn")]
    Fn,
    #[token("for")]
    For,
    #[token("if")]
    If,
    #[token("impl")]
    Impl,
    #[token("in")]
    In,
    #[token("let")]
    Let,
    #[token("loop")]
    Loop,
    #[token("match")]
    Match,
    #[token("mod")]
    Mod,
    #[token("mut")]
    Mut,
    #[token("pub")]
    Pub,
    #[token("ref")]
    Ref,
    #[token("return")]
    Return,
    #[token("self")]
    SelfValue,
    #[token("Self")]
    SelfType,
    #[token("static")]
    Static,
    #[token("struct")]
    Struct,
    #[token("super")]
    Super,
    #[token("trait")]
    Trait,
    #[token("true")]
    True,
    #[token("type")]
    Type,
    #[token("unsafe")]
    Unsafe,
    #[token("use")]
    Use,
    #[token("where")]
    Where,
    #[token("while")]
    While,
    #[token("_")]
    Underscore,

    // ---- punctuation -------------------------------------------------------------
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token(",")]
    Comma,
    #[token(";")]
    Semi,
    #[token(":")]
    Colon,
    #[token("::")]
    PathSep,
    #[token(".")]
    Dot,
    #[token("..")]
    DotDot,
    #[token("..=")]
    DotDotEq,
    #[token("->")]
    Arrow,
    #[token("=>")]
    FatArrow,
    #[token("=")]
    Eq,
    #[token("==")]
    EqEq,
    #[token("!=")]
    Ne,
    #[token("<")]
    Lt,
    #[token("<=")]
    Le,
    #[token(">")]
    Gt,
    #[token(">=")]
    Ge,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("!")]
    Bang,
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("&")]
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token("+=")]
    PlusEq,
    #[token("-=")]
    MinusEq,
    #[token("*=")]
    StarEq,
    #[token("/=")]
    SlashEq,
    #[token("%=")]
    PercentEq,
    #[token("&=")]
    AmpEq,
    #[token("|=")]
    PipeEq,
    #[token("^=")]
    CaretEq,
    #[token("<<=")]
    ShlEq,
    #[token(">>=")]
    ShrEq,
    #[token("#")]
    Hash,
    #[token("?")]
    Question,
    #[token("@")]
    At,
    #[token("~")]
    Tilde,
    #[token("$")]
    Dollar,

    /// End of input. Never produced by logos; appended by [`lex`].
    Eof,
}

fn unterminated_string(_lex: &mut Lexer<TokenKind>) -> Result<(), LexError> {
    Err(LexError::UnterminatedString)
}

/// `////...` is a plain comment; only exactly three slashes start a doc comment.
fn doc_comment(lex: &mut Lexer<TokenKind>) -> logos::Filter<()> {
    if lex.slice().starts_with("////") {
        logos::Filter::Skip
    } else {
        logos::Filter::Emit(())
    }
}

/// Consumes a (possibly nested) block comment. Nesting matches Rust.
fn block_comment(lex: &mut Lexer<TokenKind>) -> logos::FilterResult<(), LexError> {
    let rest = lex.remainder();
    let bytes = rest.as_bytes();
    let mut depth = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                lex.bump(i);
                return logos::FilterResult::Skip;
            }
        } else {
            i += 1;
        }
    }
    lex.bump(bytes.len());
    logos::FilterResult::Error(LexError::UnterminatedBlockComment)
}

impl TokenKind {
    /// Human-readable name used in "expected X, found Y" diagnostics.
    #[must_use]
    pub fn describe(self) -> &'static str {
        use TokenKind as T;
        match self {
            T::DocComment | T::InnerDocComment => "doc comment",
            T::BlockComment => "comment",
            T::Int => "integer literal",
            T::Float => "float literal",
            T::Str | T::UnterminatedStr => "string literal",
            T::Char => "character literal",
            T::Lifetime => "lifetime",
            T::Ident => "identifier",
            T::As => "`as`",
            T::Break => "`break`",
            T::Const => "`const`",
            T::Continue => "`continue`",
            T::Crate => "`crate`",
            T::Dyn => "`dyn`",
            T::Else => "`else`",
            T::Enum => "`enum`",
            T::Extern => "`extern`",
            T::False => "`false`",
            T::Fn => "`fn`",
            T::For => "`for`",
            T::If => "`if`",
            T::Impl => "`impl`",
            T::In => "`in`",
            T::Let => "`let`",
            T::Loop => "`loop`",
            T::Match => "`match`",
            T::Mod => "`mod`",
            T::Mut => "`mut`",
            T::Pub => "`pub`",
            T::Ref => "`ref`",
            T::Return => "`return`",
            T::SelfValue => "`self`",
            T::SelfType => "`Self`",
            T::Static => "`static`",
            T::Struct => "`struct`",
            T::Super => "`super`",
            T::Trait => "`trait`",
            T::True => "`true`",
            T::Type => "`type`",
            T::Unsafe => "`unsafe`",
            T::Use => "`use`",
            T::Where => "`where`",
            T::While => "`while`",
            T::Underscore => "`_`",
            T::LParen => "`(`",
            T::RParen => "`)`",
            T::LBrace => "`{`",
            T::RBrace => "`}`",
            T::LBracket => "`[`",
            T::RBracket => "`]`",
            T::Comma => "`,`",
            T::Semi => "`;`",
            T::Colon => "`:`",
            T::PathSep => "`::`",
            T::Dot => "`.`",
            T::DotDot => "`..`",
            T::DotDotEq => "`..=`",
            T::Arrow => "`->`",
            T::FatArrow => "`=>`",
            T::Eq => "`=`",
            T::EqEq => "`==`",
            T::Ne => "`!=`",
            T::Lt => "`<`",
            T::Le => "`<=`",
            T::Gt => "`>`",
            T::Ge => "`>=`",
            T::Plus => "`+`",
            T::Minus => "`-`",
            T::Star => "`*`",
            T::Slash => "`/`",
            T::Percent => "`%`",
            T::Bang => "`!`",
            T::AndAnd => "`&&`",
            T::OrOr => "`||`",
            T::Amp => "`&`",
            T::Pipe => "`|`",
            T::Caret => "`^`",
            T::Shl => "`<<`",
            T::Shr => "`>>`",
            T::PlusEq => "`+=`",
            T::MinusEq => "`-=`",
            T::StarEq => "`*=`",
            T::SlashEq => "`/=`",
            T::PercentEq => "`%=`",
            T::AmpEq => "`&=`",
            T::PipeEq => "`|=`",
            T::CaretEq => "`^=`",
            T::ShlEq => "`<<=`",
            T::ShrEq => "`>>=`",
            T::Hash => "`#`",
            T::Question => "`?`",
            T::At => "`@`",
            T::Tilde => "`~`",
            T::Dollar => "`$`",
            T::Eof => "end of file",
        }
    }

    #[must_use]
    pub fn is_keyword(self) -> bool {
        use TokenKind as T;
        matches!(
            self,
            T::As
                | T::Break
                | T::Const
                | T::Continue
                | T::Crate
                | T::Dyn
                | T::Else
                | T::Enum
                | T::Extern
                | T::False
                | T::Fn
                | T::For
                | T::If
                | T::Impl
                | T::In
                | T::Let
                | T::Loop
                | T::Match
                | T::Mod
                | T::Mut
                | T::Pub
                | T::Ref
                | T::Return
                | T::SelfValue
                | T::SelfType
                | T::Static
                | T::Struct
                | T::Super
                | T::Trait
                | T::True
                | T::Type
                | T::Unsafe
                | T::Use
                | T::Where
                | T::While
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Tokenizes `src`, reporting lexical errors to `sink`. The result always ends with an
/// [`TokenKind::Eof`] token whose span is the empty span at the end of the file.
#[must_use]
pub fn lex(file: FileId, src: &str, sink: &mut DiagnosticSink) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut lexer = TokenKind::lexer(src);
    let to_u32 = |n: usize| u32::try_from(n).expect("source file larger than 4 GiB");
    while let Some(result) = lexer.next() {
        let range = lexer.span();
        let span = Span::new(file, to_u32(range.start), to_u32(range.end));
        match result {
            Ok(kind) => tokens.push(Token { kind, span }),
            Err(LexError::UnexpectedCharacter) => {
                let text = &src[range.clone()];
                // `'ab'` lexes as the lifetime `'ab` followed by a stray `'`; present it as
                // one (malformed) character literal, which the parser then reports.
                if text == "'" {
                    if let Some(last) = tokens.last_mut() {
                        if last.kind == TokenKind::Lifetime && last.span.end == span.start {
                            last.kind = TokenKind::Char;
                            last.span = last.span.to(span);
                            continue;
                        }
                    }
                }
                let shown: String = if text.chars().any(char::is_control) {
                    text.escape_default().collect()
                } else {
                    text.to_owned()
                };
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNEXPECTED_CHARACTER,
                        format!("unexpected character `{shown}`"),
                    )
                    .with_label(span, "not valid in Sabiko source"),
                );
            }
            Err(LexError::UnterminatedBlockComment) => {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNTERMINATED_BLOCK_COMMENT,
                        "unterminated block comment",
                    )
                    .with_label(
                        Span::new(file, span.start, span.start + 2),
                        "comment opened here",
                    )
                    .with_note("block comments nest: every `/*` needs a matching `*/`"),
                );
            }
            Err(LexError::UnterminatedString) => {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNTERMINATED_STRING,
                        "unterminated string literal",
                    )
                    .with_label(
                        Span::new(file, span.start, span.start + 1),
                        "string opened here",
                    ),
                );
                // Keep going as if the string had been closed so the parser can continue.
                tokens.push(Token {
                    kind: TokenKind::Str,
                    span,
                });
            }
            Err(LexError::UnterminatedChar) => {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::INVALID_CHAR_LITERAL,
                        "unterminated character literal",
                    )
                    .with_label(span, "character literal opened here"),
                );
            }
        }
    }
    let end = to_u32(src.len());
    tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span::point(file, end),
    });
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind as T;

    fn kinds(src: &str) -> (Vec<TokenKind>, Vec<Diagnostic>) {
        let mut sink = DiagnosticSink::new();
        let toks = lex(FileId(0), src, &mut sink);
        (toks.into_iter().map(|t| t.kind).collect(), sink.into_vec())
    }

    #[test]
    fn lexes_a_function() {
        let (k, d) = kinds("fn Main() -> i32 { let mut x: i32 = 42; x }");
        assert!(d.is_empty());
        assert_eq!(
            k,
            vec![
                T::Fn,
                T::Ident,
                T::LParen,
                T::RParen,
                T::Arrow,
                T::Ident,
                T::LBrace,
                T::Let,
                T::Mut,
                T::Ident,
                T::Colon,
                T::Ident,
                T::Eq,
                T::Int,
                T::Semi,
                T::Ident,
                T::RBrace,
                T::Eof
            ]
        );
    }

    #[test]
    fn literals() {
        let (k, d) = kinds(
            r#"1 1_000u32 0xFFi64 0o77 0b1010usize 1.5 2e10 3.0f32 4f64 'a' '\n' '\u{1F600}' "hi \"there\"" 'a"#,
        );
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(
            k,
            vec![
                T::Int,
                T::Int,
                T::Int,
                T::Int,
                T::Int,
                T::Float,
                T::Float,
                T::Float,
                T::Float,
                T::Char,
                T::Char,
                T::Char,
                T::Str,
                T::Lifetime,
                T::Eof
            ]
        );
    }

    #[test]
    fn multi_char_literal_is_one_char_token_and_lifetimes_survive() {
        let (k, d) = kinds("'ab'; 'a: 'b' x");
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(
            k,
            vec![
                T::Char,
                T::Semi,
                T::Lifetime,
                T::Colon,
                T::Char,
                T::Ident,
                T::Eof
            ]
        );
    }

    #[test]
    fn range_and_field_access_do_not_become_floats() {
        let (k, _) = kinds("1..2 x.0 1.Foo()");
        assert_eq!(
            k,
            vec![
                T::Int,
                T::DotDot,
                T::Int,
                T::Ident,
                T::Dot,
                T::Int,
                T::Int,
                T::Dot,
                T::Ident,
                T::LParen,
                T::RParen,
                T::Eof
            ]
        );
    }

    #[test]
    fn comments_and_doc_comments() {
        let (k, d) =
            kinds("// plain\n/// doc\n//// not doc\n//! inner\n/* block /* nested */ still */ fn");
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(k, vec![T::DocComment, T::InnerDocComment, T::Fn, T::Eof]);
    }

    #[test]
    fn unterminated_block_comment_is_reported() {
        let (k, d) = kinds("fn /* never closed");
        assert_eq!(k, vec![T::Fn, T::Eof]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, ErrorCode::UNTERMINATED_BLOCK_COMMENT);
    }

    #[test]
    fn unterminated_string_keeps_a_token() {
        let (k, d) = kinds("let s = \"abc");
        assert_eq!(k, vec![T::Let, T::Ident, T::Eq, T::Str, T::Eof]);
        assert_eq!(d[0].code, ErrorCode::UNTERMINATED_STRING);
    }

    #[test]
    fn unexpected_character() {
        let (k, d) = kinds("let x = ¿;");
        assert_eq!(k, vec![T::Let, T::Ident, T::Eq, T::Semi, T::Eof]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, ErrorCode::UNEXPECTED_CHARACTER);
        assert!(d[0].message.contains('¿'), "{}", d[0].message);
    }

    #[test]
    fn compound_operators_are_longest_match() {
        let (k, _) = kinds("a >>= b <<= c ..= d :: e");
        assert_eq!(
            k,
            vec![
                T::Ident,
                T::ShrEq,
                T::Ident,
                T::ShlEq,
                T::Ident,
                T::DotDotEq,
                T::Ident,
                T::PathSep,
                T::Ident,
                T::Eof
            ]
        );
    }

    #[test]
    fn spans_are_byte_accurate() {
        let mut sink = DiagnosticSink::new();
        let toks = lex(FileId(3), "let  x", &mut sink);
        assert_eq!(toks[1].span, Span::new(FileId(3), 5, 6));
        assert_eq!(toks[2].span, Span::point(FileId(3), 6));
    }
}
