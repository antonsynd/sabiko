//! Expressions: a Pratt loop for binary operators over a recursive descent core.

use sabiko_ast::{BinOp, Expr, ExprKind, FieldInit, Lit, Type, TypeKind, UnOp, CAST_PRECEDENCE};
use sabiko_diagnostics::{Diagnostic, ErrorCode};
use sabiko_lexer::literal::{self, LiteralError};
use sabiko_lexer::TokenKind;
use sabiko_span::Span;

use crate::{PResult, Parser};

impl Parser<'_, '_> {
    /// Full expression including assignment (lowest precedence, right associative).
    pub(crate) fn parse_expr(&mut self) -> PResult<Expr> {
        self.nested(Self::parse_expr_inner)
    }

    /// [`Self::parse_expr`] without its level of nesting, for the operand of a construct
    /// that already counted one (call arguments, an index).
    fn parse_expr_inner(&mut self) -> PResult<Expr> {
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
        self.keep_depth(|p| p.parse_binary_chain(min_prec))
    }

    /// Each operator of the chain is a level: `a + b + c` is `(a + b) + c`.
    fn parse_binary_chain(&mut self, min_prec: u8) -> PResult<Expr> {
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
            self.descend()?;
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
        self.keep_depth(Self::parse_cast_chain)
    }

    fn parse_cast_chain(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        let stmt_start = self.stmt_start;
        let mut e = self.parse_unary()?;
        if stmt_start && e.is_block_like() {
            return Ok(e);
        }
        while self.eat(TokenKind::As).is_some() {
            self.descend()?;
            let ty = self.parse_type_no_generics()?;
            if matches!(ty.kind, TypeKind::Path(_))
                && matches!(self.peek_kind(), TokenKind::Lt | TokenKind::Shl)
            {
                return Err(self.error_cast_followed_by_lt(start, &ty));
            }
            e = self.mk(ExprKind::Cast(Box::new(e), ty), start);
        }
        debug_assert!(CAST_PRECEDENCE > BinOp::Mul.precedence());
        Ok(e)
    }

    /// `x as i32 < y`: as in rustc, the `<` after a cast's type path starts generic
    /// arguments, not a comparison. When what follows really looks like `<T, U>`, the
    /// usual "generic arguments are not yet supported" diagnostic is more helpful.
    fn error_cast_followed_by_lt(&mut self, cast_start: Span, ty: &Type) -> crate::ParseError {
        let op = self.peek();
        if self.looks_like_generic_args() {
            return self.error_unsupported(
                "generic type arguments",
                op.span,
                "planned for Phase 6",
            );
        }
        let (op_text, meaning, verb) = if op.kind == TokenKind::Lt {
            ("<", "comparison", "comparing")
        } else {
            ("<<", "shift", "shifting")
        };
        let ty_text = &self.src[ty.span.range()];
        let cast_text = &self.src[cast_start.to(ty.span).range()];
        let d = Diagnostic::error(
            ErrorCode::UNEXPECTED_TOKEN,
            format!(
                "`{op_text}` is interpreted as a start of generic arguments for `{ty_text}`, not a {meaning}"
            ),
        )
        .with_label(op.span, format!("not interpreted as {meaning}"))
        .with_label(ty.span, "the type of the cast")
        .with_note(format!(
            "try {verb} the cast value: `({cast_text}) {op_text} ...`"
        ));
        self.emit(d)
    }

    /// Whether the tokens from the current `<` form a balanced `<...>` made only of tokens
    /// that can appear in type arguments (`Vec<Option<&mut T>>`, `Map<K, V>`).
    fn looks_like_generic_args(&self) -> bool {
        let mut depth = 0i32;
        let mut n = 0;
        loop {
            match self.nth_kind(n) {
                TokenKind::Lt => depth += 1,
                TokenKind::Shl => depth += 2,
                TokenKind::Gt => depth -= 1,
                TokenKind::Shr => depth -= 2,
                TokenKind::Ident
                | TokenKind::PathSep
                | TokenKind::Comma
                | TokenKind::Amp
                | TokenKind::AndAnd
                | TokenKind::Mut
                | TokenKind::Underscore
                | TokenKind::LParen
                | TokenKind::RParen
                | TokenKind::Lifetime => {}
                _ => return false,
            }
            if depth <= 0 {
                return depth == 0;
            }
            n += 1;
        }
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
                let e = self.nested(Self::parse_unary)?;
                Ok(self.mk(ExprKind::Unary(UnOp::Neg, Box::new(e)), start))
            }
            TokenKind::Bang => {
                self.bump();
                let e = self.nested(Self::parse_unary)?;
                Ok(self.mk(ExprKind::Unary(UnOp::Not, Box::new(e)), start))
            }
            TokenKind::Star => {
                self.bump();
                let e = self.nested(Self::parse_unary)?;
                Ok(self.mk(ExprKind::Deref(Box::new(e)), start))
            }
            TokenKind::Amp => {
                self.bump();
                let mutable = self.eat(TokenKind::Mut).is_some();
                let e = self.nested(Self::parse_unary)?;
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
                let e = self.nested(|p| p.nested(Self::parse_unary))?;
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
        self.keep_depth(Self::parse_postfix_chain)
    }

    /// Each call, index and field access of the chain is a level.
    fn parse_postfix_chain(&mut self) -> PResult<Expr> {
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
                    self.descend()?;
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
                    self.descend()?;
                    self.bump();
                    // Like parentheses, brackets lift the `if`/`while` condition restriction:
                    // `if a[P { x: 1 }] {}` is fine.
                    let index = self.in_delimited(TokenKind::RBracket, |p| {
                        let index = p.parse_expr_inner()?;
                        p.expect(TokenKind::RBracket)?;
                        Ok(index)
                    })?;
                    e = self.mk(ExprKind::Index(Box::new(e), Box::new(index)), start);
                }
                TokenKind::Dot => {
                    self.descend()?;
                    self.bump();
                    if matches!(self.peek_kind(), TokenKind::Int | TokenKind::Float) {
                        e = self.parse_tuple_index(e, start)?;
                        continue;
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

    /// `base.0` after the `.`: a tuple field index. The index is kept as written (`01` and
    /// `0x1` name no field; the type checker says so, as rustc does), but a suffix is a
    /// syntax error. `t.0.1` lexes as `t`, `.`, `0.1`: the float literal is split into two
    /// field accesses, as rustc does.
    fn parse_tuple_index(&mut self, base: Expr, start: Span) -> PResult<Expr> {
        let tok = self.bump();
        let text = self.text(tok);
        let bad_suffix = |s: &str| {
            s.find(|c: char| c.is_ascii_alphabetic())
                .filter(|_| !s.starts_with("0x") && !s.starts_with("0o") && !s.starts_with("0b"))
        };
        let pieces: Vec<&str> = if tok.kind == TokenKind::Float {
            text.split('.').collect()
        } else {
            vec![text]
        };
        let well_formed = pieces.len() <= 2
            && pieces.iter().all(|p| !p.is_empty())
            && pieces.iter().all(|p| bad_suffix(p).is_none())
            && (tok.kind == TokenKind::Int
                || pieces
                    .iter()
                    .all(|p| p.chars().all(|c| c.is_ascii_digit() || c == '_')));
        if !well_formed {
            let suffix = pieces
                .iter()
                .find_map(|p| bad_suffix(p).map(|at| &p[at..]))
                .filter(|_| tok.kind == TokenKind::Int || !text.contains(['e', 'E']));
            let d = match suffix {
                Some(suffix) => Diagnostic::error(
                    ErrorCode::UNEXPECTED_TOKEN,
                    "suffixes on a tuple index are invalid",
                )
                .with_label(tok.span, format!("invalid suffix `{suffix}`")),
                None => Diagnostic::error(
                    ErrorCode::UNEXPECTED_TOKEN,
                    format!("invalid tuple index `{text}`"),
                )
                .with_label(
                    tok.span,
                    "expected an unsuffixed decimal integer, as in `t.0`",
                ),
            };
            return Err(self.emit_at(d, tok.span));
        }
        let mut e = base;
        let mut offset = tok.span.start;
        for (i, piece) in pieces.iter().enumerate() {
            if i > 0 {
                // The second index of `t.0.1` is one more level of nesting.
                self.descend()?;
            }
            let len = u32::try_from(piece.len()).expect("token fits in u32");
            let name = sabiko_ast::Ident {
                name: (*piece).to_owned(),
                span: Span::new(self.file, offset, offset + len),
            };
            offset += len + 1;
            let id = self.new_id();
            e = Expr {
                id,
                kind: ExprKind::Field(Box::new(e), name.clone()),
                span: start.to(name.span),
            };
        }
        Ok(e)
    }

    /// Parses `args)` after the opening parenthesis has been consumed.
    fn parse_call_args(&mut self) -> PResult<Vec<Expr>> {
        self.in_delimited(TokenKind::RParen, |p| {
            let mut args = Vec::new();
            while !p.at(TokenKind::RParen) && !p.at_eof() {
                args.push(p.parse_expr_inner()?);
                if p.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            p.expect(TokenKind::RParen)?;
            Ok(args)
        })
    }

    /// Runs `f` for the inside of a bracketed construct whose closing token is `close`.
    /// Brackets lift the struct-literal restriction of `if`/`while` conditions, and on an
    /// error the rest of the construct is skipped (up to and including `close`) before the
    /// error propagates, so that the enclosing statement's recovery does not stop at a
    /// closer that is not its own (`let p = P { x: , }; ...` must not end the block).
    fn in_delimited<T>(
        &mut self,
        close: TokenKind,
        f: impl FnOnce(&mut Self) -> PResult<T>,
    ) -> PResult<T> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let r = f(self);
        self.no_struct_literal = saved;
        if r.is_err() {
            self.skip_to_closer(close);
        }
        r
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
                let (mut elems, trailing_comma) = self.in_delimited(TokenKind::RParen, |p| {
                    p.parse_comma_list(TokenKind::RParen, Self::parse_expr)
                })?;
                if elems.len() == 1 && !trailing_comma {
                    let inner = elems.pop().expect("one element");
                    return Ok(self.mk(ExprKind::Paren(Box::new(inner)), start));
                }
                Ok(self.mk(ExprKind::Tuple(elems), start))
            }
            TokenKind::LBrace => {
                let block = self.parse_block()?;
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
            TokenKind::SelfValue => {
                // `self`: a path the resolver binds to the receiver. Never a struct literal.
                let path = self.parse_path()?;
                Ok(self.mk(ExprKind::Path(path), start))
            }
            TokenKind::Ident | TokenKind::SelfType => {
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
                Some(Box::new(self.nested(Self::parse_if)?))
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
        let fields = self.in_delimited(TokenKind::RBrace, |p| {
            let mut fields = Vec::new();
            while !p.at(TokenKind::RBrace) && !p.at_eof() {
                if p.at(TokenKind::DotDot) {
                    let span = p.peek().span;
                    return Err(p.error_unsupported(
                        "struct update syntax (`..base`)",
                        span,
                        "planned for Phase 5",
                    ));
                }
                let fstart = p.peek().span;
                let name = p.parse_ident()?;
                let value = if p.eat(TokenKind::Colon).is_some() {
                    p.parse_expr()?
                } else {
                    // Shorthand `Point { X, Y }` means `X: X`.
                    let path = sabiko_ast::Path {
                        segments: vec![name.clone()],
                        span: name.span,
                    };
                    p.mk(ExprKind::Path(path), fstart)
                };
                fields.push(FieldInit {
                    name,
                    value,
                    span: p.span_from(fstart),
                });
                if p.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            p.expect(TokenKind::RBrace)?;
            Ok(fields)
        })?;
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
                let Ok(l) = literal::parse_float(text) else {
                    let d = Diagnostic::error(
                        ErrorCode::INVALID_FLOAT_LITERAL,
                        "invalid float literal",
                    )
                    .with_label(tok.span, "cannot be parsed as a number");
                    return Err(self.emit_at(d, tok.span));
                };
                // The suffix (`f32`/`f64`) is always three characters long.
                let body = &text[..text.len() - if l.suffix.is_some() { 3 } else { 0 }];
                Ok(Lit::Float {
                    text: body.replace('_', ""),
                    suffix: l.suffix,
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
