//! Items, attributes, types, patterns, blocks and statements.

use sabiko_ast::{
    Attribute, Block, ConstDecl, FieldDef, FnDecl, Ident, Item, ItemKind, LetStmt, Param, Pat,
    PatKind, Path, SourceFile, Stmt, StmtKind, StructDecl, StructKind, Type, TypeKind, Visibility,
};
use sabiko_lexer::TokenKind;
use sabiko_span::Span;

use crate::recovery::is_item_start;
use crate::{PResult, Parser};

impl Parser<'_, '_> {
    pub(crate) fn parse_source_file(&mut self) -> SourceFile {
        let mut docs = Vec::new();
        while self.at(TokenKind::InnerDocComment) {
            let t = self.bump();
            docs.push(self.text(t)[3..].to_owned());
        }
        let mut items = Vec::new();
        while !self.at_eof() {
            if self.at(TokenKind::InnerDocComment) {
                let t = self.bump();
                let _ = self.emit(
                    sabiko_diagnostics::Diagnostic::error(
                        sabiko_diagnostics::ErrorCode::UNEXPECTED_TOKEN,
                        "inner doc comments (`//!`) are only allowed at the start of the file",
                    )
                    .with_label(t.span, "move this to the top of the file"),
                );
                continue;
            }
            match self.parse_item() {
                Ok(item) => items.push(item),
                Err(_) => self.recover_to_item_start(),
            }
        }
        // The file's span covers the whole source text (also when it is empty), matching
        // the fatal-lexer-error path in `parse_file`.
        let end = u32::try_from(self.src.len()).expect("source file larger than 4 GiB");
        let span = Span::new(self.file, 0, end);
        SourceFile { docs, items, span }
    }

    fn parse_item(&mut self) -> PResult<Item> {
        let start = self.peek().span;
        let mut docs = Vec::new();
        let mut attrs = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::DocComment => {
                    let t = self.bump();
                    docs.push(self.text(t)[3..].to_owned());
                }
                TokenKind::Hash => attrs.push(self.parse_attribute()?),
                _ => break,
            }
        }
        let vis = if self.eat(TokenKind::Pub).is_some() {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let id = self.new_id();
        let kind = match self.peek_kind() {
            TokenKind::Fn => ItemKind::Fn(self.parse_fn()?),
            TokenKind::Struct => ItemKind::Struct(self.parse_struct()?),
            TokenKind::Const => ItemKind::Const(self.parse_const()?),
            TokenKind::Enum => {
                return Err(self.unsupported_item("`enum` items", "planned for Phase 5"))
            }
            TokenKind::Impl => {
                return Err(self.unsupported_item("`impl` blocks", "planned for Phase 5"))
            }
            TokenKind::Trait => {
                return Err(self.unsupported_item("`trait` items", "planned for Phase 6"))
            }
            TokenKind::Mod | TokenKind::Use => {
                return Err(self.unsupported_item("modules and `use`", "planned for Phase 9"))
            }
            TokenKind::Extern => {
                return Err(self.unsupported_item("`extern` blocks", "planned for Phase 8"))
            }
            TokenKind::Static => {
                return Err(self.unsupported_item("`static` items", "planned for Phase 7"))
            }
            TokenKind::Type => {
                return Err(self.unsupported_item("type aliases", "planned for Phase 6"))
            }
            TokenKind::Eof if docs.is_empty() && attrs.is_empty() && vis == Visibility::Private => {
                return Err(crate::ParseError);
            }
            _ => {
                let d = sabiko_diagnostics::Diagnostic::error(
                    sabiko_diagnostics::ErrorCode::EXPECTED_ITEM,
                    format!(
                        "expected an item (`fn`, `struct` or `const`), found {}",
                        self.describe_current_pub()
                    ),
                )
                .with_label(self.peek().span, "expected an item here");
                return Err(self.emit(d));
            }
        };
        Ok(Item {
            id,
            attrs,
            docs,
            vis,
            kind,
            span: self.span_from(start),
        })
    }

    pub(crate) fn describe_current_pub(&self) -> String {
        let tok = self.peek();
        match tok.kind {
            TokenKind::Ident => format!("identifier `{}`", self.text(tok)),
            k => k.describe().to_owned(),
        }
    }

    fn unsupported_item(&mut self, what: &str, phase: &str) -> crate::ParseError {
        let span = self.peek().span;
        self.error_unsupported(what, span, phase)
    }

    /// `#[path args]`; the args are kept as raw text.
    fn parse_attribute(&mut self) -> PResult<Attribute> {
        let start = self.expect(TokenKind::Hash)?.span;
        if self.at(TokenKind::Bang) {
            let span = self.peek().span;
            return Err(self.error_unsupported(
                "inner attributes (`#![...]`)",
                span,
                "planned for Phase 9",
            ));
        }
        self.expect(TokenKind::LBracket)?;
        let path = self.parse_path()?;
        let args_start = self.peek().span.start;
        let mut depth = 0i32;
        loop {
            match self.peek_kind() {
                TokenKind::Eof => return Err(self.error_expected("`]`")),
                TokenKind::LBracket | TokenKind::LParen | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBrace => depth -= 1,
                TokenKind::RBracket => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.bump();
        }
        let args_end = self.peek().span.start;
        let args = self.src[args_start as usize..args_end as usize]
            .trim()
            .to_owned();
        self.expect(TokenKind::RBracket)?;
        Ok(Attribute {
            path,
            args,
            span: self.span_from(start),
        })
    }

    pub(crate) fn parse_ident(&mut self) -> PResult<Ident> {
        if self.at(TokenKind::Ident) {
            let t = self.bump();
            Ok(Ident {
                name: self.text(t).to_owned(),
                span: t.span,
            })
        } else if self.peek_kind().is_keyword() {
            let t = self.peek();
            let d = sabiko_diagnostics::Diagnostic::error(
                sabiko_diagnostics::ErrorCode::UNEXPECTED_TOKEN,
                format!("expected identifier, found keyword {}", t.kind.describe()),
            )
            .with_label(t.span, "keywords cannot be used as names");
            Err(self.emit(d))
        } else {
            Err(self.error_expected("identifier"))
        }
    }

    pub(crate) fn parse_path(&mut self) -> PResult<Path> {
        let first = self.parse_ident()?;
        let start = first.span;
        let mut segments = vec![first];
        while self.at(TokenKind::PathSep) {
            if self.nth_kind(1) == TokenKind::Lt {
                let span = self.peek().span.to(self.tokens[self.pos + 1].span);
                return Err(self.error_unsupported(
                    "turbofish generic arguments (`::<`)",
                    span,
                    "planned for Phase 6",
                ));
            }
            self.bump();
            segments.push(self.parse_ident()?);
        }
        Ok(Path {
            segments,
            span: self.span_from(start),
        })
    }

    fn parse_fn(&mut self) -> PResult<FnDecl> {
        self.expect(TokenKind::Fn)?;
        let name = self.parse_ident()?;
        if self.at(TokenKind::Lt) {
            let span = self.peek().span;
            return Err(self.error_unsupported("generic parameters", span, "planned for Phase 6"));
        }
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        while !self.at(TokenKind::RParen) && !self.at_eof() {
            params.push(self.parse_param()?);
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        let ret = if self.eat(TokenKind::Arrow).is_some() {
            Some(self.parse_type()?)
        } else {
            None
        };
        if self.at(TokenKind::Where) {
            let span = self.peek().span;
            return Err(self.error_unsupported("`where` clauses", span, "planned for Phase 6"));
        }
        if self.at(TokenKind::Semi) {
            let span = self.peek().span;
            return Err(self.error_unsupported(
                "functions without a body",
                span,
                "planned for Phase 6 (traits) and Phase 8 (extern)",
            ));
        }
        let body = self.parse_block()?;
        Ok(FnDecl {
            name,
            params,
            ret,
            body,
        })
    }

    fn parse_param(&mut self) -> PResult<Param> {
        let start = self.peek().span;
        if matches!(self.peek_kind(), TokenKind::SelfValue)
            || (self.at(TokenKind::Amp) && matches!(self.nth_kind(1), TokenKind::SelfValue))
            || (self.at(TokenKind::Amp)
                && self.nth_kind(1) == TokenKind::Mut
                && self.nth_kind(2) == TokenKind::SelfValue)
        {
            return Err(self.error_unsupported(
                "`self` parameters (methods)",
                start,
                "planned for Phase 5",
            ));
        }
        let id = self.new_id();
        let pat = self.parse_pat()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        Ok(Param {
            id,
            pat,
            ty,
            span: self.span_from(start),
        })
    }

    fn parse_struct(&mut self) -> PResult<StructDecl> {
        self.expect(TokenKind::Struct)?;
        let name = self.parse_ident()?;
        if self.at(TokenKind::Lt) {
            let span = self.peek().span;
            return Err(self.error_unsupported("generic parameters", span, "planned for Phase 6"));
        }
        let kind = match self.peek_kind() {
            TokenKind::Semi => {
                self.bump();
                StructKind::Unit
            }
            TokenKind::LParen => {
                let span = self.peek().span;
                return Err(self.error_unsupported("tuple structs", span, "planned for Phase 5"));
            }
            TokenKind::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                while !self.at(TokenKind::RBrace) && !self.at_eof() {
                    fields.push(self.parse_field_def()?);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace)?;
                StructKind::Named(fields)
            }
            _ => return Err(self.error_expected("`{`, or `;` for a unit struct")),
        };
        Ok(StructDecl { name, kind })
    }

    fn parse_field_def(&mut self) -> PResult<FieldDef> {
        let start = self.peek().span;
        let mut docs = Vec::new();
        while self.at(TokenKind::DocComment) {
            let t = self.bump();
            docs.push(self.text(t)[3..].to_owned());
        }
        let vis = if self.eat(TokenKind::Pub).is_some() {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let id = self.new_id();
        let name = self.parse_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        Ok(FieldDef {
            id,
            docs,
            vis,
            name,
            ty,
            span: self.span_from(start),
        })
    }

    fn parse_const(&mut self) -> PResult<ConstDecl> {
        self.expect(TokenKind::Const)?;
        let name = self.parse_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        if !self.at(TokenKind::Semi) {
            return Err(self.error_expected_after_prev("`;`"));
        }
        self.bump();
        Ok(ConstDecl { name, ty, value })
    }

    // ---- types ----------------------------------------------------------------------------

    pub(crate) fn parse_type(&mut self) -> PResult<Type> {
        self.parse_type_inner(true)
    }

    /// Like [`Self::parse_type`], but a `<` after a type path is left unconsumed instead
    /// of being reported as (unsupported) generic arguments. Used for the type of a cast,
    /// where `x as i32 < y` needs its own diagnostic (see `parse_cast`).
    pub(crate) fn parse_type_no_generics(&mut self) -> PResult<Type> {
        self.parse_type_inner(false)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_type_inner(&mut self, generics: bool) -> PResult<Type> {
        let start = self.peek().span;
        let id = self.new_id();
        let kind = match self.peek_kind() {
            TokenKind::Amp | TokenKind::AndAnd => {
                let double = self.at(TokenKind::AndAnd);
                self.bump();
                if self.at(TokenKind::Lifetime) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "explicit lifetimes",
                        span,
                        "planned for Phase 9",
                    ));
                }
                let mutable = self.eat(TokenKind::Mut).is_some();
                let inner = self.parse_type()?;
                if double {
                    // `&&T` is `&(&T)`.
                    let inner_span = inner.span;
                    let inner_id = self.new_id();
                    let inner = Type {
                        id: inner_id,
                        kind: TypeKind::Ref {
                            mutable,
                            inner: Box::new(inner),
                        },
                        span: Span::new(self.file, start.start + 1, inner_span.end),
                    };
                    TypeKind::Ref {
                        mutable: false,
                        inner: Box::new(inner),
                    }
                } else {
                    TypeKind::Ref {
                        mutable,
                        inner: Box::new(inner),
                    }
                }
            }
            TokenKind::LParen => {
                self.bump();
                if self.eat(TokenKind::RParen).is_some() {
                    TypeKind::Unit
                } else {
                    let span = start;
                    return Err(self.error_unsupported("tuple types", span, "planned for Phase 5"));
                }
            }
            TokenKind::LBracket => {
                return Err(self.error_unsupported(
                    "array and slice types",
                    start,
                    "planned for Phase 5",
                ))
            }
            TokenKind::Underscore => {
                self.bump();
                TypeKind::Infer
            }
            TokenKind::Bang => {
                return Err(self.error_unsupported(
                    "the never type `!`",
                    start,
                    "planned for Phase 5",
                ))
            }
            TokenKind::Star => {
                return Err(self.error_unsupported(
                    "raw pointer types",
                    start,
                    "not planned (Sabiko has no `unsafe`)",
                ))
            }
            TokenKind::Dyn | TokenKind::Impl => {
                return Err(self.error_unsupported(
                    "trait object types",
                    start,
                    "planned for Phase 6",
                ))
            }
            TokenKind::Fn => {
                return Err(self.error_unsupported(
                    "function pointer types",
                    start,
                    "planned for Phase 6",
                ))
            }
            TokenKind::SelfType => {
                return Err(self.error_unsupported("`Self` types", start, "planned for Phase 5"))
            }
            TokenKind::Ident => {
                let path = self.parse_path()?;
                if generics && self.at(TokenKind::Lt) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "generic type arguments",
                        span,
                        "planned for Phase 6",
                    ));
                }
                TypeKind::Path(path)
            }
            _ => {
                let d = sabiko_diagnostics::Diagnostic::error(
                    sabiko_diagnostics::ErrorCode::EXPECTED_TYPE,
                    format!("expected a type, found {}", self.describe_current_pub()),
                )
                .with_label(start, "expected a type here");
                return Err(self.emit(d));
            }
        };
        Ok(Type {
            id,
            kind,
            span: self.span_from(start),
        })
    }

    // ---- patterns --------------------------------------------------------------------------

    pub(crate) fn parse_pat(&mut self) -> PResult<Pat> {
        let start = self.peek().span;
        let id = self.new_id();
        let kind = match self.peek_kind() {
            TokenKind::Underscore => {
                self.bump();
                PatKind::Wild
            }
            TokenKind::Mut => {
                self.bump();
                let name = self.parse_ident()?;
                PatKind::Ident {
                    mutable: true,
                    name,
                }
            }
            TokenKind::Ident => {
                if matches!(
                    self.nth_kind(1),
                    TokenKind::LBrace | TokenKind::LParen | TokenKind::PathSep
                ) {
                    let span = start;
                    return Err(self.error_unsupported(
                        "destructuring patterns",
                        span,
                        "planned for Phase 5",
                    ));
                }
                let name = self.parse_ident()?;
                PatKind::Ident {
                    mutable: false,
                    name,
                }
            }
            TokenKind::Ref => {
                return Err(self.error_unsupported("`ref` bindings", start, "planned for Phase 5"))
            }
            TokenKind::Amp => {
                return Err(self.error_unsupported(
                    "reference patterns",
                    start,
                    "planned for Phase 5",
                ))
            }
            TokenKind::LParen => {
                return Err(self.error_unsupported("tuple patterns", start, "planned for Phase 5"))
            }
            TokenKind::Int
            | TokenKind::Str
            | TokenKind::Char
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Minus => {
                return Err(self.error_unsupported(
                    "literal patterns",
                    start,
                    "planned for Phase 5",
                ));
            }
            k if k.is_keyword() => {
                // "expected identifier, found keyword `fn`"
                self.parse_ident()?;
                unreachable!("parse_ident fails on keywords")
            }
            _ => {
                let d = sabiko_diagnostics::Diagnostic::error(
                    sabiko_diagnostics::ErrorCode::EXPECTED_PATTERN,
                    format!("expected a pattern, found {}", self.describe_current_pub()),
                )
                .with_label(start, "expected a binding name or `_` here");
                return Err(self.emit(d));
            }
        };
        Ok(Pat {
            id,
            kind,
            span: self.span_from(start),
        })
    }

    // ---- blocks and statements --------------------------------------------------------------

    pub(crate) fn parse_block(&mut self) -> PResult<Block> {
        let start = self.expect(TokenKind::LBrace)?.span;
        let id = self.new_id();
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.at(TokenKind::RBrace) {
            if self.at_eof() {
                let _ = self.error_expected("`}`");
                return Ok(Block {
                    id,
                    stmts,
                    tail,
                    span: self.span_from(start),
                });
            }
            let before = self.pos;
            match self.parse_stmt() {
                Ok(StmtOrTail::Stmt(s)) => stmts.push(s),
                // `parse_stmt` only returns `Tail` when the next token is `}`.
                Ok(StmtOrTail::Tail(e)) => tail = Some(Box::new(e)),
                Err(_) => {
                    self.recover_to_statement_end();
                    // Recovery must always make progress, or this loop would re-parse the
                    // same statement forever (its error is deduplicated, so nothing would
                    // be reported either). `}` is left to the loop condition.
                    if self.pos == before && !self.at(TokenKind::RBrace) && !self.at_eof() {
                        self.bump();
                    }
                }
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Block {
            id,
            stmts,
            tail,
            span: self.span_from(start),
        })
    }

    fn parse_stmt(&mut self) -> PResult<StmtOrTail> {
        let start = self.peek().span;
        match self.peek_kind() {
            TokenKind::Semi => {
                self.bump();
                let id = self.new_id();
                Ok(StmtOrTail::Stmt(Stmt {
                    id,
                    kind: StmtKind::Empty,
                    span: start,
                }))
            }
            TokenKind::Let => {
                let id = self.new_id();
                self.bump();
                let pat = self.parse_pat()?;
                let ty = if self.eat(TokenKind::Colon).is_some() {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let init = if self.eat(TokenKind::Eq).is_some() {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                if self.at(TokenKind::Else) {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "`let`-`else`",
                        span,
                        "planned for Phase 5",
                    ));
                }
                if !self.at(TokenKind::Semi) {
                    return Err(self.error_expected_after_prev("`;`"));
                }
                self.bump();
                Ok(StmtOrTail::Stmt(Stmt {
                    id,
                    kind: StmtKind::Let(LetStmt { pat, ty, init }),
                    span: self.span_from(start),
                }))
            }
            k if is_item_start(k) && k != TokenKind::Hash => Err(self.error_unsupported(
                "items inside function bodies",
                start,
                "planned for Phase 9",
            )),
            TokenKind::Lifetime if self.nth_kind(1) == TokenKind::Colon => {
                Err(self.error_unsupported("loop labels", start, "planned for Phase 5"))
            }
            _ => {
                let id = self.new_id();
                self.stmt_start = true;
                let expr = self.parse_expr();
                self.stmt_start = false;
                let expr = expr?;
                let block_like = expr.is_block_like();
                if self.eat(TokenKind::Semi).is_some() {
                    return Ok(StmtOrTail::Stmt(Stmt {
                        id,
                        kind: StmtKind::Expr { expr, semi: true },
                        span: self.span_from(start),
                    }));
                }
                if self.at(TokenKind::RBrace) {
                    return Ok(StmtOrTail::Tail(expr));
                }
                if block_like {
                    return Ok(StmtOrTail::Stmt(Stmt {
                        id,
                        kind: StmtKind::Expr { expr, semi: false },
                        span: self.span_from(start),
                    }));
                }
                Err(self.error_expected_after_prev("`;`"))
            }
        }
    }
}

pub(crate) enum StmtOrTail {
    Stmt(Stmt),
    Tail(sabiko_ast::Expr),
}
