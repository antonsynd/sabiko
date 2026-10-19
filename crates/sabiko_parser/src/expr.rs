//! Expressions: a Pratt loop for binary operators over a recursive descent core.

use sabiko_ast::{BinOp, Expr, ExprKind, FieldInit, Lit, UnOp, CAST_PRECEDENCE};
use sabiko_diagnostics::{Diagnostic, ErrorCode};
use sabiko_lexer::literal::{self, LiteralError};
use sabiko_lexer::TokenKind;
use sabiko_span::Span;

use crate::{PResult, Parser};

impl Parser<'_, '_> {
    /// Full expression including assignment (lowest precedence, right associative).
    pub(crate) fn parse_expr(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        let stmt_start = self.stmt_start;
        let lhs = self.parse_binary(0)?;
        if stmt_start && lhs.is_block_like() {
            return Ok(lhs);
        }
        let op = match self.peek_kind() {
            TokenKind::Eq => None,
            TokenKind::PlusEq => Some(BinOp::Add),
            TokenKind::MinusEq => Some(BinOp::Sub),
            TokenKind::StarEq => Some(BinOp::Mul),
            TokenKind::SlashEq => Some(BinOp::Div),
            TokenKind::PercentEq => Some(BinOp::Rem),
            TokenKind::AmpEq => Some(BinOp::BitAnd),
            TokenKind::PipeEq => Some(BinOp::BitOr),
            TokenKind::CaretEq => Some(BinOp::BitXor),
            TokenKind::ShlEq => Some(BinOp::Shl),
            TokenKind::ShrEq => Some(BinOp::Shr),
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.parse_expr()?;
        let kind = match op {
            None => ExprKind::Assign(Box::new(lhs), Box::new(rhs)),
            Some(op) => ExprKind::AssignOp(op, Box::new(lhs), Box::new(rhs)),
        };
        Ok(self.mk(kind, start))
    }

    /// Parses an expression in a context where a struct literal is not allowed bare
    /// (`if`/`while` conditions).
    fn parse_expr_no_struct(&mut self) -> PResult<Expr> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let r = self.parse_expr();
        self.no_struct_literal = saved;
        r
    }

    fn mk(&mut self, kind: ExprKind, start: Span) -> Expr {
        let id = self.new_id();
        Expr {
            id,
            kind,
            span: self.span_from(start),
        }
    }

    fn binop(&self) -> Option<BinOp> {
        Some(match self.peek_kind() {
            TokenKind::Plus => BinOp::Add,
            TokenKind::Minus => BinOp::Sub,
            TokenKind::Star => BinOp::Mul,
            TokenKind::Slash => BinOp::Div,
            TokenKind::Percent => BinOp::Rem,
            TokenKind::AndAnd => BinOp::And,
            TokenKind::OrOr => BinOp::Or,
            TokenKind::Amp => BinOp::BitAnd,
            TokenKind::Pipe => BinOp::BitOr,
            TokenKind::Caret => BinOp::BitXor,
            TokenKind::Shl => BinOp::Shl,
            TokenKind::Shr => BinOp::Shr,
            TokenKind::EqEq => BinOp::Eq,
            TokenKind::Ne => BinOp::Ne,
            TokenKind::Lt => BinOp::Lt,
            TokenKind::Le => BinOp::Le,
            TokenKind::Gt => BinOp::Gt,
            TokenKind::Ge => BinOp::Ge,
            _ => return None,
        })
    }

    fn parse_binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let start = self.peek().span;
        let stmt_start = self.stmt_start;
        let mut lhs = self.parse_cast()?;
        if stmt_start && lhs.is_block_like() {
            return Ok(lhs);
        }
        if matches!(self.peek_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            let span = self.peek().span;
            return Err(self.error_unsupported("range expressions", span, "planned for Phase 5"));
        }
        while let Some(op) = self.binop() {
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            let op_tok = self.bump();
            if op.is_comparison() {
                if let ExprKind::Binary(prev, ..) = &lhs.kind {
                    if prev.is_comparison() {
                        let d = Diagnostic::error(ErrorCode::CHAINED_COMPARISON, "comparison operators cannot be chained")
                            .with_label(op_tok.span, "second comparison here")
                            .with_note("split the comparison into two and join them with `&&`, or add parentheses");
                        return Err(self.emit(d));
                    }
                }
            }
            let rhs = self.parse_binary(prec + 1)?;
            lhs = self.mk(ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), start);
        }
        Ok(lhs)
    }

    /// `expr as Type`, binding tighter than every binary operator but looser than unary.
    fn parse_cast(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        let stmt_start = self.stmt_start;
        let mut e = self.parse_unary()?;
        if stmt_start && e.is_block_like() {
            return Ok(e);
        }
        while self.eat(TokenKind::As).is_some() {
            let ty = self.parse_type()?;
            e = self.mk(ExprKind::Cast(Box::new(e), ty), start);
        }
        debug_assert!(CAST_PRECEDENCE > BinOp::Mul.precedence());
        Ok(e)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        if !matches!(
            self.peek_kind(),
            TokenKind::Minus
                | TokenKind::Bang
                | TokenKind::Star
                | TokenKind::Amp
                | TokenKind::AndAnd
        ) {
            return self.parse_postfix();
        }
        // A prefix operator means the statement's expression is not block-like.
        self.stmt_start = false;
        match self.peek_kind() {
            TokenKind::Minus => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(self.mk(ExprKind::Unary(UnOp::Neg, Box::new(e)), start))
            }
            TokenKind::Bang => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(self.mk(ExprKind::Unary(UnOp::Not, Box::new(e)), start))
            }
            TokenKind::Star => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(self.mk(ExprKind::Deref(Box::new(e)), start))
            }
            TokenKind::Amp => {
                self.bump();
                let mutable = self.eat(TokenKind::Mut).is_some();
                let e = self.parse_unary()?;
                Ok(self.mk(
                    ExprKind::Ref {
                        mutable,
                        expr: Box::new(e),
                    },
                    start,
                ))
            }
            TokenKind::AndAnd => {
                // `&&x` in prefix position is two borrows.
                let tok = self.bump();
                let mutable = self.eat(TokenKind::Mut).is_some();
                let inner_start = Span::new(self.file, tok.span.start + 1, tok.span.end);
                let e = self.parse_unary()?;
                let inner = self.mk(
                    ExprKind::Ref {
                        mutable,
                        expr: Box::new(e),
                    },
                    inner_start,
                );
                Ok(self.mk(
                    ExprKind::Ref {
                        mutable: false,
                        expr: Box::new(inner),
                    },
                    start,
                ))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        let stmt_start = self.stmt_start;
        let mut e = self.parse_primary()?;
        loop {
            // rustc: `.` and `?` always continue; `(` and `[` do not follow a block-like
            // expression at statement start.
            let complete = stmt_start && e.is_block_like();
            match self.peek_kind() {
                TokenKind::LParen | TokenKind::LBracket if complete => return Ok(e),
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_call_args()?;
                    e = self.mk(
                        ExprKind::Call {
                            callee: Box::new(e),
                            args,
                        },
                        start,
                    );
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    self.expect(TokenKind::RBracket)?;
                    e = self.mk(ExprKind::Index(Box::new(e), Box::new(index)), start);
                }
                TokenKind::Dot => {
                    self.bump();
                    if self.at(TokenKind::Int) {
                        let span = self.peek().span;
                        return Err(self.error_unsupported(
                            "tuple field access (`.0`)",
                            span,
                            "planned for Phase 5",
                        ));
                    }
                    let name = self.parse_ident()?;
                    if self.at(TokenKind::PathSep) {
                        let span = self.peek().span;
                        return Err(self.error_unsupported(
                            "turbofish method calls",
                            span,
                            "planned for Phase 6",
                        ));
                    }
                    if self.eat(TokenKind::LParen).is_some() {
                        let args = self.parse_call_args()?;
                        e = self.mk(
                            ExprKind::MethodCall {
                                receiver: Box::new(e),
                                method: name,
                                args,
                            },
                            start,
                        );
                    } else {
                        e = self.mk(ExprKind::Field(Box::new(e), name), start);
                    }
                }
                TokenKind::Question => {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "the `?` operator",
                        span,
                        "planned for Phase 5",
                    ));
                }
                _ => return Ok(e),
            }
        }
    }

    /// Parses `args)` after the opening parenthesis has been consumed.
    fn parse_call_args(&mut self) -> PResult<Vec<Expr>> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let mut args = Vec::new();
        while !self.at(TokenKind::RParen) && !self.at_eof() {
            match self.parse_expr() {
                Ok(a) => args.push(a),
                Err(e) => {
                    self.no_struct_literal = saved;
                    return Err(e);
                }
            }
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.no_struct_literal = saved;
        self.expect(TokenKind::RParen)?;
        Ok(args)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        // Only the first primary of a statement is subject to the statement restriction.
        self.stmt_start = false;
        match self.peek_kind() {
            TokenKind::Int
            | TokenKind::Float
            | TokenKind::Str
            | TokenKind::Char
            | TokenKind::True
            | TokenKind::False => {
                let lit = self.parse_lit()?;
                Ok(self.mk(ExprKind::Lit(lit), start))
            }
            TokenKind::LParen => {
                self.bump();
                if self.eat(TokenKind::RParen).is_some() {
                    return Ok(self.mk(ExprKind::Unit, start));
                }
                let saved = self.no_struct_literal;
                self.no_struct_literal = false;
                let inner = self.parse_expr();
                self.no_struct_literal = saved;
                let inner = inner?;
                if self.at(TokenKind::Comma) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "tuple expressions",
                        span,
                        "planned for Phase 5",
                    ));
                }
                self.expect(TokenKind::RParen)?;
                Ok(self.mk(ExprKind::Paren(Box::new(inner)), start))
            }
            TokenKind::LBrace => {
                let saved = self.no_struct_literal;
                self.no_struct_literal = false;
                let block = self.parse_block();
                self.no_struct_literal = saved;
                let block = block?;
                Ok(self.mk(ExprKind::Block(block), start))
            }
            TokenKind::If => self.parse_if(),
            TokenKind::While => {
                self.bump();
                if self.at(TokenKind::Let) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported("`while let`", span, "planned for Phase 5"));
                }
                let cond = self.parse_expr_no_struct()?;
                let body = self.parse_block()?;
                Ok(self.mk(
                    ExprKind::While {
                        cond: Box::new(cond),
                        body,
                    },
                    start,
                ))
            }
            TokenKind::Loop => {
                self.bump();
                let body = self.parse_block()?;
                Ok(self.mk(ExprKind::Loop { body }, start))
            }
            TokenKind::Break => {
                self.bump();
                if self.at(TokenKind::Lifetime) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported("loop labels", span, "planned for Phase 5"));
                }
                let value = if self.can_start_operand() {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                Ok(self.mk(ExprKind::Break(value), start))
            }
            TokenKind::Continue => {
                self.bump();
                if self.at(TokenKind::Lifetime) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported("loop labels", span, "planned for Phase 5"));
                }
                Ok(self.mk(ExprKind::Continue, start))
            }
            TokenKind::Return => {
                self.bump();
                let value = if self.can_start_operand() {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                Ok(self.mk(ExprKind::Return(value), start))
            }
            TokenKind::Ident => {
                let path = self.parse_path()?;
                if self.at(TokenKind::LBrace) {
                    if !self.no_struct_literal {
                        return self.parse_struct_literal(path, start);
                    }
                    // `if x == Point { X: 1 } {`: looks like a struct literal where one is
                    // not allowed. rustc reports this specially; so do we.
                    if self.nth_kind(1) == TokenKind::Ident
                        && matches!(self.nth_kind(2), TokenKind::Colon | TokenKind::Comma)
                    {
                        let brace = self.peek().span;
                        let d = Diagnostic::error(
                            ErrorCode::STRUCT_LITERAL_NOT_ALLOWED,
                            "struct literals are not allowed here",
                        )
                        .with_label(path.span.to(brace), "this looks like a struct literal")
                        .with_note(
                            "surround the struct literal with parentheses: `(Point { ... })`",
                        );
                        return Err(self.emit(d));
                    }
                }
                if self.at(TokenKind::Bang) && self.nth_kind(1) == TokenKind::LParen {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "macro invocations",
                        span,
                        "not planned (use `Print`/`PrintLine`)",
                    ));
                }
                Ok(self.mk(ExprKind::Path(path), start))
            }
            TokenKind::Match => {
                Err(self.error_unsupported("`match` expressions", start, "planned for Phase 5"))
            }
            TokenKind::For => {
                Err(self.error_unsupported("`for` loops", start, "planned for Phase 5"))
            }
            TokenKind::SelfValue => {
                Err(self.error_unsupported("`self`", start, "planned for Phase 5"))
            }
            TokenKind::Pipe | TokenKind::OrOr => {
                Err(self.error_unsupported("closures", start, "not planned"))
            }
            TokenKind::LBracket => {
                Err(self.error_unsupported("array expressions", start, "planned for Phase 5"))
            }
            TokenKind::Unsafe => Err(self.error_unsupported(
                "`unsafe` blocks",
                start,
                "not planned (see docs/DECISIONS.md #13)",
            )),
            TokenKind::DotDot | TokenKind::DotDotEq => {
                Err(self.error_unsupported("range expressions", start, "planned for Phase 5"))
            }
            TokenKind::Eof => Err(self.error_expected("an expression")),
            _ => {
                let d = Diagnostic::error(
                    ErrorCode::EXPECTED_EXPRESSION,
                    format!(
                        "expected an expression, found {}",
                        self.describe_current_pub()
                    ),
                )
                .with_label(start, "expected an expression here");
                Err(self.emit(d))
            }
        }
    }

    fn parse_if(&mut self) -> PResult<Expr> {
        let start = self.expect(TokenKind::If)?.span;
        if self.at(TokenKind::Let) {
            let span = self.peek().span;
            return Err(self.error_unsupported("`if let`", span, "planned for Phase 5"));
        }
        let cond = self.parse_expr_no_struct()?;
        let then = self.parse_block()?;
        let els = if self.eat(TokenKind::Else).is_some() {
            if self.at(TokenKind::If) {
                Some(Box::new(self.parse_if()?))
            } else {
                let bstart = self.peek().span;
                let block = self.parse_block()?;
                Some(Box::new(self.mk(ExprKind::Block(block), bstart)))
            }
        } else {
            None
        };
        Ok(self.mk(
            ExprKind::If {
                cond: Box::new(cond),
                then,
                els,
            },
            start,
        ))
    }

    fn parse_struct_literal(&mut self, path: sabiko_ast::Path, start: Span) -> PResult<Expr> {
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let result = (|| {
            while !self.at(TokenKind::RBrace) && !self.at_eof() {
                if self.at(TokenKind::DotDot) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "struct update syntax (`..base`)",
                        span,
                        "planned for Phase 5",
                    ));
                }
                let fstart = self.peek().span;
                let name = self.parse_ident()?;
                let value = if self.eat(TokenKind::Colon).is_some() {
                    self.parse_expr()?
                } else {
                    // Shorthand `Point { X, Y }` means `X: X`.
                    let path = sabiko_ast::Path {
                        segments: vec![name.clone()],
                        span: name.span,
                    };
                    self.mk(ExprKind::Path(path), fstart)
                };
                fields.push(FieldInit {
                    name,
                    value,
                    span: self.span_from(fstart),
                });
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::RBrace)?;
            Ok(())
        })();
        self.no_struct_literal = saved;
        result?;
        Ok(self.mk(ExprKind::Struct { path, fields }, start))
    }

    /// Whether the current token begins an operand for `break`/`return`. In a condition
    /// (`while break {}`), a `{` starts the loop body, not a block operand (as in rustc).
    fn can_start_operand(&self) -> bool {
        if self.no_struct_literal && self.at(TokenKind::LBrace) {
            return false;
        }
        !matches!(
            self.peek_kind(),
            TokenKind::Semi
                | TokenKind::RBrace
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::Comma
                | TokenKind::Eof
                | TokenKind::Else
        )
    }

    fn parse_lit(&mut self) -> PResult<Lit> {
        let tok = self.bump();
        let text = self.text(tok);
        match tok.kind {
            TokenKind::True => Ok(Lit::Bool(true)),
            TokenKind::False => Ok(Lit::Bool(false)),
            TokenKind::Int => match literal::parse_int(text) {
                Ok(l) => Ok(Lit::Int {
                    value: l.value,
                    suffix: l.suffix,
                }),
                Err(LiteralError::IntegerTooLarge) => {
                    let d = Diagnostic::error(
                        ErrorCode::INVALID_INTEGER_LITERAL,
                        "integer literal is too large",
                    )
                    .with_label(tok.span, "does not fit in 128 bits");
                    Err(self.emit_at(d, tok.span))
                }
                Err(_) => {
                    let d = Diagnostic::error(
                        ErrorCode::INVALID_INTEGER_LITERAL,
                        "invalid integer literal",
                    )
                    .with_label(tok.span, "no digits after the radix prefix");
                    Err(self.emit_at(d, tok.span))
                }
            },
            TokenKind::Float => {
                let (body, suffix) = if let Some(b) = text.strip_suffix("f32") {
                    (b, Some(sabiko_ast::FloatSuffix::F32))
                } else if let Some(b) = text.strip_suffix("f64") {
                    (b, Some(sabiko_ast::FloatSuffix::F64))
                } else {
                    (text, None)
                };
                if literal::parse_float(text).is_err() {
                    let d = Diagnostic::error(
                        ErrorCode::INVALID_FLOAT_LITERAL,
                        "invalid float literal",
                    )
                    .with_label(tok.span, "cannot be parsed as a number");
                    return Err(self.emit_at(d, tok.span));
                }
                Ok(Lit::Float {
                    text: body.replace('_', ""),
                    suffix,
                })
            }
            TokenKind::Str => {
                let body = text.strip_prefix('"').unwrap_or(text);
                let body = body.strip_suffix('"').unwrap_or(body);
                match literal::unescape_str(body) {
                    Ok(s) => Ok(Lit::Str(s)),
                    Err(e) => Err(self.escape_error(&e, tok.span, 1)),
                }
            }
            TokenKind::Char => {
                let body = &text[1..text.len() - 1];
                match literal::unescape_char(body) {
                    Ok(c) => Ok(Lit::Char(c)),
                    Err(e) => Err(self.escape_error(&e, tok.span, 1)),
                }
            }
            _ => unreachable!("parse_lit called on a non-literal token"),
        }
    }

    fn escape_error(
        &mut self,
        e: &LiteralError,
        lit_span: Span,
        body_offset: u32,
    ) -> crate::ParseError {
        let d = match e {
            LiteralError::InvalidEscape { start, end }
            | LiteralError::InvalidUnicodeEscape { start, end } => {
                let to_u32 = |n: usize| u32::try_from(n).expect("literal fits in u32");
                let span = Span::new(
                    self.file,
                    lit_span.start + body_offset + to_u32(*start),
                    lit_span.start + body_offset + to_u32(*end),
                );
                let msg = if matches!(e, LiteralError::InvalidEscape { .. }) {
                    "unknown escape sequence"
                } else {
                    "invalid unicode escape"
                };
                Diagnostic::error(ErrorCode::INVALID_ESCAPE, msg)
                    .with_label(span, "this escape is not valid")
                    .with_note("valid escapes: `\\n` `\\r` `\\t` `\\\\` `\\0` `\\'` `\\\"` `\\xNN` `\\u{NNNN}`")
            }
            LiteralError::NotASingleChar => Diagnostic::error(
                ErrorCode::INVALID_CHAR_LITERAL,
                "character literal must contain exactly one character",
            )
            .with_label(lit_span, "contains zero or several characters"),
            LiteralError::IntegerTooLarge | LiteralError::NoDigits => {
                unreachable!("not an escape error")
            }
        };
        self.emit_at(d, lit_span)
    }

    /// Emits a diagnostic about an already-consumed token (so the dedup key is its span,
    /// not the current position).
    fn emit_at(&mut self, d: Diagnostic, _span: Span) -> crate::ParseError {
        self.sink.emit(d);
        crate::ParseError
    }
}
