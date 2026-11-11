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
                    d.generic_params(&f.generics);
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
                    d.where_clause(&f.where_clause);
                    match &f.body {
                        Some(body) => d.block(body),
                        None => d.line("NoBody"),
                    }
                });
            }
            ItemKind::Struct(s) => {
                self.line(&format!("{vis}Struct {}", s.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    d.generic_params(&s.generics);
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
                    if let Some(value) = &c.value {
                        d.expr(value);
                    }
                });
            }
            ItemKind::Enum(e) => {
                self.line(&format!("{vis}Enum {}", e.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    d.generic_params(&e.generics);
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
                    Some(t) => self.line(&format!("{vis}Impl {} for", path_text(&t.path, ""))),
                    None => self.line(&format!("{vis}Impl")),
                }
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    d.generic_params(&imp.generics);
                    d.ty(&imp.self_ty);
                    d.where_clause(&imp.where_clause);
                    for member in &imp.items {
                        d.item(member);
                    }
                });
            }
            ItemKind::Trait(t) => {
                self.line(&format!("{vis}Trait {}", t.name.name));
                self.nested(|d| {
                    d.meta(&item.attrs, &item.docs);
                    d.bounds("Supertraits", &t.supertraits);
                    for member in &t.items {
                        d.item(member);
                    }
                });
            }
            ItemKind::Use(u) => {
                self.line(&format!("{vis}Use {}", use_text(u)));
                self.nested(|d| d.meta(&item.attrs, &item.docs));
            }
        }
    }

    fn generic_params(&mut self, params: &[GenericParam]) {
        for g in params {
            self.line(&format!("GenericParam {}", g.name.name));
            self.nested(|d| d.bounds("Bounds", &g.bounds));
        }
    }

    fn bounds(&mut self, what: &str, bounds: &[TraitRef]) {
        if bounds.is_empty() {
            return;
        }
        let paths: Vec<String> = bounds.iter().map(|b| path_text(&b.path, "")).collect();
        self.line(&format!("{what} {}", paths.join(" + ")));
    }

    fn where_clause(&mut self, preds: &[WherePredicate]) {
        for p in preds {
            self.line("Where");
            self.nested(|d| {
                d.ty(&p.ty);
                d.bounds("Bounds", &p.bounds);
            });
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
            TypeKind::Path(p) => self.line(&format!("Type {}", path_text(p, ""))),
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
            TypeKind::Never => self.line("Type !"),
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
                    if let Some(lo) = lo {
                        d.expr(lo);
                    }
                    if let Some(hi) = hi {
                        d.expr(hi);
                    }
                });
            }
            PatKind::Ref { pat, mutable } => {
                self.line(if *mutable { "RefPat mut" } else { "RefPat" });
                self.nested(|d| d.pat(pat));
            }
            PatKind::Or(alts) => {
                self.line("OrPat");
                self.nested(|d| {
                    for p in alts {
                        d.pat(p);
                    }
                });
            }
            PatKind::Assignee(e) => {
                self.line("AssigneePat");
                self.nested(|d| d.expr(e));
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
            ExprKind::DestructuringAssign(p, r) => {
                self.line("DestructuringAssign");
                self.nested(|d| {
                    d.pat(p);
                    d.expr(r);
                });
            }
            ExprKind::Underscore => self.line("Underscore"),
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
                generic_args,
                args,
            } => {
                let name = Path {
                    segments: vec![method.clone()],
                    generic_args: generic_args.iter().map(|a| (**a).clone()).collect(),
                    span: method.span,
                };
                self.line(&format!("MethodCall {}", path_str(&name)));
                self.nested(|d| {
                    d.expr(receiver);
                    d.exprs("Args", args);
                });
            }
            ExprKind::Field(e, name) => {
                self.line(&format!("Field {}", name.name));
                self.nested(|d| d.expr(e));
            }
            ExprKind::Try(e) => {
                self.line("Try");
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
            ExprKind::Struct { path, fields, rest } => {
                self.line(&format!("Struct {}", path_str(path)));
                self.nested(|d| {
                    for f in fields {
                        d.line(&format!("FieldInit {}", f.name.name));
                        d.nested(|d| d.expr(&f.value));
                    }
                    if rest.is_some() {
                        d.line("Rest");
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

/// The path as written in an expression or pattern: generic arguments after `::`.
pub(crate) fn path_str(p: &Path) -> String {
    path_text(p, "::")
}

/// The path with `sep` before each generic argument list: `"::"` in expressions and
/// patterns (`Id::<i32>`), `""` in types (`Pair<i32, bool>`).
pub fn path_text(p: &Path, sep: &str) -> String {
    let mut s = String::new();
    for (i, seg) in p.segments.iter().enumerate() {
        if i > 0 {
            s.push_str("::");
        }
        s.push_str(&seg.name);
        if let Some(args) = p.args_of(i) {
            s.push_str(sep);
            s.push('<');
            for (j, a) in args.args.iter().enumerate() {
                if j > 0 {
                    s.push_str(", ");
                }
                s.push_str(&crate::pretty::type_str(a));
            }
            s.push('>');
        }
    }
    s
}

/// A `use` declaration's tree as written: `Option::Some` or `Option::{None, Some}`.
#[must_use]
pub fn use_text(u: &UseDecl) -> String {
    let names: Vec<&str> = u.names.iter().map(|n| n.name.as_str()).collect();
    let prefix = path_text(&u.prefix, "");
    if u.braced {
        format!("{prefix}::{{{}}}", names.join(", "))
    } else {
        format!("{prefix}::{}", names.join(", "))
    }
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
