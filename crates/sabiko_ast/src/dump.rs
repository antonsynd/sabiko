//! Structural, span-free rendering of the AST, one node per line, indented by depth.
//! Used by `--emit=ast` and by the parser's snapshot tests.

use std::fmt::Write as _;

#[allow(clippy::wildcard_imports)]
use crate::*;

#[must_use]
pub fn dump(file: &SourceFile) -> String {
    let mut d = Dumper {
        out: String::new(),
        depth: 0,
    };
    d.file(file);
    d.out
}

#[must_use]
pub fn dump_expr(expr: &Expr) -> String {
    let mut d = Dumper {
        out: String::new(),
        depth: 0,
    };
    d.expr(expr);
    d.out
}

struct Dumper {
    out: String,
    depth: usize,
}

impl Dumper {
    fn line(&mut self, s: &str) {
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn nested(&mut self, f: impl FnOnce(&mut Self)) {
        self.depth += 1;
        f(self);
        self.depth -= 1;
    }

    fn file(&mut self, file: &SourceFile) {
        self.line("SourceFile");
        self.nested(|d| {
            for doc in &file.docs {
                d.line(&format!("InnerDoc {doc:?}"));
            }
            for item in &file.items {
                d.item(item);
            }
        });
    }

    #[allow(clippy::too_many_lines)]
    fn item(&mut self, item: &Item) {
        let vis = if item.vis == Visibility::Public {
            "pub "
        } else {
            ""
        };
        match &item.kind {
            ItemKind::Fn(f) => {
                self.line(&format!("{vis}Fn {}", f.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    if let Some(sp) = &f.self_param {
                        d.line(&format!("SelfParam {}", sp.kind.as_str()));
                    }
                    for p in &f.params {
                        d.line("Param");
                        d.nested(|d| {
                            d.pat(&p.pat);
                            d.ty(&p.ty);
                        });
                    }
                    if let Some(ret) = &f.ret {
                        d.line("Ret");
                        d.nested(|d| d.ty(ret));
                    }
                    d.block(&f.body);
                });
            }
            ItemKind::Struct(s) => {
                self.line(&format!("{vis}Struct {}", s.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    match &s.kind {
                        StructKind::Unit => d.line("Unit"),
                        StructKind::Named(fields) => {
                            for f in fields {
                                let fv = if f.vis == Visibility::Public {
                                    "pub "
                                } else {
                                    ""
                                };
                                d.line(&format!("{fv}Field {}", f.name.name));
                                d.nested(|d| {
                                    d.meta(&[], &f.docs);
                                    d.ty(&f.ty);
                                });
                            }
                        }
                    }
                });
            }
            ItemKind::Const(c) => {
                self.line(&format!("{vis}Const {}", c.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    d.ty(&c.ty);
                    d.expr(&c.value);
                });
            }
            ItemKind::Enum(e) => {
                self.line(&format!("{vis}Enum {}", e.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    for v in &e.variants {
                        d.line(&format!("Variant {}", v.name.name));
                        d.nested(|d| {
                            d.meta(&v.attrs, &v.docs);
                            match &v.data {
                                VariantData::Unit => {}
                                VariantData::Tuple(tys) => {
                                    for t in tys {
                                        d.ty(t);
                                    }
                                }
                                VariantData::Named(fields) => {
                                    for f in fields {
                                        d.line(&format!("Field {}", f.name.name));
                                        d.nested(|d| {
                                            d.meta(&[], &f.docs);
                                            d.ty(&f.ty);
                                        });
                                    }
                                }
                            }
                            if let Some(e) = &v.discriminant {
                                d.line("Discriminant");
                                d.nested(|d| d.expr(e));
                            }
                        });
                    }
                });
            }
            ItemKind::Impl(imp) => {
                match &imp.trait_ref {
                    Some(t) => self.line(&format!("{vis}Impl {} for", t.name)),
                    None => self.line(&format!("{vis}Impl")),
                }
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    d.ty(&imp.self_ty);
                    for member in &imp.items {
                        d.item(member);
                    }
                });
            }
        }
    }

    fn meta(&mut self, attrs: &[Attribute], docs: &[String]) {
        for a in attrs {
            self.line(&format!("Attr {}{}", path_str(&a.path), a.args));
        }
        for doc in docs {
            self.line(&format!("Doc {doc:?}"));
        }
    }

    fn ty(&mut self, ty: &Type) {
        match &ty.kind {
            TypeKind::Path(p) => self.line(&format!("Type {}", path_str(p))),
            TypeKind::Ref { mutable, inner } => {
                self.line(if *mutable { "RefMut" } else { "Ref" });
                self.nested(|d| d.ty(inner));
            }
            TypeKind::Unit => self.line("Type ()"),
            TypeKind::Tuple(elems) => {
                self.line("TupleType");
                self.nested(|d| {
                    for t in elems {
                        d.ty(t);
                    }
                });
            }
            TypeKind::Array(elem, len) => {
                self.line("ArrayType");
                self.nested(|d| {
                    d.ty(elem);
                    d.expr(len);
                });
            }
            TypeKind::Slice(elem) => {
                self.line("SliceType");
                self.nested(|d| d.ty(elem));
            }
            TypeKind::Infer => self.line("Type _"),
        }
    }

    fn pat(&mut self, pat: &Pat) {
        match &pat.kind {
            PatKind::Ident {
                by_ref,
                mutable,
                name,
                sub,
            } => {
                self.line(&format!(
                    "Pat {}{}{}",
                    if *by_ref { "ref " } else { "" },
                    if *mutable { "mut " } else { "" },
                    name.name
                ));
                if let Some(sub) = sub {
                    self.nested(|d| {
                        d.line("At");
                        d.nested(|d| d.pat(sub));
                    });
                }
            }
            PatKind::Wild => self.line("Pat _"),
            PatKind::Rest => self.line("Pat .."),
            PatKind::Tuple(elems) => {
                self.line("TuplePat");
                self.nested(|d| {
                    for p in elems {
                        d.pat(p);
                    }
                });
            }
            PatKind::Path(path) => self.line(&format!("PathPat {}", path_str(path))),
            PatKind::TupleStruct(path, elems) => {
                self.line(&format!("TupleStructPat {}", path_str(path)));
                self.nested(|d| {
                    for p in elems {
                        d.pat(p);
                    }
                });
            }
            PatKind::Struct { path, fields, rest } => {
                self.line(&format!("StructPat {}", path_str(path)));
                self.nested(|d| {
                    for f in fields {
                        d.line(&format!(
                            "FieldPat {}{}",
                            f.name.name,
                            if f.shorthand { " (shorthand)" } else { "" }
                        ));
                        d.nested(|d| d.pat(&f.pat));
                    }
                    if rest.is_some() {
                        d.line("Rest");
                    }
                });
            }
            PatKind::Lit(e) => {
                self.line("LitPat");
                self.nested(|d| d.expr(e));
            }
            PatKind::Range(lo, hi, end) => {
                self.line(&format!("RangePat {}", end.as_str()));
                self.nested(|d| {
                    d.expr(lo);
                    d.expr(hi);
                });
            }
            PatKind::Or(alts) => {
                self.line("OrPat");
                self.nested(|d| {
                    for p in alts {
                        d.pat(p);
                    }
                });
            }
        }
    }

    fn block(&mut self, block: &Block) {
        self.line("Block");
        self.nested(|d| {
            for s in &block.stmts {
                d.stmt(s);
            }
            if let Some(tail) = &block.tail {
                d.line("Tail");
                d.nested(|d| d.expr(tail));
            }
        });
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let(l) => {
                self.line("Let");
                self.nested(|d| {
                    d.pat(&l.pat);
                    if let Some(ty) = &l.ty {
                        d.ty(ty);
                    }
                    if let Some(init) = &l.init {
                        d.line("Init");
                        d.nested(|d| d.expr(init));
                    }
                    if let Some(els) = &l.els {
                        d.line("Else");
                        d.nested(|d| d.block(els));
                    }
                });
            }
            StmtKind::Expr { expr, semi } => {
                self.line(if *semi { "Semi" } else { "Expr" });
                self.nested(|d| d.expr(expr));
            }
            StmtKind::Empty => self.line("Empty"),
        }
    }

    fn exprs(&mut self, label: &str, exprs: &[Expr]) {
        if exprs.is_empty() {
            return;
        }
        self.line(label);
        self.nested(|d| {
            for e in exprs {
                d.expr(e);
            }
        });
    }

    #[allow(clippy::too_many_lines)]
    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Lit(l) => self.line(&format!("Lit {}", lit_str(l))),
            ExprKind::Path(p) => self.line(&format!("Path {}", path_str(p))),
            ExprKind::Unit => self.line("Unit"),
            ExprKind::Tuple(elems) => {
                self.line("Tuple");
                self.nested(|d| {
                    for e in elems {
                        d.expr(e);
                    }
                });
            }
            ExprKind::Array(elems) => {
                self.line("Array");
                self.nested(|d| {
                    for e in elems {
                        d.expr(e);
                    }
                });
            }
            ExprKind::Repeat(value, count) => {
                self.line("Repeat");
                self.nested(|d| {
                    d.expr(value);
                    d.expr(count);
                });
            }
            ExprKind::Paren(e) => {
                self.line("Paren");
                self.nested(|d| d.expr(e));
            }
            ExprKind::Unary(op, e) => {
                self.line(&format!("Unary {}", op.as_str()));
                self.nested(|d| d.expr(e));
            }
            ExprKind::Binary(op, l, r) => {
                self.line(&format!("Binary {}", op.as_str()));
                self.nested(|d| {
                    d.expr(l);
                    d.expr(r);
                });
            }
            ExprKind::Assign(l, r) => {
                self.line("Assign");
                self.nested(|d| {
                    d.expr(l);
                    d.expr(r);
                });
            }
            ExprKind::AssignOp(op, l, r) => {
                self.line(&format!("AssignOp {}=", op.as_str()));
                self.nested(|d| {
                    d.expr(l);
                    d.expr(r);
                });
            }
            ExprKind::Call { callee, args } => {
                self.line("Call");
                self.nested(|d| {
                    d.expr(callee);
                    d.exprs("Args", args);
                });
            }
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => {
                self.line(&format!("MethodCall {}", method.name));
                self.nested(|d| {
                    d.expr(receiver);
                    d.exprs("Args", args);
                });
            }
            ExprKind::Field(e, name) => {
                self.line(&format!("Field {}", name.name));
                self.nested(|d| d.expr(e));
            }
            ExprKind::Index(e, i) => {
                self.line("Index");
                self.nested(|d| {
                    d.expr(e);
                    d.expr(i);
                });
            }
            ExprKind::Range(lo, hi, end) => {
                self.line(&format!("Range {}", end.as_str()));
                self.nested(|d| {
                    for bound in [lo, hi].into_iter().flatten() {
                        d.expr(bound);
                    }
                });
            }
            ExprKind::Ref { mutable, expr } => {
                self.line(if *mutable { "RefMut" } else { "Ref" });
                self.nested(|d| d.expr(expr));
            }
            ExprKind::Deref(e) => {
                self.line("Deref");
                self.nested(|d| d.expr(e));
            }
            ExprKind::Cast(e, ty) => {
                self.line("Cast");
                self.nested(|d| {
                    d.expr(e);
                    d.ty(ty);
                });
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::If { cond, then, els } => {
                self.line("If");
                self.nested(|d| {
                    d.expr(cond);
                    d.block(then);
                    if let Some(e) = els {
                        d.line("Else");
                        d.nested(|d| d.expr(e));
                    }
                });
            }
            ExprKind::While { cond, body } => {
                self.line("While");
                self.nested(|d| {
                    d.expr(cond);
                    d.block(body);
                });
            }
            ExprKind::Loop { body } => {
                self.line("Loop");
                self.nested(|d| d.block(body));
            }
            ExprKind::Break(e) => {
                self.line("Break");
                if let Some(e) = e {
                    self.nested(|d| d.expr(e));
                }
            }
            ExprKind::Continue => self.line("Continue"),
            ExprKind::Return(e) => {
                self.line("Return");
                if let Some(e) = e {
                    self.nested(|d| d.expr(e));
                }
            }
            ExprKind::Struct { path, fields } => {
                self.line(&format!("Struct {}", path_str(path)));
                self.nested(|d| {
                    for f in fields {
                        d.line(&format!("FieldInit {}", f.name.name));
                        d.nested(|d| d.expr(&f.value));
                    }
                });
            }
            ExprKind::Match { scrutinee, arms } => {
                self.line("Match");
                self.nested(|d| {
                    d.expr(scrutinee);
                    for arm in arms {
                        d.line("Arm");
                        d.nested(|d| {
                            d.pat(&arm.pat);
                            if let Some(g) = &arm.guard {
                                d.line("Guard");
                                d.nested(|d| d.expr(g));
                            }
                            d.expr(&arm.body);
                        });
                    }
                });
            }
            ExprKind::Let(pat, e) => {
                self.line("Let");
                self.nested(|d| {
                    d.pat(pat);
                    d.expr(e);
                });
            }
        }
    }
}

pub(crate) fn path_str(p: &Path) -> String {
    let mut s = String::new();
    for (i, seg) in p.segments.iter().enumerate() {
        if i > 0 {
            s.push_str("::");
        }
        s.push_str(&seg.name);
    }
    s
}

pub(crate) fn lit_str(l: &Lit) -> String {
    let mut s = String::new();
    match l {
        Lit::Int { value, suffix } => {
            let _ = write!(s, "{value}");
            if let Some(sfx) = suffix {
                s.push_str(sfx.as_str());
            }
        }
        Lit::Float { text, suffix } => {
            s.push_str(text);
            if let Some(sfx) = suffix {
                s.push_str(sfx.as_str());
            }
        }
        Lit::Bool(b) => {
            let _ = write!(s, "{b}");
        }
        Lit::Char(c) => {
            let _ = write!(s, "'{}'", c.escape_default());
        }
        Lit::Str(st) => {
            let _ = write!(s, "\"{}\"", st.escape_default());
        }
    }
    s
}
