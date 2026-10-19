//! Error recovery: skipping to the next statement or item boundary while keeping
//! bracket nesting balanced.

use sabiko_lexer::TokenKind;

use crate::Parser;

impl Parser<'_, '_> {
    /// Skips to the end of the current statement: past the next `;` at nesting depth 0,
    /// or up to (not past) a closing `}` that ends the enclosing block.
    ///
    /// A stray `)` or `]` at depth 0 (`let x = );`) belongs to nothing and is consumed, so
    /// that the caller always makes progress: only `}` can end the enclosing block.
    pub(crate) fn recover_to_statement_end(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.peek_kind() {
                TokenKind::Eof => return,
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace if depth == 0 => return,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    if depth > 0 {
                        depth -= 1;
                        if depth == 0 && self.peek_kind() == TokenKind::RBrace {
                            // `match x { }` or `if c { }`: the closing brace completes the
                            // statement; the next token starts a new one.
                            self.bump();
                            return;
                        }
                    }
                }
                TokenKind::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                _ => {}
            }
            self.bump();
        }
    }

    /// After an error inside a delimited sub-expression (call arguments, a struct literal,
    /// a parenthesised or index expression), skips past that construct's own `close`
    /// delimiter so that the statement-level recovery does not mistake it for the end of
    /// the enclosing block or statement. Stops without consuming at end of file, at a `;`
    /// at depth 0, and at a `}` at depth 0 unless `}` is the delimiter being sought (the
    /// construct was probably never closed, and those tokens end the statement or block).
    pub(crate) fn skip_to_closer(&mut self, close: TokenKind) {
        let mut depth = 0i32;
        loop {
            let k = self.peek_kind();
            match k {
                TokenKind::Eof => return,
                TokenKind::Semi if depth == 0 => return,
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    if depth == 0 {
                        if k == close {
                            self.bump();
                        } else if k == TokenKind::RBrace {
                            return;
                        } else {
                            // A stray `)` or `]`: skip it and keep looking.
                            self.bump();
                            continue;
                        }
                        return;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.bump();
        }
    }

    /// Skips to the start of the next item at nesting depth 0 (or end of file).
    pub(crate) fn recover_to_item_start(&mut self) {
        let mut depth = 0i32;
        loop {
            let k = self.peek_kind();
            if k == TokenKind::Eof {
                return;
            }
            if depth == 0
                && self.pos != self.last_error_pos.unwrap_or(usize::MAX)
                && is_item_start(k)
            {
                return;
            }
            match k {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    depth -= 1;
                    if depth <= 0 {
                        self.bump();
                        if depth == 0 {
                            // A closing brace at depth 0 ends the item we were in.
                            return;
                        }
                        depth = 0;
                        continue;
                    }
                }
                _ => {}
            }
            self.bump();
        }
    }
}

pub(crate) fn is_item_start(k: TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Fn
            | TokenKind::Struct
            | TokenKind::Const
            | TokenKind::Pub
            | TokenKind::Hash
            | TokenKind::DocComment
            | TokenKind::Enum
            | TokenKind::Impl
            | TokenKind::Trait
            | TokenKind::Mod
            | TokenKind::Use
            | TokenKind::Extern
            | TokenKind::Static
            | TokenKind::Type
    )
}
