//! Property test: pretty-printing any AST and parsing it back yields the same tree
//! (modulo the `Paren` nodes the printer inserts for precedence, which are stripped
//! before comparison), and the printer is a fixpoint of `parse`.

use proptest::prelude::*;
use sabiko_ast::dump::dump;
use sabiko_ast::pretty::pretty;
use sabiko_ast::*;
use sabiko_diagnostics::DiagnosticSink;
use sabiko_span::{FileId, Span};

const SP: Span = Span {
    file: FileId(0),
    start: 0,
    end: 0,
};
const ID: NodeId = NodeId::DUMMY;

fn ident(name: &str) -> Ident {
    Ident {
        name: name.to_owned(),
        span: SP,
    }
}

fn path(name: &str) -> Path {
    Path::single(ident(name))
}

fn leaf_ty(kind: TypeKind) -> Type {
    Type {
        id: ID,
        kind,
        span: SP,
    }
}

/// Generic arguments for segment `segment`: `()` or a one-segment type path.
fn generic_args(segment: usize) -> impl Strategy<Value = GenericArgs> {
    prop::collection::vec(
        prop_oneof![
            Just(leaf_ty(TypeKind::Unit)),
            Just(leaf_ty(TypeKind::Infer)),
            name().prop_map(|n| leaf_ty(TypeKind::Path(path(&n)))),
        ],
        0..3,
    )
    .prop_map(move |args| GenericArgs {
        segment,
        args,
        span: SP,
    })
}

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_owned()),
        Just("b".to_owned()),
        Just("Foo".to_owned()),
        Just("x1".to_owned()),
        Just("_y".to_owned())
    ]
}

/// An integer or float literal, the literals a pattern may negate.
fn num_lit() -> impl Strategy<Value = Lit> {
    prop_oneof![
        (
            any::<u128>(),
            prop::option::of(prop_oneof![
                Just(IntSuffix::I32),
                Just(IntSuffix::U8),
                Just(IntSuffix::Usize)
            ])
        )
            .prop_map(|(value, suffix)| Lit::Int { value, suffix }),
        (
            1u32..1000,
            0u32..100,
            prop::option::of(prop_oneof![Just(FloatSuffix::F32), Just(FloatSuffix::F64)])
        )
            .prop_map(|(a, b, suffix)| Lit::Float {
                text: format!("{a}.{b}"),
                suffix
            }),
    ]
}

fn lit() -> impl Strategy<Value = Lit> {
    prop_oneof![
        2 => num_lit(),
        1 => any::<bool>().prop_map(Lit::Bool),
        1 => any::<char>().prop_map(Lit::Char),
        1 => ".*".prop_map(Lit::Str),
    ]
}

fn ty() -> impl Strategy<Value = Type> {
    let leaf = prop_oneof![
        name().prop_map(|n| TypeKind::Path(path(&n))),
        Just(TypeKind::Unit),
        Just(TypeKind::Infer),
    ];
    leaf.prop_map(|kind| Type {
        id: ID,
        kind,
        span: SP,
    })
    .prop_recursive(3, 8, 2, |inner| {
        prop_oneof![
            (name(), prop::collection::vec(inner.clone(), 0..3)).prop_map(|(n, args)| Type {
                id: ID,
                kind: TypeKind::Path(Path {
                    segments: vec![ident(&n)],
                    generic_args: vec![GenericArgs {
                        segment: 0,
                        args,
                        span: SP,
                    }],
                    span: SP,
                }),
                span: SP,
            }),
            (any::<bool>(), inner.clone()).prop_map(|(mutable, t)| Type {
                id: ID,
                kind: TypeKind::Ref {
                    mutable,
                    inner: Box::new(t),
                },
                span: SP,
            }),
            prop::collection::vec(inner.clone(), 1..4).prop_map(|elems| Type {
                id: ID,
                kind: TypeKind::Tuple(elems),
                span: SP,
            }),
            (inner.clone(), 0u128..40).prop_map(|(t, n)| Type {
                id: ID,
                kind: TypeKind::Array(
                    Box::new(t),
                    Box::new(mk(ExprKind::Lit(Lit::Int {
                        value: n,
                        suffix: None
                    })))
                ),
                span: SP,
            }),
            inner.prop_map(|t| Type {
                id: ID,
                kind: TypeKind::Slice(Box::new(t)),
                span: SP,
            }),
        ]
    })
}

/// A path of one to `max` segments: `a`, `Foo::b`.
fn path_upto(max: usize) -> impl Strategy<Value = Path> {
    (
        prop::collection::vec(name(), 1..=max),
        prop::option::of(generic_args(0)),
    )
        .prop_map(|(segs, args)| Path {
            generic_args: args
                .into_iter()
                .map(|a| GenericArgs {
                    segment: segs.len() - 1,
                    ..a
                })
                .collect(),
            segments: segs.iter().map(|s| ident(s)).collect(),
            span: SP,
        })
}

fn neg(l: Lit) -> Expr {
    mk(ExprKind::Unary(UnOp::Neg, Box::new(mk(ExprKind::Lit(l)))))
}

/// The expression of a literal pattern: any literal, or a negated number (`-5`).
fn lit_pat_expr() -> impl Strategy<Value = Expr> {
    prop_oneof![
        lit().prop_map(|l| mk(ExprKind::Lit(l))),
        num_lit().prop_map(neg)
    ]
}

/// A bound of a range pattern: a number (possibly negated), a character or a path.
fn range_bound() -> impl Strategy<Value = Expr> {
    prop_oneof![
        num_lit().prop_map(|l| mk(ExprKind::Lit(l))),
        num_lit().prop_map(neg),
        any::<char>().prop_map(|c| mk(ExprKind::Lit(Lit::Char(c)))),
        path_upto(2).prop_map(|p| mk(ExprKind::Path(p))),
    ]
}

fn mk_pat(kind: PatKind) -> Pat {
    Pat {
        id: ID,
        kind,
        span: SP,
    }
}

/// Patterns of every kind. `..` (`Rest`) appears only as an element of a tuple or
/// tuple-struct pattern, and a one-segment `Path` would read back as a binding, so path
/// patterns have two segments. Or-patterns may appear anywhere: the printer
/// parenthesises them where the grammar needs it (`let`, parameters, `@`).
fn pat() -> BoxedStrategy<Pat> {
    let leaf = prop_oneof![
        Just(PatKind::Wild),
        (any::<bool>(), any::<bool>(), name()).prop_map(|(by_ref, mutable, n)| PatKind::Ident {
            by_ref,
            mutable,
            name: ident(&n),
            sub: None,
        }),
        (name(), name()).prop_map(|(a, b)| PatKind::Path(Path {
            segments: vec![ident(&a), ident(&b)],
            generic_args: Vec::new(),
            span: SP,
        })),
        lit_pat_expr().prop_map(|e| PatKind::Lit(Box::new(e))),
        (
            range_bound(),
            range_bound(),
            prop_oneof![Just(RangeEnd::Included), Just(RangeEnd::Excluded)]
        )
            .prop_map(|(lo, hi, end)| PatKind::Range(Box::new(lo), Box::new(hi), end)),
    ]
    .prop_map(mk_pat);
    leaf.prop_recursive(3, 12, 3, |inner| {
        // Elements of a tuple or tuple-struct pattern, with a `..` somewhere or not.
        let elems = (
            prop::collection::vec(inner.clone(), 0..3),
            prop::option::of(0usize..3),
        )
            .prop_map(|(mut elems, rest)| {
                if let Some(i) = rest {
                    elems.insert(i.min(elems.len()), mk_pat(PatKind::Rest));
                }
                elems
            });
        let field = prop_oneof![
            // `X`, `ref X`, `mut X`, `ref mut X`
            (name(), any::<bool>(), any::<bool>()).prop_map(|(n, by_ref, mutable)| FieldPat {
                name: ident(&n),
                pat: mk_pat(PatKind::Ident {
                    by_ref,
                    mutable,
                    name: ident(&n),
                    sub: None,
                }),
                shorthand: true,
                span: SP,
            }),
            // `X: pat`
            (name(), inner.clone()).prop_map(|(n, pat)| FieldPat {
                name: ident(&n),
                pat,
                shorthand: false,
                span: SP,
            }),
        ];
        prop_oneof![
            elems.clone().prop_map(PatKind::Tuple),
            (path_upto(2), elems).prop_map(|(p, elems)| PatKind::TupleStruct(p, elems)),
            (
                path_upto(2),
                prop::collection::vec(field, 0..3),
                any::<bool>()
            )
                .prop_map(|(path, fields, rest)| PatKind::Struct {
                    path,
                    fields,
                    rest: rest.then_some(SP),
                }),
            prop::collection::vec(inner.clone(), 2..4).prop_map(PatKind::Or),
            (any::<bool>(), any::<bool>(), name(), inner).prop_map(|(by_ref, mutable, n, sub)| {
                PatKind::Ident {
                    by_ref,
                    mutable,
                    name: ident(&n),
                    sub: Some(Box::new(sub)),
                }
            }),
        ]
        .prop_map(mk_pat)
    })
    .boxed()
}

/// Whether the parser reads `e`, on the left of `=`, as a destructuring pattern (see
/// `Parser::is_destructuring`): such an `Assign` is a `DestructuringAssign` once parsed.
fn destructures(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Paren(inner) => destructures(inner),
        ExprKind::Tuple(_) | ExprKind::Unit | ExprKind::Struct { .. } | ExprKind::Array(_) => true,
        ExprKind::Call { callee, .. } => {
            matches!(&callee.kind, ExprKind::Path(p) if p.segments.len() > 1)
        }
        _ => false,
    }
}

/// The left side of a destructuring assignment: tuples, tuple-variant and struct patterns
/// of places (any expression that does not itself destructure), `_` and `..`; the top is
/// never a lone place, which would be a plain assignment.
fn assignee_pat(place: impl Strategy<Value = Expr> + Clone + 'static) -> BoxedStrategy<Pat> {
    let leaf = prop_oneof![
        1 => Just(PatKind::Wild),
        3 => place
            .prop_filter("a place, not a pattern", |e| !destructures(e))
            .prop_map(|e| PatKind::Assignee(Box::new(e))),
    ]
    .prop_map(mk_pat);
    let structural = leaf.prop_recursive(2, 8, 3, |inner| {
        let elems = (
            prop::collection::vec(inner.clone(), 0..3),
            prop::option::of(0usize..3),
        )
            .prop_map(|(mut elems, rest)| {
                if let Some(i) = rest {
                    elems.insert(i.min(elems.len()), mk_pat(PatKind::Rest));
                }
                elems
            });
        let field = (name(), inner).prop_map(|(n, pat)| FieldPat {
            name: ident(&n),
            pat,
            shorthand: false,
            span: SP,
        });
        prop_oneof![
            elems.clone().prop_map(PatKind::Tuple),
            ((name(), name()), elems).prop_map(|((a, b), elems)| PatKind::TupleStruct(
                Path {
                    segments: vec![ident(&a), ident(&b)],
                    generic_args: Vec::new(),
                    span: SP,
                },
                elems
            )),
            (
                path_upto(2),
                prop::collection::vec(field, 0..3),
                any::<bool>()
            )
                .prop_map(|(path, fields, rest)| PatKind::Struct {
                    path,
                    fields,
                    rest: rest.then_some(SP),
                }),
        ]
        .prop_map(mk_pat)
    });
    structural
        .prop_map(|p| match p.kind {
            PatKind::Assignee(_) => mk_pat(PatKind::Tuple(vec![p])),
            _ => p,
        })
        .boxed()
}

/// A field name: an identifier, or a tuple index.
fn field_name() -> impl Strategy<Value = String> {
    prop_oneof![
        name(),
        Just("0".to_owned()),
        Just("1".to_owned()),
        Just("12".to_owned())
    ]
}

fn binop() -> impl Strategy<Value = BinOp> {
    prop_oneof![
        Just(BinOp::Add),
        Just(BinOp::Sub),
        Just(BinOp::Mul),
        Just(BinOp::Div),
        Just(BinOp::Rem),
        Just(BinOp::And),
        Just(BinOp::Or),
        Just(BinOp::BitAnd),
        Just(BinOp::BitOr),
        Just(BinOp::BitXor),
        Just(BinOp::Shl),
        Just(BinOp::Shr),
        Just(BinOp::Eq),
        Just(BinOp::Ne),
        Just(BinOp::Lt),
        Just(BinOp::Le),
        Just(BinOp::Gt),
        Just(BinOp::Ge),
    ]
}

fn arith_op() -> impl Strategy<Value = BinOp> {
    prop_oneof![
        Just(BinOp::Add),
        Just(BinOp::Sub),
        Just(BinOp::Mul),
        Just(BinOp::BitAnd),
        Just(BinOp::Shl)
    ]
}

fn mk(kind: ExprKind) -> Expr {
    Expr {
        id: ID,
        kind,
        span: SP,
    }
}

/// `e[lo..hi]` and the other range forms: a range only as an index (#10); an inclusive one
/// always has an end.
fn range_index(inner: impl Strategy<Value = Expr> + Clone) -> impl Strategy<Value = Expr> {
    (
        inner.clone(),
        prop::option::of(inner.clone()),
        prop::option::of(inner.clone()),
        any::<bool>(),
    )
        .prop_map(|(e, lo, hi, inclusive)| {
            let end = if inclusive && hi.is_some() {
                RangeEnd::Included
            } else {
                RangeEnd::Excluded
            };
            let range = mk(ExprKind::Range(lo.map(Box::new), hi.map(Box::new), end));
            mk(ExprKind::Index(Box::new(e), Box::new(range)))
        })
}

/// Expressions. `in_cond` forbids bare struct literals (they would be parenthesised by
/// the printer, producing a `Paren` node that is stripped anyway, so this just keeps the
/// generated trees closer to what the printer emits).
#[allow(clippy::too_many_lines)]
fn expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        lit().prop_map(|l| mk(ExprKind::Lit(l))),
        name().prop_map(|n| mk(ExprKind::Path(path(&n)))),
        Just(mk(ExprKind::Unit)),
        Just(mk(ExprKind::Continue)),
    ];
    leaf.prop_recursive(4, 24, 3, |inner| {
        let block = block_with(inner.clone());
        // The condition of an `if` or `while`: an expression, or a `let` (`if let`,
        // `while let`), which is only allowed as the whole condition.
        let cond = prop_oneof![
            3 => inner.clone(),
            1 => (pat(), inner.clone())
                .prop_map(|(p, e)| mk(ExprKind::Let(Box::new(p), Box::new(e)))),
        ];
        let arm = (pat(), prop::option::of(inner.clone()), inner.clone()).prop_map(
            |(pat, guard, body)| Arm {
                id: ID,
                pat,
                guard,
                body,
                span: SP,
            },
        );
        prop_oneof![
            (inner.clone(), prop::collection::vec(arm, 0..3)).prop_map(|(s, arms)| mk(
                ExprKind::Match {
                    scrutinee: Box::new(s),
                    arms
                }
            )),
            (prop_oneof![Just(UnOp::Neg), Just(UnOp::Not)], inner.clone())
                .prop_map(|(op, e)| mk(ExprKind::Unary(op, Box::new(e)))),
            (binop(), inner.clone(), inner.clone()).prop_map(|(op, l, r)| mk(ExprKind::Binary(
                op,
                Box::new(l),
                Box::new(r)
            ))),
            (inner.clone(), inner.clone())
                .prop_filter("a plain assignment", |(l, _)| !destructures(l))
                .prop_map(|(l, r)| mk(ExprKind::Assign(Box::new(l), Box::new(r)))),
            (assignee_pat(inner.clone()), inner.clone())
                .prop_map(|(p, r)| mk(ExprKind::DestructuringAssign(Box::new(p), Box::new(r)))),
            (arith_op(), inner.clone(), inner.clone())
                .prop_map(|(op, l, r)| mk(ExprKind::AssignOp(op, Box::new(l), Box::new(r)))),
            (inner.clone(), prop::collection::vec(inner.clone(), 0..3)).prop_map(
                |(callee, args)| mk(ExprKind::Call {
                    callee: Box::new(callee),
                    args
                })
            ),
            (
                inner.clone(),
                name(),
                prop::option::of(generic_args(0)),
                prop::collection::vec(inner.clone(), 0..3)
            )
                .prop_map(|(r, m, generic_args, args)| mk(ExprKind::MethodCall {
                    receiver: Box::new(r),
                    method: ident(&m),
                    generic_args: generic_args.map(Box::new),
                    args
                })),
            (inner.clone(), field_name())
                .prop_map(|(e, f)| mk(ExprKind::Field(Box::new(e), ident(&f)))),
            prop::collection::vec(inner.clone(), 1..4).prop_map(|elems| mk(ExprKind::Tuple(elems))),
            prop::collection::vec(inner.clone(), 0..4).prop_map(|elems| mk(ExprKind::Array(elems))),
            (inner.clone(), inner.clone())
                .prop_map(|(v, n)| mk(ExprKind::Repeat(Box::new(v), Box::new(n)))),
            (inner.clone(), inner.clone())
                .prop_map(|(e, i)| mk(ExprKind::Index(Box::new(e), Box::new(i)))),
            range_index(inner.clone()),
            (any::<bool>(), inner.clone()).prop_map(|(mutable, e)| mk(ExprKind::Ref {
                mutable,
                expr: Box::new(e)
            })),
            inner.clone().prop_map(|e| mk(ExprKind::Deref(Box::new(e)))),
            (inner.clone(), ty()).prop_map(|(e, t)| mk(ExprKind::Cast(Box::new(e), t))),
            block.clone().prop_map(|b| mk(ExprKind::Block(b))),
            (cond.clone(), block.clone(), prop::option::of(block.clone())).prop_map(
                |(c, t, e)| mk(ExprKind::If {
                    cond: Box::new(c),
                    then: t,
                    els: e.map(|b| Box::new(mk(ExprKind::Block(b)))),
                })
            ),
            (cond, block.clone()).prop_map(|(c, b)| mk(ExprKind::While {
                cond: Box::new(c),
                body: b
            })),
            block.prop_map(|b| mk(ExprKind::Loop { body: b })),
            prop::option::of(inner.clone()).prop_map(|e| mk(ExprKind::Break(e.map(Box::new)))),
            prop::option::of(inner.clone()).prop_map(|e| mk(ExprKind::Return(e.map(Box::new)))),
            (name(), prop::collection::vec((name(), inner), 0..3)).prop_map(|(p, fields)| mk(
                ExprKind::Struct {
                    path: path(&p),
                    fields: fields
                        .into_iter()
                        .map(|(n, v)| FieldInit {
                            name: ident(&n),
                            value: v,
                            span: SP
                        })
                        .collect(),
                    rest: None,
                }
            )),
        ]
    })
}

fn block_with(inner: BoxedStrategy<Expr>) -> BoxedStrategy<Block> {
    let stmt = prop_oneof![
        Just(StmtKind::Empty),
        (
            pat(),
            prop::option::of(ty()),
            prop::option::of(inner.clone()),
            // The `else` block of a `let`-`else`: `{}` or `{ tail }`.
            prop::option::of(prop::option::of(inner.clone()).prop_map(|tail| Block {
                id: ID,
                stmts: vec![],
                tail: tail.map(Box::new),
                span: SP,
            }))
        )
            .prop_map(|(pat, ty, init, els)| StmtKind::Let(Box::new(LetStmt {
                pat,
                ty,
                // `let`-`else` needs an initialiser.
                els: els.filter(|_| init.is_some()).map(Box::new),
                init,
            }))),
        // Block-like expressions may omit the semicolon; others may not.
        (inner.clone(), any::<bool>()).prop_map(|(expr, semi)| {
            let semi = semi || !expr.is_block_like();
            StmtKind::Expr { expr, semi }
        }),
    ]
    .prop_map(|kind| Stmt {
        id: ID,
        kind,
        span: SP,
    });
    (prop::collection::vec(stmt, 0..3), prop::option::of(inner))
        .prop_map(|(stmts, tail)| {
            // A block-like statement without `;` absorbs a following lone `;` (as in rustc).
            let mut merged: Vec<Stmt> = Vec::with_capacity(stmts.len());
            for s in stmts {
                if matches!(s.kind, StmtKind::Empty) {
                    if let Some(Stmt {
                        kind:
                            StmtKind::Expr {
                                semi: semi @ false, ..
                            },
                        ..
                    }) = merged.last_mut()
                    {
                        *semi = true;
                        continue;
                    }
                }
                merged.push(s);
            }
            let mut stmts = merged;
            let mut tail = tail.map(Box::new);
            // A final block-like expression statement without `;` *is* the tail.
            if tail.is_none()
                && matches!(
                    stmts.last(),
                    Some(Stmt {
                        kind: StmtKind::Expr { semi: false, .. },
                        ..
                    })
                )
            {
                if let Some(Stmt {
                    kind: StmtKind::Expr { expr, .. },
                    ..
                }) = stmts.pop()
                {
                    tail = Some(Box::new(expr));
                }
            }
            Block {
                id: ID,
                stmts,
                tail,
                span: SP,
            }
        })
        .boxed()
}

fn docs() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(" doc [a-z ]*", 0..2)
}

fn attrs() -> impl Strategy<Value = Vec<Attribute>> {
    prop::collection::vec(
        prop_oneof![
            Just(String::new()),
            Just("(Copy, Clone)".to_owned()),
            Just("(name = \"x\")".to_owned()),
            Just("[a{b(c)}]".to_owned()),
            Just("= \"x\"".to_owned())
        ]
        .prop_map(|args| Attribute {
            path: path("derive"),
            args,
            span: SP,
        }),
        0..2,
    )
}

/// An enum variant: unit, tuple or struct-like, with attributes, doc comments and an
/// optional discriminant. Fields of a struct-like variant may have doc comments but no
/// visibility (rustc's E0449).
fn variant() -> impl Strategy<Value = Variant> {
    let field = (docs(), name(), ty()).prop_map(|(docs, n, ty)| FieldDef {
        id: ID,
        docs,
        vis: Visibility::Private,
        name: ident(&n),
        ty,
        span: SP,
    });
    let data = prop_oneof![
        Just(VariantData::Unit),
        prop::collection::vec(ty(), 0..3).prop_map(VariantData::Tuple),
        prop::collection::vec(field, 0..3).prop_map(VariantData::Named),
    ];
    (attrs(), docs(), name(), data, prop::option::of(expr())).prop_map(
        |(attrs, docs, n, data, discriminant)| Variant {
            id: ID,
            attrs,
            docs,
            name: ident(&n),
            data,
            discriminant,
            span: SP,
        },
    )
}

/// Zero to two generic parameters, without bounds (a struct's or an enum's).
fn generic_params() -> impl Strategy<Value = Vec<GenericParam>> {
    prop::collection::vec(name(), 0..3).prop_map(|names| {
        names
            .iter()
            .map(|n| GenericParam {
                id: ID,
                name: ident(n),
                bounds: vec![],
            })
            .collect()
    })
}

/// Zero to two traits of a bound.
fn bounds() -> impl Strategy<Value = Vec<TraitRef>> {
    prop::collection::vec(name(), 0..3).prop_map(|names| {
        names
            .iter()
            .map(|n| TraitRef {
                id: ID,
                path: path(n),
            })
            .collect()
    })
}

/// Zero to two generic parameters, each with zero to two bounds.
fn bounded_params() -> impl Strategy<Value = Vec<GenericParam>> {
    prop::collection::vec((name(), bounds()), 0..3).prop_map(|params| {
        params
            .into_iter()
            .map(|(n, bounds)| GenericParam {
                id: ID,
                name: ident(&n),
                bounds,
            })
            .collect()
    })
}

/// A `where` clause of zero to two predicates, each with at least one bound.
fn where_clause() -> impl Strategy<Value = Vec<WherePredicate>> {
    // A path type, not `ty()`: a full type here deepens every function's strategy enough
    // to overflow a test thread's stack on macOS.
    let path_ty = name().prop_map(|n| leaf_ty(TypeKind::Path(path(&n))));
    prop::collection::vec((path_ty, name(), bounds()), 0..3).prop_map(|preds| {
        preds
            .into_iter()
            .map(|(ty, first, mut rest)| {
                rest.insert(
                    0,
                    TraitRef {
                        id: ID,
                        path: path(&first),
                    },
                );
                WherePredicate {
                    ty,
                    bounds: rest,
                    span: SP,
                }
            })
            .collect()
    })
}

/// A member of an `impl` block (`trait_items` false) or a trait: a function, which in a
/// trait may have no body.
/// A member of an `impl` block or a trait: a function or a constant, which only a trait
/// may leave without a body or value.
fn member(in_trait: bool) -> impl Strategy<Value = Item> {
    let kind = prop_oneof![
        (fn_decl(true), any::<bool>()).prop_map(move |(mut f, bodyless)| {
            if in_trait && bodyless {
                f.body = None;
            }
            ItemKind::Fn(f)
        }),
        (name(), ty(), expr(), any::<bool>()).prop_map(move |(n, ty, value, valueless)| {
            ItemKind::Const(ConstDecl {
                name: ident(&n),
                ty,
                value: (!(in_trait && valueless)).then_some(value),
            })
        }),
    ];
    (meta(), kind).prop_map(|((attrs, docs, vis), kind)| Item {
        id: ID,
        attrs,
        docs,
        vis,
        kind,
        span: SP,
    })
}

fn item() -> impl Strategy<Value = Item> {
    let vis = prop_oneof![Just(Visibility::Private), Just(Visibility::Public)];
    let kind = prop_oneof![
        (
            name(),
            generic_params(),
            prop::collection::vec(variant(), 0..4)
        )
            .prop_map(|(n, generics, variants)| {
                ItemKind::Enum(EnumDecl {
                    name: ident(&n),
                    generics,
                    variants,
                })
            }),
        fn_decl(false).prop_map(ItemKind::Fn),
        (
            prop::option::of(name()),
            bounded_params(),
            ty(),
            where_clause(),
            prop::collection::vec(member(false), 0..3)
        )
            .prop_map(|(trait_name, generics, self_ty, where_clause, items)| {
                ItemKind::Impl(ImplDecl {
                    generics,
                    trait_ref: trait_name.map(|n| TraitRef {
                        id: ID,
                        path: path(&n),
                    }),
                    self_ty,
                    where_clause,
                    items,
                })
            }),
        (name(), bounds(), prop::collection::vec(member(true), 0..3)).prop_map(
            |(n, supertraits, items)| ItemKind::Trait(TraitDecl {
                name: ident(&n),
                supertraits,
                items,
            })
        ),
        (
            name(),
            generic_params(),
            prop::option::of(prop::collection::vec((name(), ty(), any::<bool>()), 0..3))
        )
            .prop_map(|(n, generics, fields)| ItemKind::Struct(StructDecl {
                name: ident(&n),
                generics,
                kind: match fields {
                    None => StructKind::Unit,
                    Some(fs) => StructKind::Named(
                        fs.into_iter()
                            .map(|(fname, ty, public)| FieldDef {
                                id: ID,
                                docs: vec![],
                                vis: if public {
                                    Visibility::Public
                                } else {
                                    Visibility::Private
                                },
                                name: ident(&fname),
                                ty,
                                span: SP,
                            })
                            .collect(),
                    ),
                },
            })),
        (name(), ty(), expr()).prop_map(|(n, ty, value)| ItemKind::Const(ConstDecl {
            name: ident(&n),
            ty,
            value: Some(value),
        })),
    ];
    (attrs(), docs(), vis, kind).prop_map(|(attrs, docs, vis, kind)| Item {
        id: ID,
        attrs,
        docs,
        vis,
        kind,
        span: SP,
    })
}

/// Attributes, doc lines and visibility of an `impl` block's member.
fn meta() -> impl Strategy<Value = (Vec<Attribute>, Vec<String>, Visibility)> {
    (
        prop::collection::vec(
            Just(Attribute {
                path: path("inline"),
                args: String::new(),
                span: SP,
            }),
            0..2,
        ),
        prop::collection::vec(" doc [a-z ]*", 0..2),
        prop_oneof![Just(Visibility::Private), Just(Visibility::Public)],
    )
}

/// A function; with `methods`, one that may take `self`, `mut self`, `&self` or
/// `&mut self` first.
fn fn_decl(methods: bool) -> impl Strategy<Value = FnDecl> {
    let self_param = if methods {
        prop::option::of(prop_oneof![
            Just(SelfKind::Value { mutable: false }),
            Just(SelfKind::Value { mutable: true }),
            Just(SelfKind::Ref { mutable: false }),
            Just(SelfKind::Ref { mutable: true }),
        ])
        .boxed()
    } else {
        Just(None).boxed()
    };
    (
        (name(), bounded_params()),
        self_param,
        prop::collection::vec((pat(), ty()), 0..3),
        (prop::option::of(ty()), where_clause()),
        block_with(expr().boxed()),
    )
        .prop_map(
            |((n, generics), self_kind, params, (ret, where_clause), body)| FnDecl {
                name: ident(&n),
                generics,
                self_param: self_kind.map(|kind| SelfParam {
                    id: ID,
                    kind,
                    span: SP,
                }),
                params: params
                    .into_iter()
                    .map(|(pat, ty)| Param {
                        id: ID,
                        pat,
                        ty,
                        span: SP,
                    })
                    .collect(),
                ret,
                where_clause,
                body: Some(body),
            },
        )
}

fn source_file() -> impl Strategy<Value = SourceFile> {
    (
        prop::collection::vec(" module doc", 0..2),
        prop::collection::vec(item(), 0..3),
    )
        .prop_map(|(docs, items)| SourceFile {
            docs,
            items,
            span: SP,
        })
}

/// Removes `Paren` nodes, which carry no structure the dump needs.
fn strip_parens_expr(e: &mut Expr) {
    if let ExprKind::Paren(inner) = &mut e.kind {
        let mut inner = std::mem::replace(inner.as_mut(), mk(ExprKind::Unit));
        strip_parens_expr(&mut inner);
        *e = inner;
        return;
    }
    match &mut e.kind {
        ExprKind::Lit(_)
        | ExprKind::Path(_)
        | ExprKind::Unit
        | ExprKind::Continue
        | ExprKind::Underscore => {}
        ExprKind::Paren(_) => unreachable!(),
        ExprKind::Unary(_, x)
        | ExprKind::Deref(x)
        | ExprKind::Ref { expr: x, .. }
        | ExprKind::Cast(x, _)
        | ExprKind::Field(x, _) => {
            strip_parens_expr(x);
        }
        ExprKind::Binary(_, l, r)
        | ExprKind::Assign(l, r)
        | ExprKind::AssignOp(_, l, r)
        | ExprKind::Index(l, r) => {
            strip_parens_expr(l);
            strip_parens_expr(r);
        }
        ExprKind::Call { callee, args } => {
            strip_parens_expr(callee);
            args.iter_mut().for_each(strip_parens_expr);
        }
        ExprKind::Tuple(elems) | ExprKind::Array(elems) => {
            elems.iter_mut().for_each(strip_parens_expr);
        }
        ExprKind::Repeat(v, n) => {
            strip_parens_expr(v);
            strip_parens_expr(n);
        }
        ExprKind::Range(lo, hi, _) => {
            for bound in [lo, hi].into_iter().flatten() {
                strip_parens_expr(bound);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            strip_parens_expr(receiver);
            args.iter_mut().for_each(strip_parens_expr);
        }
        ExprKind::Block(b) => strip_parens_block(b),
        ExprKind::If { cond, then, els } => {
            strip_parens_expr(cond);
            strip_parens_block(then);
            if let Some(e) = els {
                strip_parens_expr(e);
            }
        }
        ExprKind::While { cond, body } => {
            strip_parens_expr(cond);
            strip_parens_block(body);
        }
        ExprKind::Loop { body } => strip_parens_block(body),
        ExprKind::Break(x) | ExprKind::Return(x) => {
            if let Some(x) = x {
                strip_parens_expr(x);
            }
        }
        ExprKind::Struct { fields, .. } => fields
            .iter_mut()
            .for_each(|f| strip_parens_expr(&mut f.value)),
        ExprKind::Match { scrutinee, arms } => {
            strip_parens_expr(scrutinee);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    strip_parens_expr(g);
                }
                strip_parens_expr(&mut arm.body);
            }
        }
        ExprKind::Let(_, e) => strip_parens_expr(e),
        ExprKind::DestructuringAssign(p, r) => {
            strip_parens_assignees(p);
            strip_parens_expr(r);
        }
    }
}

fn strip_parens_assignees(p: &mut Pat) {
    match &mut p.kind {
        PatKind::Assignee(e) => strip_parens_expr(e),
        PatKind::Tuple(elems) | PatKind::TupleStruct(_, elems) => {
            elems.iter_mut().for_each(strip_parens_assignees);
        }
        PatKind::Struct { fields, .. } => {
            for f in fields {
                strip_parens_assignees(&mut f.pat);
            }
        }
        _ => {}
    }
}

fn strip_parens_block(b: &mut Block) {
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Let(l) => {
                if let Some(init) = &mut l.init {
                    strip_parens_expr(init);
                }
                if let Some(els) = &mut l.els {
                    strip_parens_block(els);
                }
            }
            StmtKind::Expr { expr, .. } => strip_parens_expr(expr),
            StmtKind::Empty => {}
        }
    }
    if let Some(t) = &mut b.tail {
        strip_parens_expr(t);
    }
}

fn strip_parens_file(f: &mut SourceFile) {
    for item in &mut f.items {
        strip_parens_item(item);
    }
}

fn strip_parens_item(item: &mut Item) {
    match &mut item.kind {
        ItemKind::Fn(func) => {
            if let Some(body) = &mut func.body {
                strip_parens_block(body);
            }
        }
        ItemKind::Const(c) => {
            if let Some(value) = &mut c.value {
                strip_parens_expr(value);
            }
        }
        ItemKind::Impl(imp) => imp.items.iter_mut().for_each(strip_parens_item),
        ItemKind::Trait(t) => t.items.iter_mut().for_each(strip_parens_item),
        ItemKind::Enum(e) => e
            .variants
            .iter_mut()
            .filter_map(|v| v.discriminant.as_mut())
            .for_each(strip_parens_expr),
        ItemKind::Struct(_) | ItemKind::Use(_) => {}
    }
}

/// Prints a generated file, parses it back, and checks that the trees agree and that the
/// printed text is a fixpoint. Run on a thread with a stack of its own: the generated
/// trees and the strategies building them are deep enough to overflow a default 2 MiB test
/// thread now and then (debug builds on macOS have the largest frames).
#[test]
fn pretty_then_parse_is_identity() {
    let run = || {
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        let result = runner.run(&source_file(), |mut file| {
            strip_parens_file(&mut file);
            let src = pretty(&file);
            let mut sink = DiagnosticSink::new();
            let mut parsed = sabiko_parser::parse_file(FileId(0), &src, &mut sink);
            prop_assert!(
                !sink.has_errors(),
                "diagnostics on printed source:\n{src}\n{:#?}",
                sink.diagnostics()
            );
            strip_parens_file(&mut parsed);
            prop_assert_eq!(dump(&parsed), dump(&file), "source:\n{}", src);
            // Fixpoint: printing the parsed tree reproduces the text.
            prop_assert_eq!(pretty(&parsed), src);
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    };
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(run)
        .expect("spawn the property test thread")
        .join()
        .expect("the property test passes");
}

/// Parses `src` (which must have no errors), prints it, and checks that the printed text
/// parses back to the same tree and is a fixpoint. Returns the printed text.
fn assert_roundtrips(src: &str) -> String {
    let mut sink = DiagnosticSink::new();
    let mut file = sabiko_parser::parse_file(FileId(0), src, &mut sink);
    assert!(!sink.has_errors(), "{src}: {:#?}", sink.diagnostics());
    strip_parens_file(&mut file);
    let printed = pretty(&file);
    let mut sink = DiagnosticSink::new();
    let mut parsed = sabiko_parser::parse_file(FileId(0), &printed, &mut sink);
    assert!(
        !sink.has_errors(),
        "diagnostics on printed source:\n{printed}\n{:#?}",
        sink.diagnostics()
    );
    strip_parens_file(&mut parsed);
    assert_eq!(dump(&parsed), dump(&file), "printed:\n{printed}");
    assert_eq!(pretty(&parsed), printed, "printer is not a fixpoint");
    printed
}

/// Regressions found when the generator learned enums, patterns, `match`, `if let`,
/// `while let` and `let`-`else` (#8):
/// - the printer dropped the doc comments of a struct-like variant's fields;
/// - `break`/`return` without an operand before the `=>` of a match guard took `=>` for
///   the start of their operand (rustc accepts `_ if break => ..`);
/// - an arm body that starts with a block-like expression but is not one
///   (`if c {} else {}[i]`) was printed bare, and a block-like expression ends an arm as it
///   ends a statement, so the rest no longer parsed (rustc reads it the same way).
#[test]
fn match_and_enum_syntax_roundtrips() {
    let printed = assert_roundtrips("enum E { A { /// width\n W: i32, H: i32 } = 1, B }");
    assert_eq!(
        printed,
        "enum E {\n    A {\n        /// width\n        W: i32,\n        H: i32,\n    } = 1,\n    B,\n}\n"
    );
    assert_roundtrips("fn F() { loop { match 0 { _ if break => 1, _ if return => 2, _ => 3 }; } }");
    let printed =
        assert_roundtrips("fn F(b: bool) -> i32 { match 0 { _ => (if b { a } else { c }[0]), } }");
    assert!(printed.contains("_ => (if b {"), "{printed}");
    let printed = assert_roundtrips("fn F() { match 0 { _ => ({ 1 } + 2), } }");
    assert!(printed.contains("_ => ({"), "{printed}");
    // Every pattern kind, in every position a pattern can take.
    assert_roundtrips(
        "fn F((A | B): T, E::V(ref mut x @ (1 | 2), ..): T) {
            let S { X, ref Y, mut Z, W: (_, ..), .. } = s else { return };
            let (-5..=-1 | 'a'..'z' | E::A..=E::B) = n;
            if let E::V { X: 1.5, Y: \"s\" } | E::U = e {}
            while let (a, .., z) = t {}
            match m { x if x > 0 => {} -1 | 0 => (), E::W() | E::Z {} => return, }
        }",
    );
}

/// Regression: `x as i32 < y` is rejected by the parser (the `<` reads as the start of
/// generic arguments on the type path), so the printer must parenthesise the cast.
#[test]
fn cast_before_lt_roundtrips() {
    fn var(name: &str) -> Expr {
        mk(ExprKind::Path(path(name)))
    }
    fn ty_path(name: &str) -> Type {
        Type {
            id: ID,
            kind: TypeKind::Path(path(name)),
            span: SP,
        }
    }
    let cases: Vec<Expr> = vec![
        // x as i32 < y
        mk(ExprKind::Binary(
            BinOp::Lt,
            Box::new(mk(ExprKind::Cast(Box::new(var("x")), ty_path("i32")))),
            Box::new(var("y")),
        )),
        // a * b as i32 < c
        mk(ExprKind::Binary(
            BinOp::Lt,
            Box::new(mk(ExprKind::Binary(
                BinOp::Mul,
                Box::new(var("a")),
                Box::new(mk(ExprKind::Cast(Box::new(var("b")), ty_path("i32")))),
            ))),
            Box::new(var("c")),
        )),
        // x as u32 <= y
        mk(ExprKind::Binary(
            BinOp::Le,
            Box::new(mk(ExprKind::Cast(Box::new(var("x")), ty_path("u32")))),
            Box::new(var("y")),
        )),
        // x as u32 << y
        mk(ExprKind::Binary(
            BinOp::Shl,
            Box::new(mk(ExprKind::Cast(Box::new(var("x")), ty_path("u32")))),
            Box::new(var("y")),
        )),
    ];
    for tail in cases {
        assert_tail_roundtrips(tail);
    }
}

/// Prints a function whose body is `tail`, parses it back, and checks that the two trees
/// agree and that printing again changes nothing.
fn assert_tail_roundtrips(tail: Expr) {
    let file = SourceFile {
        docs: vec![],
        items: vec![Item {
            id: ID,
            attrs: vec![],
            docs: vec![],
            vis: Visibility::Private,
            kind: ItemKind::Fn(FnDecl {
                name: ident("f"),
                generics: vec![],
                self_param: None,
                params: vec![],
                ret: None,
                where_clause: vec![],
                body: Some(Block {
                    id: ID,
                    stmts: vec![],
                    tail: Some(Box::new(tail)),
                    span: SP,
                }),
            }),
            span: SP,
        }],
        span: SP,
    };
    let src = pretty(&file);
    let mut sink = DiagnosticSink::new();
    let mut parsed = sabiko_parser::parse_file(FileId(0), &src, &mut sink);
    assert!(
        !sink.has_errors(),
        "diagnostics on printed source:\n{src}\n{:#?}",
        sink.diagnostics()
    );
    strip_parens_file(&mut parsed);
    assert_eq!(dump(&parsed), dump(&file), "source:\n{src}");
    assert_eq!(pretty(&parsed), src, "printer is not a fixpoint");
}

/// Regression (nightly fuzz, `parse` target): `#[d s {)]` parsed with args `s {)` and
/// printed as `#[ds {)]`, which re-parses with the path `ds`. Rust's attribute grammar is
/// enforced now; every accepted shape prints back unchanged.
#[test]
fn attribute_args_follow_rust_and_roundtrip() {
    for src in [
        "#[d]\nstruct A;\n",
        "#[d(x, y)]\nstruct A;\n",
        "#[d[a{b(c)}]]\nstruct A;\n",
        "#[d{x}]\nstruct A;\n",
        "#[d = \"x\"]\nstruct A;\n",
        "#[d = f(a, [b])]\nstruct A;\n",
    ] {
        let mut sink = DiagnosticSink::new();
        let file = sabiko_parser::parse_file(FileId(0), src, &mut sink);
        assert!(!sink.has_errors(), "{src}: {:#?}", sink.diagnostics());
        assert_eq!(pretty(&file), src);
    }
    for src in [
        "#[d s {)]struct m{}",
        "#[d s]\nstruct A;\n",
        "#[d(x) y]\nstruct A;\n",
        "#[d(x]]\nstruct A;\n",
        "#[d)]\nstruct A;\n",
        "#[d = (]\nstruct A;\n",
    ] {
        let mut sink = DiagnosticSink::new();
        let _ = sabiko_parser::parse_file(FileId(0), src, &mut sink);
        assert!(sink.has_errors(), "{src} should be rejected");
    }
}

/// Regression (CI, Windows, the round-trip property test): `break (_ = 0)` printed as
/// `break _ = 0`, which neither rustc nor the parser reads as one expression (a `_` cannot
/// start the operand of `break` or `return`).
#[test]
fn jump_operand_starting_with_underscore_roundtrips() {
    // `_ = 0`
    let assign_to_wild = || {
        mk(ExprKind::DestructuringAssign(
            Box::new(mk_pat(PatKind::Wild)),
            Box::new(mk(ExprKind::Lit(Lit::Int {
                value: 0,
                suffix: None,
            }))),
        ))
    };
    let cases = vec![
        // loop { break (_ = 0) }
        mk(ExprKind::Loop {
            body: Block {
                id: ID,
                stmts: vec![],
                tail: Some(Box::new(mk(ExprKind::Break(Some(Box::new(
                    assign_to_wild(),
                )))))),
                span: SP,
            },
        }),
        // return (_ = 0)
        mk(ExprKind::Return(Some(Box::new(assign_to_wild())))),
    ];
    for tail in cases {
        assert_tail_roundtrips(tail);
    }
}
