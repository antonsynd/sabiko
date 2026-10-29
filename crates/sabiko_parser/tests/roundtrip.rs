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
    Path {
        segments: vec![ident(name)],
        span: SP,
    }
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

fn lit() -> impl Strategy<Value = Lit> {
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
        any::<bool>().prop_map(Lit::Bool),
        any::<char>().prop_map(Lit::Char),
        ".*".prop_map(Lit::Str),
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
            (any::<bool>(), inner.clone()).prop_map(|(mutable, t)| Type {
                id: ID,
                kind: TypeKind::Ref {
                    mutable,
                    inner: Box::new(t),
                },
                span: SP,
            }),
            prop::collection::vec(inner, 1..4).prop_map(|elems| Type {
                id: ID,
                kind: TypeKind::Tuple(elems),
                span: SP,
            }),
        ]
    })
}

fn pat() -> impl Strategy<Value = Pat> {
    prop_oneof![
        Just(PatKind::Wild),
        (any::<bool>(), name()).prop_map(|(mutable, n)| PatKind::Ident {
            mutable,
            name: ident(&n)
        }),
    ]
    .prop_map(|kind| Pat {
        id: ID,
        kind,
        span: SP,
    })
    .prop_recursive(3, 8, 3, |inner| {
        prop::collection::vec(inner, 0..4).prop_map(|elems| Pat {
            id: ID,
            kind: PatKind::Tuple(elems),
            span: SP,
        })
    })
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

/// Expressions. `in_cond` forbids bare struct literals (they would be parenthesised by
/// the printer, producing a `Paren` node that is stripped anyway, so this just keeps the
/// generated trees closer to what the printer emits).
fn expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        lit().prop_map(|l| mk(ExprKind::Lit(l))),
        name().prop_map(|n| mk(ExprKind::Path(path(&n)))),
        Just(mk(ExprKind::Unit)),
        Just(mk(ExprKind::Continue)),
    ];
    leaf.prop_recursive(4, 24, 3, |inner| {
        let block = block_with(inner.clone());
        prop_oneof![
            (prop_oneof![Just(UnOp::Neg), Just(UnOp::Not)], inner.clone())
                .prop_map(|(op, e)| mk(ExprKind::Unary(op, Box::new(e)))),
            (binop(), inner.clone(), inner.clone()).prop_map(|(op, l, r)| mk(ExprKind::Binary(
                op,
                Box::new(l),
                Box::new(r)
            ))),
            (inner.clone(), inner.clone())
                .prop_map(|(l, r)| mk(ExprKind::Assign(Box::new(l), Box::new(r)))),
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
                prop::collection::vec(inner.clone(), 0..3)
            )
                .prop_map(|(r, m, args)| mk(ExprKind::MethodCall {
                    receiver: Box::new(r),
                    method: ident(&m),
                    args
                })),
            (inner.clone(), field_name())
                .prop_map(|(e, f)| mk(ExprKind::Field(Box::new(e), ident(&f)))),
            prop::collection::vec(inner.clone(), 1..4).prop_map(|elems| mk(ExprKind::Tuple(elems))),
            (inner.clone(), inner.clone())
                .prop_map(|(e, i)| mk(ExprKind::Index(Box::new(e), Box::new(i)))),
            (any::<bool>(), inner.clone()).prop_map(|(mutable, e)| mk(ExprKind::Ref {
                mutable,
                expr: Box::new(e)
            })),
            inner.clone().prop_map(|e| mk(ExprKind::Deref(Box::new(e)))),
            (inner.clone(), ty()).prop_map(|(e, t)| mk(ExprKind::Cast(Box::new(e), t))),
            block.clone().prop_map(|b| mk(ExprKind::Block(b))),
            (
                inner.clone(),
                block.clone(),
                prop::option::of(block.clone())
            )
                .prop_map(|(c, t, e)| mk(ExprKind::If {
                    cond: Box::new(c),
                    then: t,
                    els: e.map(|b| Box::new(mk(ExprKind::Block(b)))),
                })),
            (inner.clone(), block.clone()).prop_map(|(c, b)| mk(ExprKind::While {
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
            prop::option::of(inner.clone())
        )
            .prop_map(|(pat, ty, init)| StmtKind::Let(LetStmt { pat, ty, init })),
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

fn item() -> impl Strategy<Value = Item> {
    let vis = prop_oneof![Just(Visibility::Private), Just(Visibility::Public)];
    let docs = prop::collection::vec(" doc [a-z ]*", 0..2);
    let attrs = prop::collection::vec(
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
    );
    let kind = prop_oneof![
        (
            name(),
            prop::collection::vec((pat(), ty()), 0..3),
            prop::option::of(ty()),
            block_with(expr().boxed())
        )
            .prop_map(|(n, params, ret, body)| ItemKind::Fn(FnDecl {
                name: ident(&n),
                params: params
                    .into_iter()
                    .map(|(pat, ty)| Param {
                        id: ID,
                        pat,
                        ty,
                        span: SP
                    })
                    .collect(),
                ret,
                body,
            })),
        (
            name(),
            prop::option::of(prop::collection::vec((name(), ty(), any::<bool>()), 0..3))
        )
            .prop_map(|(n, fields)| ItemKind::Struct(StructDecl {
                name: ident(&n),
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
            value
        })),
    ];
    (attrs, docs, vis, kind).prop_map(|(attrs, docs, vis, kind)| Item {
        id: ID,
        attrs,
        docs,
        vis,
        kind,
        span: SP,
    })
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
        ExprKind::Lit(_) | ExprKind::Path(_) | ExprKind::Unit | ExprKind::Continue => {}
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
        ExprKind::Tuple(elems) => elems.iter_mut().for_each(strip_parens_expr),
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
    }
}

fn strip_parens_block(b: &mut Block) {
    for s in &mut b.stmts {
        match &mut s.kind {
            StmtKind::Let(l) => {
                if let Some(init) = &mut l.init {
                    strip_parens_expr(init);
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
        match &mut item.kind {
            ItemKind::Fn(func) => strip_parens_block(&mut func.body),
            ItemKind::Const(c) => strip_parens_expr(&mut c.value),
            ItemKind::Struct(_) => {}
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn pretty_then_parse_is_identity(mut file in source_file()) {
        strip_parens_file(&mut file);
        let src = pretty(&file);
        let mut sink = DiagnosticSink::new();
        let mut parsed = sabiko_parser::parse_file(FileId(0), &src, &mut sink);
        prop_assert!(!sink.has_errors(), "diagnostics on printed source:\n{src}\n{:#?}", sink.diagnostics());
        strip_parens_file(&mut parsed);
        prop_assert_eq!(dump(&parsed), dump(&file), "source:\n{}", src);
        // Fixpoint: printing the parsed tree reproduces the text.
        prop_assert_eq!(pretty(&parsed), src);
    }
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
        let file = SourceFile {
            docs: vec![],
            items: vec![Item {
                id: ID,
                attrs: vec![],
                docs: vec![],
                vis: Visibility::Private,
                kind: ItemKind::Fn(FnDecl {
                    name: ident("f"),
                    params: vec![],
                    ret: None,
                    body: Block {
                        id: ID,
                        stmts: vec![],
                        tail: Some(Box::new(tail)),
                        span: SP,
                    },
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
