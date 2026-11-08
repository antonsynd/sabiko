//! Expressions: a Pratt loop for binary operators over a recursive descent core.

use sabiko_ast::{
    Arm, BinOp, Expr, ExprKind, FieldInit, Lit, NodeId, RangeEnd, Type, TypeKind, UnOp,
    CAST_PRECEDENCE,
};
use sabiko_diagnostics::{Diagnostic, ErrorCode};
use sabiko_lexer::literal::{self, LiteralError};
use sabiko_lexer::TokenKind;
use sabiko_span::Span;

use crate::{AssigneeOnly, PResult, Parser};

impl Parser<'_, '_> {
    /// Full expression including assignment (lowest precedence, right associative).
    pub(crate) fn parse_expr(&mut self) -> PResult<Expr> {
        self.nested(Self::parse_expr_inner)
    }

    /// [`Self::parse_expr`] without its level of nesting, for the operand of a construct
    /// that already counted one (call arguments, an index).
    fn parse_expr_inner(&mut self) -> PResult<Expr> {
        self.parse_assign(None)
    }

    /// An assignment, or the operand of one. `range` (the span of the `[`) allows a range
    /// expression (`a..b`, `a..`): only the whole index of `e[...]` may be one (#10), so
    /// elsewhere the `..` after an operand is reported as not yet supported.
    ///
    /// The outermost one also settles the forms only a destructuring assignment's left
    /// side may contain (`_`, `..`): those its left side did not turn into a pattern are
    /// misplaced, since no enclosing expression can still be such a left side.
    fn parse_assign(&mut self, range: Option<Span>) -> PResult<Expr> {
        let mark = self.assignee_only.len();
        let r = self.parse_assign_inner(range);
        if r.is_err() {
            // Already reported; whatever the broken expression held is moot.
            self.assignee_only.truncate(mark);
        } else if self.lhs_depth == 0 {
            self.report_assignee_only(mark);
        }
        r
    }

    fn parse_assign_inner(&mut self, range: Option<Span>) -> PResult<Expr> {
        let start = self.peek().span;
        let stmt_start = self.stmt_start;
        self.lhs_depth += 1;
        let lhs = self.parse_binary(0);
        self.lhs_depth -= 1;
        let lhs = lhs?;
        if stmt_start && lhs.is_block_like() {
            return Ok(lhs);
        }
        if matches!(self.peek_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            let Some(bracket) = range else {
                let span = self.peek().span;
                return Err(self.error_unsupported(
                    "range expressions outside an index",
                    span,
                    "only an index `e[a..b]` is supported until range values (generic types, Phase 6)",
                ));
            };
            return self.parse_range_rest(Some(lhs), bracket);
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
        if op.is_none() && Self::is_destructuring(&lhs) {
            // The left side is read as a pattern before the right side is parsed, so a
            // syntax error there does not leave its `_` and `..` behind as misplaced.
            let pat = self.assignee_pat(lhs);
            let rhs = self.parse_expr()?;
            return Ok(self.mk(
                ExprKind::DestructuringAssign(Box::new(pat), Box::new(rhs)),
                start,
            ));
        }
        let rhs = self.parse_expr()?;
        let kind = match op {
            None => ExprKind::Assign(Box::new(lhs), Box::new(rhs)),
            Some(op) => ExprKind::AssignOp(op, Box::new(lhs), Box::new(rhs)),
        };
        Ok(self.mk(kind, start))
    }

    /// The index of `e[...]`: an expression, or a range (`a..b`, `a..`, `..b`, `..`,
    /// `a..=b`, `..=b`), the one place a range is parsed (#10). A range's span starts at
    /// the `[` (`bracket`), where rustc locates a panic of the slicing.
    fn parse_index(&mut self, bracket: Span) -> PResult<Expr> {
        if matches!(self.peek_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            return self.parse_range_rest(None, bracket);
        }
        self.parse_assign(Some(bracket))
    }

    /// From the `..` or `..=` of a range on: the optional end, which is parsed at the
    /// precedence of `||` as in rustc. An inclusive range needs an end.
    fn parse_range_rest(&mut self, lo: Option<Expr>, start: Span) -> PResult<Expr> {
        let op = self.bump();
        let end = if op.kind == TokenKind::DotDotEq {
            RangeEnd::Included
        } else {
            RangeEnd::Excluded
        };
        let hi = if self.at(TokenKind::RBracket) {
            if end == RangeEnd::Included {
                let d = Diagnostic::error(
                    ErrorCode::EXPECTED_EXPRESSION,
                    "inclusive range with no end",
                )
                .with_label(op.span, "an inclusive range needs an end")
                .with_note("use `..` instead of `..=` for a range with no end");
                return Err(self.emit(d));
            }
            None
        } else {
            self.descend()?;
            Some(Box::new(self.parse_binary(0)?))
        };
        Ok(self.mk(ExprKind::Range(lo.map(Box::new), hi, end), start))
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
        // A `..` ends the operand chain: `parse_assign` decides whether a range may follow.
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
            let mut ty = self.parse_type_no_generics()?;
            let open_args = matches!(&ty.kind, TypeKind::Path(path)
                if path.args_of(path.segments.len() - 1).is_none())
                && self.at(TokenKind::Lt)
                && self.looks_like_generic_args();
            if open_args {
                // `x as Wrap<i32>`: as in rustc, generic arguments.
                let lt = self.peek().span;
                let (args, end) = self.parse_generic_args()?;
                if let TypeKind::Path(path) = &mut ty.kind {
                    path.generic_args.push(sabiko_ast::GenericArgs {
                        segment: path.segments.len() - 1,
                        args,
                        span: lt.to(end),
                    });
                    path.span = path.span.to(end);
                }
                ty.span = ty.span.to(end);
            }
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
    /// arguments, not a comparison; when what follows is not a list of them, this reports
    /// rustc's error.
    fn error_cast_followed_by_lt(&mut self, cast_start: Span, ty: &Type) -> crate::ParseError {
        let op = self.peek();
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
                | TokenKind::LBracket
                | TokenKind::RBracket
                | TokenKind::Semi
                | TokenKind::Int
                | TokenKind::SelfType
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
                    let bracket = self.bump().span;
                    // Like parentheses, brackets lift the `if`/`while` condition restriction:
                    // `if a[P { x: 1 }] {}` is fine.
                    let index = self.in_delimited(TokenKind::RBracket, |p| {
                        let index = p.parse_index(bracket)?;
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
                    let generic_args =
                        if self.at(TokenKind::PathSep) && self.nth_kind(1) == TokenKind::Lt {
                            let sep = self.bump().span;
                            let (args, end) = self.parse_generic_args()?;
                            if !self.at(TokenKind::LParen) {
                                return Err(self.error_expected("`(`"));
                            }
                            Some(sabiko_ast::GenericArgs {
                                segment: 0,
                                args,
                                span: sep.to(end),
                            })
                        } else {
                            None
                        };
                    if self.eat(TokenKind::LParen).is_some() {
                        let args = self.parse_call_args()?;
                        e = self.mk(
                            ExprKind::MethodCall {
                                receiver: Box::new(e),
                                method: name,
                                generic_args: generic_args.map(Box::new),
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
                args.push(p.parse_list_elem(Self::parse_expr_inner)?);
                if p.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            p.expect(TokenKind::RParen)?;
            Ok(args)
        })
    }

    /// An element of a tuple or an argument list: an expression, or a lone `..`, which
    /// only the left side of a destructuring assignment may contain (`(a, ..) = t`).
    fn parse_list_elem(&mut self, elem: impl FnOnce(&mut Self) -> PResult<Expr>) -> PResult<Expr> {
        if self.at(TokenKind::DotDot)
            && matches!(self.nth_kind(1), TokenKind::Comma | TokenKind::RParen)
        {
            let start = self.bump().span;
            let e = self.mk(ExprKind::Range(None, None, RangeEnd::Excluded), start);
            self.assignee_only.push((e.id, AssigneeOnly::Rest(start)));
            return Ok(e);
        }
        elem(self)
    }

    /// Runs `f` for the inside of a bracketed construct whose closing token is `close`.
    /// Brackets lift the struct-literal restriction of `if`/`while` conditions and end the
    /// part of one where `let` is an expression, and on an
    /// error the rest of the construct is skipped (up to and including `close`) before the
    /// error propagates, so that the enclosing statement's recovery does not stop at a
    /// closer that is not its own (`let p = P { x: , }; ...` must not end the block).
    fn in_delimited<T>(
        &mut self,
        close: TokenKind,
        f: impl FnOnce(&mut Self) -> PResult<T>,
    ) -> PResult<T> {
        let saved = (self.no_struct_literal, self.let_allowed);
        self.no_struct_literal = false;
        self.let_allowed = false;
        let r = f(self);
        (self.no_struct_literal, self.let_allowed) = saved;
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
                    p.parse_comma_list(TokenKind::RParen, |p| p.parse_list_elem(Self::parse_expr))
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
                let cond = self.parse_cond()?;
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
                        // Recover as rustc does: read the literal and go on.
                        self.emit(d);
                        return self.parse_struct_literal(path, start);
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
            TokenKind::Match => self.parse_match(),
            TokenKind::Let if self.let_allowed => self.parse_let_expr(),
            TokenKind::Let => {
                // Read it whole, to report it whole.
                let e = self.parse_let_expr()?;
                self.cond_lets.pop();
                Err(self.misplaced_let(e.span, false))
            }
            TokenKind::For => {
                Err(self.error_unsupported("`for` loops", start, "planned for Phase 5"))
            }
            TokenKind::Pipe | TokenKind::OrOr => {
                Err(self.error_unsupported("closures", start, "not planned"))
            }
            TokenKind::LBracket => {
                self.bump();
                // Like parentheses, brackets lift the `if`/`while` condition restriction.
                let kind = self.in_delimited(TokenKind::RBracket, |p| {
                    if p.eat(TokenKind::RBracket).is_some() {
                        return Ok(ExprKind::Array(Vec::new()));
                    }
                    let first = p.parse_expr()?;
                    if p.eat(TokenKind::Semi).is_some() {
                        let count = p.parse_expr()?;
                        p.expect(TokenKind::RBracket)?;
                        return Ok(ExprKind::Repeat(Box::new(first), Box::new(count)));
                    }
                    let mut elems = vec![first];
                    if p.eat(TokenKind::Comma).is_some() {
                        let (rest, _) =
                            p.parse_comma_list(TokenKind::RBracket, Self::parse_expr)?;
                        elems.extend(rest);
                    } else {
                        p.expect(TokenKind::RBracket)?;
                    }
                    Ok(ExprKind::Array(elems))
                })?;
                Ok(self.mk(kind, start))
            }
            TokenKind::Unsafe => Err(self.error_unsupported(
                "`unsafe` blocks",
                start,
                "not planned (see docs/DECISIONS.md #13)",
            )),
            TokenKind::DotDot | TokenKind::DotDotEq => Err(self.error_unsupported(
                "range expressions outside an index",
                start,
                "only an index `e[a..b]` is supported until range values (generic types, Phase 6)",
            )),
            TokenKind::Underscore => {
                self.bump();
                let e = self.mk(ExprKind::Underscore, start);
                self.assignee_only
                    .push((e.id, AssigneeOnly::Underscore(start)));
                Ok(e)
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
        let cond = self.parse_cond()?;
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

    /// The condition of an `if` or `while`: an expression in which `let pat = expr` may
    /// appear, joined to the rest by `&&` only (a let chain, edition 2024): `if let Some(x)
    /// = a && x > 0 && let Ok(y) = F(x)`. A `let` under `||`, `!`, parentheses or any other
    /// operator is an error, as in rustc.
    fn parse_cond(&mut self) -> PResult<Expr> {
        self.parse_cond_with(true)
    }

    /// [`Self::parse_cond`], where a struct literal may be bare unless `no_struct`: in a
    /// match guard (`p if let q = e && c => ..`, #98) the `=>` that follows makes one
    /// unambiguous, as in rustc.
    fn parse_cond_with(&mut self, no_struct: bool) -> PResult<Expr> {
        let saved = std::mem::replace(&mut self.let_allowed, true);
        let mark = self.cond_lets.len();
        let cond = if no_struct {
            self.parse_expr_no_struct()
        } else {
            self.parse_expr()
        };
        self.let_allowed = saved;
        let lets = self.cond_lets.split_off(mark);
        let cond = cond?;
        if lets.is_empty() {
            return Ok(cond);
        }
        let mut chain = Vec::new();
        let mut or = false;
        chain_lets(&cond, &mut chain, &mut or);
        if let Some(&(_, span)) = lets.iter().find(|(id, _)| !chain.contains(id)) {
            return Err(self.misplaced_let(span, or));
        }
        Ok(cond)
    }

    /// `let pat = expr` in a condition. The scrutinee binds tighter than `&&` and `||`,
    /// which continue the condition.
    fn parse_let_expr(&mut self) -> PResult<Expr> {
        let start = self.bump().span;
        let pat = self.parse_pat_or()?;
        self.expect(TokenKind::Eq)?;
        let saved = (self.no_struct_literal, self.let_allowed);
        self.no_struct_literal = true;
        self.let_allowed = false;
        let scrutinee = self.nested(|p| p.parse_binary(BinOp::And.precedence() + 1));
        (self.no_struct_literal, self.let_allowed) = saved;
        let scrutinee = scrutinee?;
        if matches!(self.peek_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
            let span = self.peek().span;
            return Err(self.error_unsupported(
                "range expressions outside an index",
                span,
                "only an index `e[a..b]` is supported until range values (generic types, Phase 6)",
            ));
        }
        let e = self.mk(ExprKind::Let(Box::new(pat), Box::new(scrutinee)), start);
        self.cond_lets.push((e.id, e.span));
        Ok(e)
    }

    /// rustc's "expected expression, found `let` statement": a `let` outside the `&&`
    /// chain of an `if` or `while` condition. `or`: the condition has a `||` on its chain.
    fn misplaced_let(&mut self, span: Span, or: bool) -> crate::ParseError {
        let mut d = Diagnostic::error(
            ErrorCode::EXPECTED_EXPRESSION,
            "expected an expression, found `let` statement",
        )
        .with_label(span, "`let` is not allowed here")
        .with_note(
            "`let` is only allowed directly in the condition of an `if` or `while` or in a match guard, joined to the rest of it by `&&` (a let chain)",
        );
        if or {
            d = d.with_note("`||` operators are not supported in let chain conditions");
        }
        self.emit(d)
    }

    /// `match scrutinee { pat if guard => body, .. }`. A block-like body ends its arm, so
    /// the comma after it is optional.
    fn parse_match(&mut self) -> PResult<Expr> {
        let start = self.expect(TokenKind::Match)?.span;
        let scrutinee = self.parse_expr_no_struct()?;
        self.expect(TokenKind::LBrace)?;
        let arms = self.in_delimited(TokenKind::RBrace, |p| {
            let mut arms = Vec::new();
            while !p.at(TokenKind::RBrace) && !p.at_eof() {
                let arm_start = p.peek().span;
                let id = p.new_id();
                let pat = p.parse_pat_or()?;
                let guard = if p.eat(TokenKind::If).is_some() {
                    Some(p.parse_cond_with(false)?)
                } else {
                    None
                };
                p.expect(TokenKind::FatArrow)?;
                p.stmt_start = true;
                let body = p.parse_expr();
                p.stmt_start = false;
                let body = body?;
                let block_like = body.is_block_like();
                arms.push(Arm {
                    id,
                    pat,
                    guard,
                    body,
                    span: p.span_from(arm_start),
                });
                if p.eat(TokenKind::Comma).is_none() && !block_like && !p.at(TokenKind::RBrace) {
                    return Err(p.error_expected_after_prev("`,`"));
                }
            }
            p.expect(TokenKind::RBrace)?;
            Ok(arms)
        })?;
        Ok(self.mk(
            ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            start,
        ))
    }

    fn parse_struct_literal(&mut self, path: sabiko_ast::Path, start: Span) -> PResult<Expr> {
        self.expect(TokenKind::LBrace)?;
        let mut rest = None;
        let fields = self.in_delimited(TokenKind::RBrace, |p| {
            let mut fields = Vec::new();
            while !p.at(TokenKind::RBrace) && !p.at_eof() {
                if p.at(TokenKind::DotDot) && p.nth_kind(1) == TokenKind::RBrace {
                    // `S { X: a, .. }`: only on the left of a destructuring assignment.
                    rest = Some(p.bump().span);
                    break;
                }
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
                    let path = sabiko_ast::Path::single(name.clone());
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
        let e = self.mk(ExprKind::Struct { path, fields, rest }, start);
        if let Some(span) = rest {
            self.assignee_only
                .push((e.id, AssigneeOnly::StructRest(span)));
        }
        Ok(e)
    }

    /// Whether the current token begins an operand for `break`/`return`. In a condition
    /// (`while break {}`), a `{` starts the loop body, not a block operand (as in rustc).
    /// A `=>` ends a match guard (`_ if break => ..`).
    fn can_start_operand(&self) -> bool {
        if self.no_struct_literal && self.at(TokenKind::LBrace) {
            return false;
        }
        // rustc's `Token::can_begin_expr`: anything else ends the operand-less `return` or
        // `break`, which a postfix or binary operator may then follow (`return.Foo()` is a
        // method call on `!`, `return as i32` a cast).
        matches!(
            self.peek_kind(),
            TokenKind::Int
                | TokenKind::Float
                | TokenKind::Str
                | TokenKind::UnterminatedStr
                | TokenKind::Char
                | TokenKind::Lifetime
                | TokenKind::Ident
                | TokenKind::Break
                | TokenKind::Const
                | TokenKind::Continue
                | TokenKind::Crate
                | TokenKind::False
                | TokenKind::For
                | TokenKind::If
                | TokenKind::Let
                | TokenKind::Loop
                | TokenKind::Match
                | TokenKind::Return
                | TokenKind::SelfValue
                | TokenKind::SelfType
                | TokenKind::Static
                | TokenKind::Super
                | TokenKind::True
                | TokenKind::Unsafe
                | TokenKind::While
                | TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                | TokenKind::Bang
                | TokenKind::Minus
                | TokenKind::Star
                | TokenKind::Amp
                | TokenKind::AndAnd
                | TokenKind::Pipe
                | TokenKind::OrOr
                | TokenKind::DotDot
                | TokenKind::DotDotEq
                | TokenKind::Lt
                | TokenKind::Shl
                | TokenKind::PathSep
                | TokenKind::Hash
        )
    }

    pub(crate) fn parse_lit(&mut self) -> PResult<Lit> {
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

/// The `let` expressions of the `&&` chain `cond` is (`a && let p = e && c`), outermost
/// operators only: a `let` under parentheses or any other operator is not on it. Sets `or`
/// when a `||` is met on the chain.
fn chain_lets(cond: &Expr, out: &mut Vec<NodeId>, or: &mut bool) {
    match &cond.kind {
        ExprKind::Binary(BinOp::And, l, r) => {
            chain_lets(l, out, or);
            chain_lets(r, out, or);
        }
        ExprKind::Binary(BinOp::Or, ..) => *or = true,
        ExprKind::Let(..) => out.push(cond.id),
        _ => {}
    }
}
