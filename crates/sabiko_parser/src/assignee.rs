//! The left side of a destructuring assignment (`(a, b) = (b, a)`), read as a pattern.
//!
//! The parser reads the left side of `=` as an expression, as rustc does; when that
//! expression destructures (a tuple, a struct literal, a tuple-variant call, `_`), it
//! becomes the pattern of an [`ExprKind::DestructuringAssign`]: tuples, struct literals
//! and variant calls become the matching patterns, `_` a wildcard, a lone `..` element a
//! rest pattern, and every other expression a [`PatKind::Assignee`], a place to assign.
//! Whether an assignee path names a unit struct (a pattern, as in rustc) is the
//! resolver's call.

use sabiko_ast::{Expr, ExprKind, FieldPat, NodeId, Pat, PatKind};
use sabiko_diagnostics::{Diagnostic, ErrorCode};
use sabiko_span::Span;

use crate::{AssigneeOnly, Parser};

impl Parser<'_, '_> {
    /// Whether `lhs`, the left side of `=`, destructures. A call destructures only when
    /// its callee is a path of two or more segments (`E::V(a, b)`): there are no tuple
    /// structs, so `f(a) = 1` stays an (invalid) place, E0070 as in rustc.
    pub(crate) fn is_destructuring(lhs: &Expr) -> bool {
        match &lhs.kind {
            ExprKind::Paren(inner) => Self::is_destructuring(inner) || is_rest(inner),
            ExprKind::Tuple(_)
            | ExprKind::Unit
            | ExprKind::Struct { .. }
            | ExprKind::Underscore
            | ExprKind::Array(_) => true,
            ExprKind::Call { callee, .. } => is_variant_path(callee),
            _ => false,
        }
    }

    /// Reads a destructuring left side as a pattern (see the module docs).
    pub(crate) fn assignee_pat(&mut self, e: Expr) -> Pat {
        let Expr { id, kind, span } = e;
        let kind = match kind {
            ExprKind::Paren(inner) if is_rest(&inner) => {
                // `(..)` is a tuple pattern, not a parenthesised `..`.
                self.consume_assignee_only(inner.id);
                PatKind::Tuple(vec![Pat {
                    id: inner.id,
                    kind: PatKind::Rest,
                    span: inner.span,
                }])
            }
            ExprKind::Paren(inner) if Self::is_destructuring(&inner) => {
                return self.assignee_pat(*inner);
            }
            ExprKind::Underscore => {
                self.consume_assignee_only(id);
                PatKind::Wild
            }
            ExprKind::Unit => PatKind::Tuple(Vec::new()),
            ExprKind::Tuple(elems) => {
                let pats = self.assignee_list(elems);
                self.check_single_rest(&pats, "tuple");
                PatKind::Tuple(pats)
            }
            ExprKind::Call { callee, args } if is_variant_path(&callee) => {
                let ExprKind::Path(path) = callee.kind else {
                    unreachable!("a variant path")
                };
                let elems = self.assignee_list(args);
                self.check_single_rest(&elems, "tuple struct");
                PatKind::TupleStruct(path, elems)
            }
            ExprKind::Struct { path, fields, rest } => {
                if rest.is_some() {
                    self.consume_assignee_only(id);
                }
                let fields = fields
                    .into_iter()
                    .map(|f| {
                        // `S { X }` is `S { X: X }`: the shorthand's path has the name's span.
                        let shorthand = f.value.span == f.name.span
                            && matches!(&f.value.kind, ExprKind::Path(p) if p.is_single());
                        FieldPat {
                            name: f.name,
                            pat: self.assignee_pat_or_place(f.value),
                            shorthand,
                            span: f.span,
                        }
                    })
                    .collect();
                PatKind::Struct { path, fields, rest }
            }
            ExprKind::Array(elems) => {
                // Its `_` and `..` are part of the unsupported pattern, not misplaced.
                let _ = self.assignee_list(elems);
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNSUPPORTED_SYNTAX,
                        "not yet supported: destructuring assignment of an array",
                    )
                    .with_label(span, "not supported in this version of Sabiko")
                    .with_note(
                        "slice patterns are not planned yet; see docs/PLAN.md. Assign each element (`a[0] = ...`) instead",
                    ),
                );
                PatKind::Wild
            }
            kind => {
                let e = Expr { id, kind, span };
                return self.place_pat(e);
            }
        };
        Pat { id, kind, span }
    }

    /// An element of a destructuring left side: a nested pattern, a rest pattern or a place.
    fn assignee_list(&mut self, elems: Vec<Expr>) -> Vec<Pat> {
        elems
            .into_iter()
            .map(|e| {
                if is_rest(&e) {
                    self.consume_assignee_only(e.id);
                    Pat {
                        id: e.id,
                        kind: PatKind::Rest,
                        span: e.span,
                    }
                } else {
                    self.assignee_pat_or_place(e)
                }
            })
            .collect()
    }

    fn assignee_pat_or_place(&mut self, e: Expr) -> Pat {
        if Self::is_destructuring(&e) {
            self.assignee_pat(e)
        } else {
            self.place_pat(e)
        }
    }

    fn place_pat(&mut self, e: Expr) -> Pat {
        let id = self.new_id();
        let span = e.span;
        Pat {
            id,
            kind: PatKind::Assignee(Box::new(e)),
            span,
        }
    }

    /// A tuple or tuple-variant pattern has at most one `..` (rustc: "`..` can only be used
    /// once per tuple pattern").
    pub(crate) fn check_single_rest(&mut self, pats: &[Pat], what: &str) {
        let mut rests = pats.iter().filter(|p| matches!(p.kind, PatKind::Rest));
        let Some(first) = rests.next() else {
            return;
        };
        for again in rests {
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::MULTIPLE_REST_PATTERNS,
                    format!("`..` can only be used once per {what} pattern"),
                )
                .with_label(
                    again.span,
                    format!("can only be used once per {what} pattern"),
                )
                .with_label(first.span, "previously used here"),
            );
        }
    }

    fn consume_assignee_only(&mut self, id: NodeId) {
        self.assignee_only.retain(|(i, _)| *i != id);
    }

    /// Reports the forms tracked since `mark` that no destructuring assignment took.
    pub(crate) fn report_assignee_only(&mut self, mark: usize) {
        for (_, form) in self.assignee_only.split_off(mark) {
            let d = match form {
                AssigneeOnly::Underscore(span) => Diagnostic::error(
                    ErrorCode::MISPLACED_UNDERSCORE,
                    "in expressions, `_` can only be used on the left-hand side of an assignment",
                )
                .with_label(span, "`_` not allowed here"),
                AssigneeOnly::Rest(span) => Diagnostic::error(
                    ErrorCode::UNSUPPORTED_SYNTAX,
                    "not yet supported: range expressions outside an index",
                )
                .with_label(span, "not supported in this version of Sabiko")
                .with_note(
                    "only an index `e[a..b]` is supported until range values (generic types, Phase 6); see docs/PLAN.md",
                ),
                AssigneeOnly::StructRest(span) => Diagnostic::error(
                    ErrorCode::MISSING_STRUCT_BASE,
                    "base expression required after `..`",
                )
                .with_label(Span::point(span.file, span.end), "add a base expression here")
                .with_note(
                    "a struct literal may end in `..` only on the left of an assignment; struct update syntax (`..base`) is not yet supported",
                ),
            };
            self.sink.emit(d);
        }
    }
}

/// A lone `..` element (`ExprKind::Range` with no bounds, which only a tuple or an argument
/// list holds outside an index).
fn is_rest(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Range(None, None, _))
}

fn is_variant_path(callee: &Expr) -> bool {
    matches!(&callee.kind, ExprKind::Path(p) if p.segments.len() > 1)
}
