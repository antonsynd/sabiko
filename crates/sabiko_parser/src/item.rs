//! Items, attributes, types, patterns, blocks and statements.

use sabiko_ast::{
    Attribute, Block, ConstDecl, EnumDecl, Expr, ExprKind, FieldDef, FieldPat, FnDecl, GenericArgs,
    GenericParam, Ident, ImplDecl, Item, ItemKind, LetStmt, NodeId, Param, Pat, PatKind, Path,
    RangeEnd, SelfKind, SelfParam, SourceFile, Stmt, StmtKind, StructDecl, StructKind, Type,
    TypeKind, UnOp, UseDecl, Variant, VariantData, Visibility,
};
use sabiko_diagnostics::{Diagnostic, ErrorCode};
use sabiko_lexer::TokenKind;
use sabiko_span::Span;

use crate::recovery::is_item_start;
use crate::{PResult, Parser};

/// What an item is a member of, which decides the members allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Members {
    /// A top-level item.
    None,
    Impl,
    Trait,
}

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
        // Every expression is inside some outermost assignment's operand, which settles
        // these; none can be left.
        debug_assert!(self.assignee_only.is_empty(), "unsettled `_` or `..`");
        // The file's span covers the whole source text (also when it is empty), matching
        // the fatal-lexer-error path in `parse_file`.
        let end = u32::try_from(self.src.len()).expect("source file larger than 4 GiB");
        let span = Span::new(self.file, 0, end);
        SourceFile { docs, items, span }
    }

    fn parse_item(&mut self) -> PResult<Item> {
        self.parse_item_in(Members::None)
    }

    /// An item, or a member of an `impl` block or a trait (`members`): a function, which
    /// may take a `self` parameter, or (in an `impl` block) a constant (#48).
    fn parse_item_in(&mut self, members: Members) -> PResult<Item> {
        let in_impl = members != Members::None;
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
            TokenKind::Fn => ItemKind::Fn(self.parse_fn(members)?),
            TokenKind::Const => ItemKind::Const(self.parse_const(members)?),
            TokenKind::Type if in_impl => {
                return Err(self.unsupported_item("associated types", "planned for Phase 6"))
            }
            _ if in_impl => {
                let block = if members == Members::Trait {
                    "trait"
                } else {
                    "`impl` block"
                };
                let d = sabiko_diagnostics::Diagnostic::error(
                    sabiko_diagnostics::ErrorCode::EXPECTED_ITEM,
                    format!(
                        "expected a function or a constant in this {block}, found {}",
                        self.describe_current_pub()
                    ),
                )
                .with_label(self.peek().span, "expected `fn` or `const` here");
                return Err(self.emit(d));
            }
            TokenKind::Struct => ItemKind::Struct(self.parse_struct()?),
            TokenKind::Enum => ItemKind::Enum(self.parse_enum()?),
            TokenKind::Impl => ItemKind::Impl(self.parse_impl()?),
            TokenKind::Trait => ItemKind::Trait(self.parse_trait()?),
            TokenKind::Use if self.options.library => ItemKind::Use(self.parse_use()?),
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
                        "expected an item (`fn`, `struct`, `enum`, `const`, `trait` or `impl`), found {}",
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

    /// `#[path]`, `#[path(tokens)]` (or `[...]`, `{...}`) or `#[path = tokens]`, like Rust;
    /// the args are kept as raw text. Delimiters inside the args must nest and match.
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
        match self.peek_kind() {
            TokenKind::RBracket => {}
            TokenKind::Eq => {
                self.bump();
                self.skip_attr_tokens(false)?;
            }
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                self.skip_attr_tokens(true)?;
            }
            _ => return Err(self.error_expected("`(`, `[`, `{`, `=` or `]`")),
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

    /// Skips attribute args up to the closing `]`. With `single_group` the args are one
    /// delimited group and skipping stops after its closer; the caller expects the `]`.
    fn skip_attr_tokens(&mut self, single_group: bool) -> PResult<()> {
        let mut closers: Vec<TokenKind> = Vec::new();
        loop {
            let kind = self.peek_kind();
            match kind {
                TokenKind::Eof => return Err(self.error_expected("`]`")),
                TokenKind::RBracket if closers.is_empty() => return Ok(()),
                TokenKind::LParen => closers.push(TokenKind::RParen),
                TokenKind::LBracket => closers.push(TokenKind::RBracket),
                TokenKind::LBrace => closers.push(TokenKind::RBrace),
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    match closers.pop() {
                        Some(expected) if expected == kind => {}
                        Some(expected) => return Err(self.error_expected(expected.describe())),
                        None => return Err(self.error_expected("`]`")),
                    }
                    if single_group && closers.is_empty() {
                        self.bump();
                        return Ok(());
                    }
                }
                _ => {}
            }
            self.bump();
        }
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

    /// `A::B::C`, with turbofish generic arguments after any segment (`Id::<i32>`,
    /// `Pair::<i32, bool>::New`). The first segment may be `Self` (the type of the
    /// enclosing `impl`) or `self` (the receiver, as a value); the resolver gives both
    /// their meaning.
    pub(crate) fn parse_path(&mut self) -> PResult<Path> {
        let first = if matches!(self.peek_kind(), TokenKind::SelfType | TokenKind::SelfValue) {
            let t = self.bump();
            Ident {
                name: self.text(t).to_owned(),
                span: t.span,
            }
        } else {
            self.parse_ident()?
        };
        let start = first.span;
        let mut segments = vec![first];
        let mut generic_args = Vec::new();
        while self.at(TokenKind::PathSep) {
            if self.nth_kind(1) == TokenKind::Lt {
                let sep = self.bump().span;
                if generic_args
                    .last()
                    .is_some_and(|a: &GenericArgs| a.segment == segments.len() - 1)
                {
                    // `F::<A>::<B>`: rustc's "expected identifier, found `<`".
                    return Err(self.error_expected("identifier"));
                }
                let (args, end) = self.parse_generic_args()?;
                generic_args.push(GenericArgs {
                    segment: segments.len() - 1,
                    args,
                    span: sep.to(end),
                });
                continue;
            }
            self.bump();
            segments.push(self.parse_ident()?);
        }
        Ok(Path {
            segments,
            generic_args,
            span: self.span_from(start),
        })
    }

    /// `use A::B;` or `use A::{B, C};` (library files only, decision 90), at `use`: the
    /// path has at least two segments and no generic arguments.
    fn parse_use(&mut self) -> PResult<UseDecl> {
        self.expect(TokenKind::Use)?;
        let first = self.parse_ident()?;
        let start = first.span;
        let mut segments = vec![first];
        let mut names = Vec::new();
        let mut braced = false;
        while self.eat(TokenKind::PathSep).is_some() {
            if self.eat(TokenKind::LBrace).is_some() {
                braced = true;
                while !self.at(TokenKind::RBrace) && !self.at_eof() {
                    names.push(self.parse_ident()?);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace)?;
                break;
            }
            segments.push(self.parse_ident()?);
        }
        if !braced {
            if segments.len() < 2 {
                return Err(self.error_expected("`::`"));
            }
            names.push(segments.pop().expect("two segments"));
        }
        let prefix = Path {
            span: Span::new(
                self.file,
                start.start,
                segments.last().map_or(start, |s| s.span).end,
            ),
            segments,
            generic_args: Vec::new(),
        };
        self.expect(TokenKind::Semi)?;
        Ok(UseDecl {
            prefix,
            names,
            braced,
        })
    }

    /// `<A, B>`, at the `<`. Returns the arguments and the span of the closing `>`. A `>>`
    /// closing two lists (`Opt<Opt<i32>>`) is split: its first `>` closes this list.
    pub(crate) fn parse_generic_args(&mut self) -> PResult<(Vec<Type>, Span)> {
        self.expect(TokenKind::Lt)?;
        self.nested(|p| {
            let mut args = Vec::new();
            while !p.at_generic_close() && !p.at_eof() {
                if p.at(TokenKind::Lifetime) {
                    let span = p.peek().span;
                    return Err(p.error_unsupported(
                        "lifetime arguments",
                        span,
                        "planned for Phase 9",
                    ));
                }
                args.push(p.parse_type()?);
                if p.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            let end = p.expect_generic_close()?;
            Ok((args, end))
        })
    }

    fn at_generic_close(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Gt | TokenKind::Shr)
    }

    /// Consumes a `>`, or the first `>` of a `>>`.
    fn expect_generic_close(&mut self) -> PResult<Span> {
        if self.at(TokenKind::Shr) {
            let span = self.peek().span;
            let first = Span::new(self.file, span.start, span.start + 1);
            self.split_token(
                TokenKind::Gt,
                Span::new(self.file, span.start + 1, span.end),
            );
            return Ok(first);
        }
        Ok(self.expect(TokenKind::Gt)?.span)
    }

    /// `<T, U>` after a function's name, at the `<`. Only a trait's parameters may have
    /// defaults (`allow_defaults`, `trait Add<Rhs = Self>`).
    fn parse_generic_params(
        &mut self,
        allow_bounds: bool,
        allow_defaults: bool,
    ) -> PResult<Vec<GenericParam>> {
        self.expect(TokenKind::Lt)?;
        let mut params = Vec::new();
        while !self.at_generic_close() && !self.at_eof() {
            match self.peek_kind() {
                TokenKind::Lifetime => {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "lifetime parameters",
                        span,
                        "planned for Phase 9",
                    ));
                }
                TokenKind::Const => {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "const generic parameters",
                        span,
                        "not planned for 0.1",
                    ));
                }
                _ => {}
            }
            let id = self.new_id();
            let name = self.parse_ident()?;
            let mut bounds = Vec::new();
            if self.at(TokenKind::Colon) {
                if !allow_bounds {
                    let span = self.peek().span;
                    let what = if allow_defaults {
                        "bounds on the type parameters of a trait"
                    } else {
                        "bounds on the type parameters of a struct or enum"
                    };
                    return Err(self.error_unsupported(
                        what,
                        span,
                        "write the bounds on the `impl` blocks and functions that need them",
                    ));
                }
                self.bump();
                bounds = self.parse_bounds()?;
            }
            let mut default = None;
            if self.at(TokenKind::Eq) {
                if !allow_defaults {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "default type parameters outside a trait",
                        span,
                        "not planned for 0.1",
                    ));
                }
                self.bump();
                default = Some(self.parse_type()?);
            } else if params.iter().any(|p: &GenericParam| p.default.is_some()) {
                // rustc's uncoded "generic parameters with a default must be trailing".
                let d = Diagnostic::error(
                    ErrorCode::DEFAULT_NOT_TRAILING,
                    "generic parameters with a default must be trailing",
                )
                .with_label(name.span, "must come before the parameters with a default");
                self.emit(d);
            }
            params.push(GenericParam {
                id,
                name,
                bounds,
                default,
            });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect_generic_close()?;
        Ok(params)
    }

    /// `A + B`: the traits of a bound, after the `:`. At least one.
    fn parse_bounds(&mut self) -> PResult<Vec<sabiko_ast::TraitRef>> {
        let mut bounds = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::Question => {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "`?Sized` bounds",
                        span,
                        "not planned for 0.1",
                    ));
                }
                TokenKind::Lifetime => {
                    let span = self.peek().span;
                    return Err(self.error_unsupported(
                        "lifetime bounds",
                        span,
                        "planned for Phase 9",
                    ));
                }
                _ => {}
            }
            bounds.push(self.parse_trait_ref()?);
            if self.eat(TokenKind::Plus).is_none() {
                return Ok(bounds);
            }
        }
    }

    /// A trait's path, as in a type (`Show`, `Into<i32>`).
    fn parse_trait_ref(&mut self) -> PResult<sabiko_ast::TraitRef> {
        let id = self.new_id();
        let ty = self.parse_type()?;
        if let TypeKind::Path(path) = ty.kind {
            return Ok(sabiko_ast::TraitRef { id, path });
        }
        let d = Diagnostic::error(
            ErrorCode::UNEXPECTED_TOKEN,
            "expected a trait, found a type",
        )
        .with_label(ty.span, "not a trait");
        Err(self.emit(d))
    }

    /// `where T: A + B, U: C` before a body or an `impl` block's `{`; empty without
    /// `where`.
    fn parse_where_clause(&mut self) -> PResult<Vec<sabiko_ast::WherePredicate>> {
        let mut preds = Vec::new();
        if self.eat(TokenKind::Where).is_none() {
            return Ok(preds);
        }
        while !matches!(
            self.peek_kind(),
            TokenKind::LBrace | TokenKind::Semi | TokenKind::Eof
        ) {
            if self.at(TokenKind::Lifetime) {
                let span = self.peek().span;
                return Err(self.error_unsupported("lifetime bounds", span, "planned for Phase 9"));
            }
            let start = self.peek().span;
            let ty = self.parse_type()?;
            self.expect(TokenKind::Colon)?;
            let bounds = self.parse_bounds()?;
            preds.push(sabiko_ast::WherePredicate {
                ty,
                bounds,
                span: self.span_from(start),
            });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        Ok(preds)
    }

    /// `impl<T> Type { fn ... }` or `impl<T> Trait for Type where .. { fn ... }`, after any
    /// attributes and visibility.
    fn parse_impl(&mut self) -> PResult<ImplDecl> {
        self.expect(TokenKind::Impl)?;
        let generics = if self.at(TokenKind::Lt) {
            self.parse_generic_params(true, false)?
        } else {
            Vec::new()
        };
        let trait_start = self.peek().span;
        let trait_id = self.new_id();
        let mut self_ty = self.parse_type()?;
        let mut trait_ref = None;
        if self.at(TokenKind::For) {
            let TypeKind::Path(path) = self_ty.kind else {
                let d = Diagnostic::error(
                    ErrorCode::UNEXPECTED_TOKEN,
                    "expected a trait, found a type",
                )
                .with_label(self.span_from(trait_start), "not a trait");
                return Err(self.emit(d));
            };
            self.bump();
            trait_ref = Some(sabiko_ast::TraitRef { id: trait_id, path });
            self_ty = self.parse_type()?;
        }
        let where_clause = self.parse_where_clause()?;
        let items = self.parse_members(Members::Impl)?;
        Ok(ImplDecl {
            generics,
            trait_ref,
            self_ty,
            where_clause,
            items,
        })
    }

    /// `{ members }` of an `impl` block or a trait.
    fn parse_members(&mut self, members: Members) -> PResult<Vec<Item>> {
        self.expect(TokenKind::LBrace)?;
        let mut items = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at_eof() {
            match self.parse_item_in(members) {
                Ok(item) => items.push(item),
                Err(_) => self.recover_to_impl_member(),
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(items)
    }

    /// `trait Name: Super { members }`, after any attributes and visibility.
    fn parse_trait(&mut self) -> PResult<sabiko_ast::TraitDecl> {
        self.expect(TokenKind::Trait)?;
        let name = self.parse_ident()?;
        let generics = if self.at(TokenKind::Lt) {
            self.parse_generic_params(false, true)?
        } else {
            Vec::new()
        };
        let supertraits = if self.eat(TokenKind::Colon).is_some() {
            self.parse_bounds()?
        } else {
            Vec::new()
        };
        if self.at(TokenKind::Where) {
            let span = self.peek().span;
            return Err(self.error_unsupported(
                "`where` clauses on traits",
                span,
                "write supertraits after `:`",
            ));
        }
        let items = self.parse_members(Members::Trait)?;
        Ok(sabiko_ast::TraitDecl {
            name,
            generics,
            supertraits,
            items,
        })
    }

    /// `self`, `mut self`, `&self`, `&mut self` at the start of a parameter list, or one
    /// of them later in it.
    fn at_self_param(&self) -> bool {
        match self.peek_kind() {
            TokenKind::SelfValue => true,
            TokenKind::Mut => self.nth_kind(1) == TokenKind::SelfValue,
            TokenKind::Amp => {
                // `&'a self` and `&'a mut self` too, which `parse_self_param` rejects.
                let at = if self.nth_kind(1) == TokenKind::Lifetime {
                    2
                } else {
                    1
                };
                match self.nth_kind(at) {
                    TokenKind::SelfValue => true,
                    TokenKind::Mut => self.nth_kind(at + 1) == TokenKind::SelfValue,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// The receiver of a method. Outside an `impl` block it is reported (rustc: "`self`
    /// parameter is only allowed in associated functions") and dropped, so the rest of
    /// the function still parses.
    fn parse_self_param(&mut self, in_impl: bool) -> PResult<Option<SelfParam>> {
        let start = self.peek().span;
        let kind = if self.eat(TokenKind::Amp).is_some() {
            if self.at(TokenKind::Lifetime) {
                let span = self.peek().span;
                return Err(self.error_unsupported(
                    "explicit lifetimes",
                    span,
                    "planned for Phase 9",
                ));
            }
            let mutable = self.eat(TokenKind::Mut).is_some();
            self.expect(TokenKind::SelfValue)?;
            SelfKind::Ref { mutable }
        } else {
            let mutable = self.eat(TokenKind::Mut).is_some();
            self.expect(TokenKind::SelfValue)?;
            SelfKind::Value { mutable }
        };
        let span = self.span_from(start);
        if self.at(TokenKind::Colon) {
            let at = self.peek().span;
            return Err(self.error_unsupported(
                "explicit `self` types (`self: Type`)",
                at,
                "planned for Phase 6",
            ));
        }
        if !in_impl {
            let d = sabiko_diagnostics::Diagnostic::error(
                sabiko_diagnostics::ErrorCode::MISPLACED_SELF_PARAM,
                "`self` parameter is only allowed in associated functions",
            )
            .with_label(span, "not semantically valid as function parameter")
            .with_note("associated functions are those in `impl` blocks");
            let _ = self.emit(d);
            return Ok(None);
        }
        Ok(Some(SelfParam {
            id: self.new_id(),
            kind,
            span,
        }))
    }

    fn parse_fn(&mut self, members: Members) -> PResult<FnDecl> {
        let in_impl = members != Members::None;
        self.expect(TokenKind::Fn)?;
        let name = self.parse_ident()?;
        let generics = if self.at(TokenKind::Lt) {
            self.parse_generic_params(true, false)?
        } else {
            Vec::new()
        };
        self.expect(TokenKind::LParen)?;
        let mut self_param = None;
        let mut params = Vec::new();
        if self.at_self_param() {
            self_param = self.parse_self_param(in_impl)?;
            if self.eat(TokenKind::Comma).is_none() && !self.at(TokenKind::RParen) {
                return Err(self.error_expected("`,` or `)`"));
            }
        }
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
        let where_clause = self.parse_where_clause()?;
        if self.at(TokenKind::Semi) {
            let span = self.peek().span;
            if members == Members::Trait {
                self.bump();
                return Ok(FnDecl {
                    name,
                    generics,
                    self_param,
                    params,
                    ret,
                    where_clause,
                    body: None,
                });
            }
            let what = if in_impl {
                "associated function in `impl` without body"
            } else {
                "free function without a body"
            };
            let d = Diagnostic::error(ErrorCode::UNEXPECTED_TOKEN, what)
                .with_label(span, "a function needs a body: `{ .. }`")
                .with_note("only the required methods of a trait have no body; `extern` functions are planned for Phase 8");
            return Err(self.emit(d));
        }
        let body = self.parse_block()?;
        Ok(FnDecl {
            name,
            generics,
            self_param,
            params,
            ret,
            where_clause,
            body: Some(body),
        })
    }

    fn parse_param(&mut self) -> PResult<Param> {
        let start = self.peek().span;
        if self.at_self_param() {
            let d = sabiko_diagnostics::Diagnostic::error(
                sabiko_diagnostics::ErrorCode::MISPLACED_SELF_PARAM,
                "unexpected `self` parameter in function",
            )
            .with_label(
                start,
                "must be the first parameter of an associated function",
            );
            return Err(self.emit(d));
        }
        let id = self.new_id();
        let pat = self.parse_pat_no_top_or("function parameters")?;
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
        let generics = if self.at(TokenKind::Lt) {
            self.parse_generic_params(false, false)?
        } else {
            Vec::new()
        };
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
                    fields.push(self.parse_field_def(false)?);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace)?;
                StructKind::Named(fields)
            }
            _ => return Err(self.error_expected("`{`, or `;` for a unit struct")),
        };
        Ok(StructDecl {
            name,
            generics,
            kind,
        })
    }

    /// A field of a struct, or of a struct-like enum variant (`in_variant`), whose fields
    /// share the enum's visibility and so cannot be `pub` (rustc's E0449; reported, then
    /// parsing goes on with the field private).
    fn parse_field_def(&mut self, in_variant: bool) -> PResult<FieldDef> {
        let start = self.peek().span;
        let mut docs = Vec::new();
        while self.at(TokenKind::DocComment) {
            let t = self.bump();
            docs.push(self.text(t)[3..].to_owned());
        }
        let vis = match self.eat(TokenKind::Pub) {
            Some(tok) if in_variant => {
                let d = Diagnostic::error(
                    ErrorCode::UNEXPECTED_TOKEN,
                    "visibility qualifiers are not permitted here",
                )
                .with_label(tok.span, "remove the qualifier")
                .with_note("enum variants and their fields always share the visibility of the enum they are in");
                self.emit(d);
                Visibility::Private
            }
            Some(_) => Visibility::Public,
            None => Visibility::Private,
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

    /// `enum Name { A, B(T, U), C { X: T }, D = 3 }`.
    fn parse_enum(&mut self) -> PResult<EnumDecl> {
        self.expect(TokenKind::Enum)?;
        let name = self.parse_ident()?;
        let generics = if self.at(TokenKind::Lt) {
            self.parse_generic_params(false, false)?
        } else {
            Vec::new()
        };
        self.expect(TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at_eof() {
            variants.push(self.parse_variant()?);
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(EnumDecl {
            name,
            generics,
            variants,
        })
    }

    fn parse_variant(&mut self) -> PResult<Variant> {
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
        let id = self.new_id();
        let name = self.parse_ident()?;
        let data = match self.peek_kind() {
            TokenKind::LParen => {
                self.bump();
                let (tys, _) =
                    self.nested(|p| p.parse_comma_list(TokenKind::RParen, Self::parse_type))?;
                VariantData::Tuple(tys)
            }
            TokenKind::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                while !self.at(TokenKind::RBrace) && !self.at_eof() {
                    fields.push(self.parse_field_def(true)?);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace)?;
                VariantData::Named(fields)
            }
            _ => VariantData::Unit,
        };
        let discriminant = if self.eat(TokenKind::Eq).is_some() {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Variant {
            id,
            attrs,
            docs,
            name,
            data,
            discriminant,
            span: self.span_from(start),
        })
    }

    /// A constant: an item, a member of an `impl` block, or a trait's (#116), which alone
    /// may leave out its value (a required constant).
    fn parse_const(&mut self, members: Members) -> PResult<ConstDecl> {
        self.expect(TokenKind::Const)?;
        let name = self.parse_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        if self.at(TokenKind::Semi) {
            let span = self.peek().span;
            if members != Members::Trait {
                let what = if members == Members::Impl {
                    "associated constant in `impl` without body"
                } else {
                    "free constant item without body"
                };
                let d = Diagnostic::error(ErrorCode::UNEXPECTED_TOKEN, what)
                    .with_label(span, "a constant needs a value: `= <expr>;`")
                    .with_note("only the required constants of a trait have no value");
                return Err(self.emit(d));
            }
            self.bump();
            return Ok(ConstDecl {
                name,
                ty,
                value: None,
            });
        }
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        if !self.at(TokenKind::Semi) {
            return Err(self.error_expected_after_prev("`;`"));
        }
        self.bump();
        Ok(ConstDecl {
            name,
            ty,
            value: Some(value),
        })
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
                let inner = self.nested(|p| {
                    if double {
                        p.descend()?;
                    }
                    p.parse_type()
                })?;
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
                    let (mut elems, trailing_comma) =
                        self.nested(|p| p.parse_comma_list(TokenKind::RParen, Self::parse_type))?;
                    if elems.len() == 1 && !trailing_comma {
                        // `(T)` is `T`.
                        return Ok(elems.pop().expect("one element"));
                    }
                    TypeKind::Tuple(elems)
                }
            }
            TokenKind::LBracket => {
                self.bump();
                self.nested(|p| {
                    let elem = p.parse_type()?;
                    if p.eat(TokenKind::Semi).is_some() {
                        let len = p.parse_expr()?;
                        p.expect(TokenKind::RBracket)?;
                        return Ok(TypeKind::Array(Box::new(elem), Box::new(len)));
                    }
                    p.expect(TokenKind::RBracket)?;
                    Ok(TypeKind::Slice(Box::new(elem)))
                })?
            }
            TokenKind::Underscore => {
                self.bump();
                TypeKind::Infer
            }
            TokenKind::Bang if self.options.library => {
                self.bump();
                TypeKind::Never
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
            TokenKind::Ident | TokenKind::SelfType => {
                let mut path = self.parse_path()?;
                if generics && self.at(TokenKind::Lt) {
                    let segment = path.segments.len() - 1;
                    if path.args_of(segment).is_some() {
                        return Err(self.error_expected("`::` or a type"));
                    }
                    let lt = self.peek().span;
                    let (args, end) = self.parse_generic_args()?;
                    path.generic_args.push(GenericArgs {
                        segment,
                        args,
                        span: lt.to(end),
                    });
                    path.span = path.span.to(end);
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

    /// Parses `elem, elem, ...` up to and including `close` (the opening delimiter has been
    /// consumed). Returns the elements and whether the last one was followed by a comma,
    /// which tells a 1-tuple `(a,)` from a parenthesised `(a)`.
    pub(crate) fn parse_comma_list<T>(
        &mut self,
        close: TokenKind,
        mut elem: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<(Vec<T>, bool)> {
        let mut elems = Vec::new();
        let mut trailing_comma = false;
        while !self.at(close) && !self.at_eof() {
            elems.push(elem(self)?);
            trailing_comma = self.eat(TokenKind::Comma).is_some();
            if !trailing_comma {
                break;
            }
        }
        self.expect(close)?;
        Ok((elems, trailing_comma))
    }

    // ---- patterns --------------------------------------------------------------------------

    /// A pattern of a `let` or a parameter (`what`): a top-level or-pattern needs
    /// parentheses there, as in rustc.
    pub(crate) fn parse_pat_no_top_or(&mut self, what: &str) -> PResult<Pat> {
        let pat = self.parse_pat()?;
        if self.at(TokenKind::Pipe) {
            let span = pat.span.to(self.peek().span);
            let d = Diagnostic::error(
                ErrorCode::UNEXPECTED_TOKEN,
                format!("{what} require top-level or-patterns in parentheses"),
            )
            .with_label(span, "add parentheses around the or-pattern");
            return Err(self.emit(d));
        }
        Ok(pat)
    }

    /// A pattern that may be an or-pattern `p | q`: a `match` arm, `if let`, `while let`,
    /// or inside parentheses and other delimiters. A leading `|` is allowed.
    pub(crate) fn parse_pat_or(&mut self) -> PResult<Pat> {
        self.nested(|p| {
            let start = p.peek().span;
            let id = p.new_id();
            p.eat(TokenKind::Pipe);
            let first = p.parse_pat()?;
            if !p.at(TokenKind::Pipe) {
                return Ok(first);
            }
            let mut alts = vec![first];
            while p.eat(TokenKind::Pipe).is_some() {
                alts.push(p.parse_pat()?);
            }
            Ok(Pat {
                id,
                kind: PatKind::Or(alts),
                span: p.span_from(start),
            })
        })
    }

    /// An element of a tuple or tuple-struct pattern: `..` alone (a rest pattern, the one
    /// place it is allowed), or a pattern that may be an or-pattern.
    fn parse_pat_elem(&mut self) -> PResult<Pat> {
        if self.at(TokenKind::DotDot)
            && matches!(self.nth_kind(1), TokenKind::Comma | TokenKind::RParen)
        {
            let tok = self.bump();
            return Ok(Pat {
                id: self.new_id(),
                kind: PatKind::Rest,
                span: tok.span,
            });
        }
        self.parse_pat_or()
    }

    /// One alternative of a pattern: everything but a top-level `|`.
    pub(crate) fn parse_pat(&mut self) -> PResult<Pat> {
        self.nested(Self::parse_pat_inner)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_pat_inner(&mut self) -> PResult<Pat> {
        let start = self.peek().span;
        let id = self.new_id();
        let kind = match self.peek_kind() {
            TokenKind::Underscore => {
                self.bump();
                PatKind::Wild
            }
            TokenKind::DotDot => {
                self.bump();
                if !self.at_range_bound() {
                    // A rest pattern is only an element of a tuple or tuple-struct pattern
                    // (`parse_pat_elem`), as in rustc.
                    let d = Diagnostic::error(
                        ErrorCode::UNEXPECTED_TOKEN,
                        "`..` patterns are not allowed here",
                    )
                    .with_label(start, "only allowed in tuple and tuple struct patterns");
                    return Err(self.emit(d));
                }
                // `..hi`: a half-open range (#49).
                let hi = self.parse_range_bound()?;
                PatKind::Range(None, Some(Box::new(hi)), RangeEnd::Excluded)
            }
            TokenKind::DotDotEq => {
                // `..=hi` (#49).
                self.bump();
                if !self.at_range_bound() {
                    return Err(self.error_expected("a range pattern's end after `..=`"));
                }
                let hi = self.parse_range_bound()?;
                PatKind::Range(None, Some(Box::new(hi)), RangeEnd::Included)
            }
            TokenKind::Ref => {
                self.bump();
                let mutable = self.eat(TokenKind::Mut).is_some();
                self.parse_binding(true, mutable)?
            }
            TokenKind::Mut => {
                self.bump();
                if self.at(TokenKind::Ref) {
                    let d = Diagnostic::error(
                        ErrorCode::UNEXPECTED_TOKEN,
                        "the order of `mut` and `ref` is incorrect",
                    )
                    .with_label(
                        start.to(self.peek().span),
                        "try switching the order: `ref mut`",
                    );
                    return Err(self.emit(d));
                }
                self.parse_binding(false, true)?
            }
            TokenKind::Ident | TokenKind::SelfType | TokenKind::SelfValue => {
                let path = self.parse_path()?;
                match self.peek_kind() {
                    TokenKind::LParen => {
                        self.bump();
                        let (elems, _) = self.nested(|p| {
                            p.parse_comma_list(TokenKind::RParen, Self::parse_pat_elem)
                        })?;
                        self.check_single_rest(&elems, "tuple struct");
                        PatKind::TupleStruct(path, elems)
                    }
                    TokenKind::LBrace => self.parse_struct_pat(path)?,
                    TokenKind::DotDot | TokenKind::DotDotEq => {
                        let lo = self.path_expr(path);
                        self.parse_range_pat(lo)?
                    }
                    _ if path.is_single() && path.segments[0].name != "Self" => {
                        let name = path.segments.into_iter().next().expect("one segment");
                        if name.name == "self" {
                            let d = Diagnostic::error(
                                ErrorCode::EXPECTED_PATTERN,
                                "expected a pattern, found `self`",
                            )
                            .with_label(name.span, "`self` is not a binding name");
                            return Err(self.emit(d));
                        }
                        self.binding_rest(false, false, name)?
                    }
                    _ => PatKind::Path(path),
                }
            }
            TokenKind::Amp | TokenKind::AndAnd => return self.parse_ref_pat(id),
            TokenKind::LParen => {
                self.bump();
                let (mut elems, trailing_comma) =
                    self.nested(|p| p.parse_comma_list(TokenKind::RParen, Self::parse_pat_elem))?;
                if elems.len() == 1 && !trailing_comma && !matches!(elems[0].kind, PatKind::Rest) {
                    // `(p)` is `p`.
                    let mut inner = elems.pop().expect("one element");
                    if let PatKind::Or(_) = inner.kind {
                        // Keep the parentheses' span: `let (A | B) = ..` is accepted.
                        inner.span = self.span_from(start);
                    }
                    return Ok(inner);
                }
                self.check_single_rest(&elems, "tuple");
                PatKind::Tuple(elems)
            }
            TokenKind::Int
            | TokenKind::Float
            | TokenKind::Str
            | TokenKind::Char
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Minus => {
                let lit = self.parse_lit_pat_expr()?;
                if matches!(self.peek_kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
                    self.parse_range_pat(lit)?
                } else {
                    PatKind::Lit(Box::new(lit))
                }
            }
            k if k.is_keyword() => {
                // "expected identifier, found keyword `fn`"
                self.parse_ident()?;
                unreachable!("parse_ident fails on keywords")
            }
            TokenKind::LBracket => {
                return Err(self.error_unsupported("slice patterns", start, "not yet planned"));
            }
            _ => {
                let d = Diagnostic::error(
                    ErrorCode::EXPECTED_PATTERN,
                    format!("expected a pattern, found {}", self.describe_current_pub()),
                )
                .with_label(start, "expected a pattern here");
                return Err(self.emit(d));
            }
        };
        Ok(Pat {
            id,
            kind,
            span: self.span_from(start),
        })
    }

    /// `name` or `name @ pat` after `ref`, `mut` or `ref mut`.
    fn parse_binding(&mut self, by_ref: bool, mutable: bool) -> PResult<PatKind> {
        let name = self.parse_ident()?;
        self.binding_rest(by_ref, mutable, name)
    }

    fn binding_rest(&mut self, by_ref: bool, mutable: bool, name: Ident) -> PResult<PatKind> {
        let sub = if self.eat(TokenKind::At).is_some() {
            Some(Box::new(self.parse_pat()?))
        } else {
            None
        };
        Ok(PatKind::Ident {
            by_ref,
            mutable,
            name,
            sub,
        })
    }

    fn path_expr(&mut self, path: Path) -> Expr {
        let start = path.span;
        Expr {
            id: self.new_id(),
            kind: ExprKind::Path(path),
            span: start,
        }
    }

    /// Whether the current token can start the upper bound of a range pattern.
    fn at_range_bound(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::Int
                | TokenKind::Float
                | TokenKind::Char
                | TokenKind::Minus
                | TokenKind::Ident
                | TokenKind::SelfType
        )
    }

    /// A literal in a pattern, possibly negated: `5`, `-3`, `'a'`, `true`, `"s"`.
    fn parse_lit_pat_expr(&mut self) -> PResult<Expr> {
        let start = self.peek().span;
        if self.eat(TokenKind::Minus).is_some() {
            if !matches!(self.peek_kind(), TokenKind::Int | TokenKind::Float) {
                return Err(self.error_expected("a numeric literal after `-` in a pattern"));
            }
            let inner = self.parse_lit_pat_expr()?;
            return Ok(Expr {
                id: self.new_id(),
                kind: ExprKind::Unary(UnOp::Neg, Box::new(inner)),
                span: self.span_from(start),
            });
        }
        let lit = self.parse_lit()?;
        Ok(Expr {
            id: self.new_id(),
            kind: ExprKind::Lit(lit),
            span: self.span_from(start),
        })
    }

    /// The rest of `lo..=hi`, `lo..hi` or `lo..`, at the `..=` or `..`.
    fn parse_range_pat(&mut self, lo: Expr) -> PResult<PatKind> {
        let end = if self.eat(TokenKind::DotDotEq).is_some() {
            RangeEnd::Included
        } else {
            self.expect(TokenKind::DotDot)?;
            RangeEnd::Excluded
        };
        if !self.at_range_bound() {
            if end == RangeEnd::Included {
                let span = self.prev_span();
                let d = Diagnostic::error(
                    ErrorCode::EXPECTED_EXPRESSION,
                    "inclusive range with no end",
                )
                .with_label(span, "an inclusive range needs an end")
                .with_note("use `..` instead of `..=` for a range with no end");
                return Err(self.emit(d));
            }
            // `lo..`: a half-open range (#49).
            return Ok(PatKind::Range(Some(Box::new(lo)), None, end));
        }
        let hi = self.parse_range_bound()?;
        Ok(PatKind::Range(Some(Box::new(lo)), Some(Box::new(hi)), end))
    }

    /// The upper bound of a range pattern: a path or a (possibly negated) literal.
    fn parse_range_bound(&mut self) -> PResult<Expr> {
        if matches!(self.peek_kind(), TokenKind::Ident | TokenKind::SelfType) {
            let path = self.parse_path()?;
            Ok(self.path_expr(path))
        } else {
            self.parse_lit_pat_expr()
        }
    }

    /// `&p`, `&mut p` and `&&p` (two reference patterns), at the `&` or `&&` (#49). The
    /// subpattern may not be a range without parentheses (`&0..=5` is ambiguous, as in
    /// rustc); `&mut x` is a `&mut` pattern around the binding `x`.
    fn parse_ref_pat(&mut self, outer_id: NodeId) -> PResult<Pat> {
        let tok = self.bump();
        let start = tok.span;
        let double = tok.kind == TokenKind::AndAnd;
        let inner_id = if double { Some(self.new_id()) } else { None };
        let mutable = self.eat(TokenKind::Mut).is_some();
        let sub_start = self.peek().span;
        let parenthesised = self.at(TokenKind::LParen);
        let sub = self.nested(|p| {
            if double {
                p.descend()?;
            }
            p.parse_pat()
        })?;
        if matches!(sub.kind, PatKind::Range(..)) && !parenthesised {
            let d = Diagnostic::error(
                ErrorCode::UNEXPECTED_TOKEN,
                "the range pattern here has ambiguous interpretation",
            )
            .with_label(
                sub_start.to(sub.span),
                "add parentheses to clarify the precedence",
            );
            return Err(self.emit(d));
        }
        let mut pat = Pat {
            id: inner_id.unwrap_or(outer_id),
            kind: PatKind::Ref {
                pat: Box::new(sub),
                mutable,
            },
            span: self.span_from(start),
        };
        if double {
            pat.span = Span::new(self.file, start.start + 1, pat.span.end);
            pat = Pat {
                id: outer_id,
                kind: PatKind::Ref {
                    pat: Box::new(pat),
                    mutable: false,
                },
                span: self.span_from(start),
            };
        }
        Ok(pat)
    }

    /// `Path { X, Y: p, ref Z, .. }`, at the `{`.
    fn parse_struct_pat(&mut self, path: Path) -> PResult<PatKind> {
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        let mut rest = None;
        while !self.at(TokenKind::RBrace) && !self.at_eof() {
            if self.at(TokenKind::DotDot) {
                rest = Some(self.bump().span);
                break;
            }
            let fstart = self.peek().span;
            let shorthand_mode = match self.peek_kind() {
                TokenKind::Ref => {
                    self.bump();
                    Some((true, self.eat(TokenKind::Mut).is_some()))
                }
                TokenKind::Mut => {
                    self.bump();
                    Some((false, true))
                }
                _ => None,
            };
            let name = self.parse_ident()?;
            let (pat, shorthand) = match shorthand_mode {
                None if self.eat(TokenKind::Colon).is_some() => (self.parse_pat_or()?, false),
                mode => {
                    let (by_ref, mutable) = mode.unwrap_or((false, false));
                    let pat = Pat {
                        id: self.new_id(),
                        kind: PatKind::Ident {
                            by_ref,
                            mutable,
                            name: name.clone(),
                            sub: None,
                        },
                        span: self.span_from(fstart),
                    };
                    (pat, true)
                }
            };
            fields.push(FieldPat {
                name,
                pat,
                shorthand,
                span: self.span_from(fstart),
            });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(PatKind::Struct { path, fields, rest })
    }

    // ---- blocks and statements --------------------------------------------------------------

    /// Braces lift the struct-literal restriction of an enclosing `if`/`while` condition,
    /// also for the block of an `if` or loop nested in one: `while if c { A {}; } {}`.
    pub(crate) fn parse_block(&mut self) -> PResult<Block> {
        let saved = (self.no_struct_literal, self.let_allowed);
        self.no_struct_literal = false;
        self.let_allowed = false;
        let block = self.parse_block_inner();
        (self.no_struct_literal, self.let_allowed) = saved;
        block
    }

    fn parse_block_inner(&mut self) -> PResult<Block> {
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
                let pat = self.parse_pat_no_top_or("`let` bindings")?;
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
                let els = match &init {
                    Some(init) if self.at(TokenKind::Else) => {
                        if init_ends_badly_for_let_else(init) {
                            let d = Diagnostic::error(
                                ErrorCode::UNEXPECTED_TOKEN,
                                "the initialiser of a `let`-`else` cannot end with `}` or a lazy boolean operator",
                            )
                            .with_label(init.span, "wrap this expression in parentheses");
                            return Err(self.emit(d));
                        }
                        self.bump();
                        Some(Box::new(self.parse_block()?))
                    }
                    _ => None,
                };
                if !self.at(TokenKind::Semi) {
                    return Err(self.error_expected_after_prev("`;`"));
                }
                self.bump();
                Ok(StmtOrTail::Stmt(Stmt {
                    id,
                    kind: StmtKind::Let(Box::new(LetStmt { pat, ty, init, els })),
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

/// rustc rejects `let p = { .. } else { .. };` and `let p = a && b else { .. };`: the
/// initialiser of a `let`-`else` may not end with `}` or be a lazy boolean.
fn init_ends_badly_for_let_else(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Binary(sabiko_ast::BinOp::And | sabiko_ast::BinOp::Or, ..) => true,
        _ => ends_with_brace(e),
    }
}

fn ends_with_brace(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Block(_)
        | ExprKind::If { .. }
        | ExprKind::While { .. }
        | ExprKind::Loop { .. }
        | ExprKind::Match { .. }
        | ExprKind::Struct { .. } => true,
        ExprKind::Binary(_, _, r)
        | ExprKind::Assign(_, r)
        | ExprKind::DestructuringAssign(_, r)
        | ExprKind::AssignOp(_, _, r)
        | ExprKind::Unary(_, r)
        | ExprKind::Ref { expr: r, .. }
        | ExprKind::Deref(r)
        | ExprKind::Break(Some(r))
        | ExprKind::Return(Some(r)) => ends_with_brace(r),
        _ => false,
    }
}

pub(crate) enum StmtOrTail {
    Stmt(Stmt),
    Tail(sabiko_ast::Expr),
}
