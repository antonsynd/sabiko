//! Renders an AST back to Sabiko source in a canonical layout.
//!
//! Round-trip property: for any tree `t`, `parse(pretty(t))` is structurally equal to
//! `t` (ignoring spans and ids), and `pretty` is a fixpoint of `parse`. Parentheses are
//! inserted wherever precedence requires them, so the printer never depends on the
//! `Paren` nodes being present.

use std::fmt::Write as _;

use crate::dump::{lit_str, path_str};
#[allow(clippy::wildcard_imports)]
use crate::*;

#[must_use]
pub fn pretty(file: &SourceFile) -> String {
    let mut p = Printer {
        out: String::new(),
        indent: 0,
    };
    for doc in &file.docs {
        let _ = writeln!(p.out, "//!{doc}");
    }
    for (i, item) in file.items.iter().enumerate() {
        if i > 0 || !file.docs.is_empty() {
            p.out.push('\n');
        }
        p.item(item);
    }
    p.out
}

#[must_use]
pub fn pretty_expr(expr: &Expr) -> String {
    let mut p = Printer {
        out: String::new(),
        indent: 0,
    };
    p.expr(expr, 0);
    p.out
}

struct Printer {
    out: String,
    indent: usize,
}

impl Printer {
    fn newline(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn meta(&mut self, attrs: &[Attribute], docs: &[String]) {
        for d in docs {
            let _ = write!(self.out, "///{d}");
            self.newline();
        }
        for a in attrs {
            // `#[derive(Copy)]` but `#[doc = "x"]`: a space keeps `=` args off the path.
            let sep = if a.args.starts_with(['(', '[', '{']) || a.args.is_empty() {
                ""
            } else {
                " "
            };
            let _ = write!(self.out, "#[{}{sep}{}]", path_str(&a.path), a.args);
            self.newline();
        }
    }

    fn vis(&mut self, vis: Visibility) {
        if vis == Visibility::Public {
            self.out.push_str("pub ");
        }
    }

    fn item(&mut self, item: &Item) {
        self.meta(&item.attrs, &item.docs);
        self.vis(item.vis);
        match &item.kind {
            ItemKind::Fn(f) => {
                let _ = write!(self.out, "fn {}(", f.name.name);
                if let Some(sp) = &f.self_param {
                    self.out.push_str(sp.kind.as_str());
                }
                for (i, p) in f.params.iter().enumerate() {
                    if i > 0 || f.self_param.is_some() {
                        self.out.push_str(", ");
                    }
                    self.pat(&p.pat);
                    self.out.push_str(": ");
                    self.ty(&p.ty);
                }
                self.out.push(')');
                if let Some(ret) = &f.ret {
                    self.out.push_str(" -> ");
                    self.ty(ret);
                }
                self.out.push(' ');
                self.block(&f.body);
                self.out.push('\n');
            }
            ItemKind::Struct(s) => {
                let _ = write!(self.out, "struct {}", s.name.name);
                match &s.kind {
                    StructKind::Unit => self.out.push_str(";\n"),
                    StructKind::Named(fields) => {
                        self.out.push_str(" {");
                        self.indent += 1;
                        for f in fields {
                            self.newline();
                            self.meta(&[], &f.docs);
                            self.vis(f.vis);
                            let _ = write!(self.out, "{}: ", f.name.name);
                            self.ty(&f.ty);
                            self.out.push(',');
                        }
                        self.indent -= 1;
                        self.newline();
                        self.out.push_str("}\n");
                    }
                }
            }
            ItemKind::Const(c) => {
                let _ = write!(self.out, "const {}: ", c.name.name);
                self.ty(&c.ty);
                self.out.push_str(" = ");
                self.expr(&c.value, 0);
                self.out.push_str(";\n");
            }
            ItemKind::Impl(imp) => {
                self.out.push_str("impl ");
                self.ty(&imp.self_ty);
                self.out.push_str(" {");
                self.indent += 1;
                for member in &imp.items {
                    self.newline();
                    self.item(member);
                    // A member ends with a newline at column 0; the next line is indented.
                    self.out.pop();
                }
                self.indent -= 1;
                self.newline();
                self.out.push_str("}\n");
            }
        }
    }

    fn ty(&mut self, ty: &Type) {
        match &ty.kind {
            TypeKind::Path(p) => self.out.push_str(&path_str(p)),
            TypeKind::Ref { mutable, inner } => {
                self.out.push_str(if *mutable { "&mut " } else { "&" });
                self.ty(inner);
            }
            TypeKind::Unit => self.out.push_str("()"),
            TypeKind::Tuple(elems) => {
                self.out.push('(');
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.ty(t);
                }
                if elems.len() == 1 {
                    self.out.push(',');
                }
                self.out.push(')');
            }
            TypeKind::Infer => self.out.push('_'),
        }
    }

    fn pat(&mut self, pat: &Pat) {
        match &pat.kind {
            PatKind::Ident { mutable, name } => {
                if *mutable {
                    self.out.push_str("mut ");
                }
                self.out.push_str(&name.name);
            }
            PatKind::Wild => self.out.push('_'),
            PatKind::Tuple(elems) => {
                self.out.push('(');
                for (i, p) in elems.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.pat(p);
                }
                if elems.len() == 1 {
                    self.out.push(',');
                }
                self.out.push(')');
            }
        }
    }

    fn block(&mut self, block: &Block) {
        self.out.push('{');
        if block.stmts.is_empty() && block.tail.is_none() {
            self.out.push('}');
            return;
        }
        self.indent += 1;
        for s in &block.stmts {
            self.newline();
            self.stmt(s);
        }
        if let Some(tail) = &block.tail {
            self.newline();
            self.stmt_expr(tail);
        }
        self.indent -= 1;
        self.newline();
        self.out.push('}');
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let(l) => {
                self.out.push_str("let ");
                self.pat(&l.pat);
                if let Some(ty) = &l.ty {
                    self.out.push_str(": ");
                    self.ty(ty);
                }
                if let Some(init) = &l.init {
                    self.out.push_str(" = ");
                    self.expr(init, 0);
                }
                self.out.push(';');
            }
            StmtKind::Expr { expr, semi } => {
                self.stmt_expr(expr);
                if *semi {
                    self.out.push(';');
                }
            }
            StmtKind::Empty => self.out.push(';'),
        }
    }

    /// An expression at statement start: if it begins with a block-like expression but is
    /// not one itself (`{ a } + 1`), the parser would end the statement after the block, so
    /// wrap it in parentheses.
    fn stmt_expr(&mut self, expr: &Expr) {
        if !expr.is_block_like() && leftmost_is_block_like(expr) {
            self.out.push('(');
            self.expr(expr, 0);
            self.out.push(')');
        } else {
            self.expr(expr, 0);
        }
    }

    fn args(&mut self, args: &[Expr]) {
        self.out.push('(');
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.expr(a, 0);
        }
        self.out.push(')');
    }

    /// Prints `expr`, parenthesising it if its own precedence is below `min_prec`.
    #[allow(clippy::too_many_lines)]
    fn expr(&mut self, expr: &Expr, min_prec: u8) {
        let prec = precedence(expr);
        let paren = prec < min_prec;
        if paren {
            self.out.push('(');
        }
        match &expr.kind {
            ExprKind::Lit(l) => self.out.push_str(&lit_str(l)),
            ExprKind::Path(p) => self.out.push_str(&path_str(p)),
            ExprKind::Unit => self.out.push_str("()"),
            ExprKind::Tuple(elems) => {
                self.out.push('(');
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(e, 0);
                }
                if elems.len() == 1 {
                    self.out.push(',');
                }
                self.out.push(')');
            }
            ExprKind::Paren(e) => {
                self.out.push('(');
                self.expr(e, 0);
                self.out.push(')');
            }
            ExprKind::Unary(op, e) => {
                self.out.push_str(op.as_str());
                self.expr(e, UNARY_PRECEDENCE);
            }
            ExprKind::Binary(op, l, r) => {
                let p = op.precedence();
                // Comparisons are non-associative: parenthesise nested comparisons on both
                // sides. Other operators are left-associative.
                let (lp, rp) = if op.is_comparison() {
                    (p + 1, p + 1)
                } else {
                    (p, p + 1)
                };
                // `x as i32 < y` is rejected by the parser (like rustc): the `<` after a
                // type path is taken as the start of generic arguments. If the left
                // operand would end in a bare `as <type>`, parenthesise it.
                let lt_like = matches!(op, BinOp::Lt | BinOp::Le | BinOp::Shl);
                if lt_like && rightmost_is_cast_at(l, lp) {
                    self.out.push('(');
                    self.expr(l, 0);
                    self.out.push(')');
                } else {
                    self.expr(l, lp);
                }
                let _ = write!(self.out, " {} ", op.as_str());
                self.expr(r, rp);
            }
            ExprKind::Assign(l, r) => {
                self.expr(l, 1);
                self.out.push_str(" = ");
                self.expr(r, 0);
            }
            ExprKind::AssignOp(op, l, r) => {
                self.expr(l, 1);
                let _ = write!(self.out, " {}= ", op.as_str());
                self.expr(r, 0);
            }
            ExprKind::Call { callee, args } => {
                // `(x.f)()` calls the field; `x.f()` would be a method call.
                let field_callee = matches!(callee.kind, ExprKind::Field(..));
                if field_callee {
                    self.out.push('(');
                }
                self.expr(callee, POSTFIX_PRECEDENCE);
                if field_callee {
                    self.out.push(')');
                }
                self.args(args);
            }
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => {
                self.expr(receiver, POSTFIX_PRECEDENCE);
                let _ = write!(self.out, ".{}", method.name);
                self.args(args);
            }
            ExprKind::Field(e, name) => {
                self.expr(e, POSTFIX_PRECEDENCE);
                // `1 .0`: without the space, `1.0` would lex as a float literal.
                if matches!(e.kind, ExprKind::Lit(Lit::Int { .. } | Lit::Float { .. }))
                    && name.name.starts_with(|c: char| c.is_ascii_digit())
                {
                    self.out.push(' ');
                }
                let _ = write!(self.out, ".{}", name.name);
            }
            ExprKind::Index(e, i) => {
                self.expr(e, POSTFIX_PRECEDENCE);
                self.out.push('[');
                self.expr(i, 0);
                self.out.push(']');
            }
            ExprKind::Ref { mutable, expr } => {
                self.out.push_str(if *mutable { "&mut " } else { "&" });
                self.expr(expr, UNARY_PRECEDENCE);
            }
            ExprKind::Deref(e) => {
                self.out.push('*');
                self.expr(e, UNARY_PRECEDENCE);
            }
            ExprKind::Cast(e, ty) => {
                self.expr(e, CAST_PRECEDENCE);
                self.out.push_str(" as ");
                self.ty(ty);
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::If { cond, then, els } => {
                self.out.push_str("if ");
                self.cond(cond);
                self.out.push(' ');
                self.block(then);
                if let Some(e) = els {
                    self.out.push_str(" else ");
                    self.expr(e, 0);
                }
            }
            ExprKind::While { cond, body } => {
                self.out.push_str("while ");
                self.cond(cond);
                self.out.push(' ');
                self.block(body);
            }
            ExprKind::Loop { body } => {
                self.out.push_str("loop ");
                self.block(body);
            }
            ExprKind::Break(e) => {
                self.out.push_str("break");
                if let Some(e) = e {
                    self.out.push(' ');
                    self.expr(e, 0);
                }
            }
            ExprKind::Continue => self.out.push_str("continue"),
            ExprKind::Return(e) => {
                self.out.push_str("return");
                if let Some(e) = e {
                    self.out.push(' ');
                    self.expr(e, 0);
                }
            }
            ExprKind::Struct { path, fields } => {
                self.out.push_str(&path_str(path));
                self.out.push_str(" {");
                for (i, f) in fields.iter().enumerate() {
                    self.out.push_str(if i > 0 { ", " } else { " " });
                    let _ = write!(self.out, "{}: ", f.name.name);
                    self.expr(&f.value, 0);
                }
                self.out
                    .push_str(if fields.is_empty() { "}" } else { " }" });
            }
        }
        if paren {
            self.out.push(')');
        }
    }

    /// Conditions of `if`/`while` may not contain a bare struct literal, and a `break`/
    /// `return` operand may not start with `{` there; wrap such conditions.
    fn cond(&mut self, cond: &Expr) {
        if needs_cond_parens(cond) {
            self.out.push('(');
            self.expr(cond, 0);
            self.out.push(')');
        } else {
            self.expr(cond, 0);
        }
    }
}

/// Whether printing `e` bare as an `if`/`while` condition would be mis-parsed: a struct
/// literal outside parentheses, blocks or argument lists, or a `break`/`return` whose
/// operand starts with `{`.
fn needs_cond_parens(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Struct { .. } => true,
        // The index operand of `Index`, call arguments and struct field values are
        // deliberately not inspected: inside `[...]`, `(...)` or `{...}` the parser
        // allows struct literals again, so `if a[Foo { x: 1 }] {}` prints bare.
        ExprKind::Unary(_, x)
        | ExprKind::Deref(x)
        | ExprKind::Ref { expr: x, .. }
        | ExprKind::Cast(x, _)
        | ExprKind::Field(x, _)
        | ExprKind::Index(x, _)
        | ExprKind::Call { callee: x, .. }
        | ExprKind::MethodCall { receiver: x, .. } => needs_cond_parens(x),
        ExprKind::Binary(_, l, r) | ExprKind::Assign(l, r) | ExprKind::AssignOp(_, l, r) => {
            needs_cond_parens(l) || needs_cond_parens(r)
        }
        ExprKind::Break(Some(x)) | ExprKind::Return(Some(x)) => {
            leftmost_is_block(x) || needs_cond_parens(x)
        }
        _ => false,
    }
}

/// Whether the last tokens printed for `e` (at top level, with no enclosing precedence
/// context) are a bare `as <type>`: `e` is a cast, or its right spine ends in one.
#[must_use]
pub fn rightmost_is_cast(e: &Expr) -> bool {
    rightmost_is_cast_at(e, 0)
}

/// [`rightmost_is_cast`] for `e` printed with `min_prec`: a node that precedence wraps
/// in parentheses ends in `)` and so never exposes a trailing cast.
fn rightmost_is_cast_at(e: &Expr, min_prec: u8) -> bool {
    if precedence(e) < min_prec {
        return false;
    }
    match &e.kind {
        ExprKind::Cast(..) => true,
        ExprKind::Binary(op, _, r) => rightmost_is_cast_at(r, op.precedence() + 1),
        ExprKind::Assign(_, r) | ExprKind::AssignOp(_, _, r) => rightmost_is_cast_at(r, 0),
        ExprKind::Unary(_, x) | ExprKind::Ref { expr: x, .. } | ExprKind::Deref(x) => {
            rightmost_is_cast_at(x, UNARY_PRECEDENCE)
        }
        ExprKind::Break(Some(x)) | ExprKind::Return(Some(x)) => rightmost_is_cast_at(x, 0),
        _ => false,
    }
}

/// Whether the first token printed for `e` is `{`.
fn leftmost_is_block(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Block(_) => true,
        ExprKind::Binary(_, l, _) | ExprKind::Assign(l, _) | ExprKind::AssignOp(_, l, _) => {
            leftmost_is_block(l)
        }
        ExprKind::Cast(x, _)
        | ExprKind::Field(x, _)
        | ExprKind::Index(x, _)
        | ExprKind::Call { callee: x, .. }
        | ExprKind::MethodCall { receiver: x, .. } => leftmost_is_block(x),
        _ => false,
    }
}

/// Whether the first token printed for `e` opens a block-like expression.
fn leftmost_is_block_like(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Block(_)
        | ExprKind::If { .. }
        | ExprKind::While { .. }
        | ExprKind::Loop { .. } => true,
        ExprKind::Binary(_, l, _) | ExprKind::Assign(l, _) | ExprKind::AssignOp(_, l, _) => {
            leftmost_is_block_like(l)
        }
        ExprKind::Cast(x, _)
        | ExprKind::Field(x, _)
        | ExprKind::Index(x, _)
        | ExprKind::Call { callee: x, .. }
        | ExprKind::MethodCall { receiver: x, .. } => leftmost_is_block_like(x),
        _ => false,
    }
}

const POSTFIX_PRECEDENCE: u8 = 12;
/// Atoms never need parentheses.
const ATOM_PRECEDENCE: u8 = 13;

fn precedence(e: &Expr) -> u8 {
    match &e.kind {
        ExprKind::Lit(_)
        | ExprKind::Path(_)
        | ExprKind::Unit
        | ExprKind::Tuple(_)
        | ExprKind::Paren(_)
        | ExprKind::Block(_)
        | ExprKind::If { .. }
        | ExprKind::While { .. }
        | ExprKind::Loop { .. }
        | ExprKind::Struct { .. } => ATOM_PRECEDENCE,
        ExprKind::Call { .. }
        | ExprKind::MethodCall { .. }
        | ExprKind::Field(..)
        | ExprKind::Index(..) => POSTFIX_PRECEDENCE,
        ExprKind::Unary(..) | ExprKind::Ref { .. } | ExprKind::Deref(_) => UNARY_PRECEDENCE,
        ExprKind::Cast(..) => CAST_PRECEDENCE,
        ExprKind::Binary(op, ..) => op.precedence(),
        // `break`/`return`/assignment bind loosest; they swallow everything to their right.
        ExprKind::Assign(..)
        | ExprKind::AssignOp(..)
        | ExprKind::Break(_)
        | ExprKind::Return(_)
        | ExprKind::Continue => 0,
    }
}

#[cfg(test)]
mod tests {
    use sabiko_span::{FileId, Span};

    use super::*;

    const SP: Span = Span {
        file: FileId(0),
        start: 0,
        end: 0,
    };

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

    fn mk(kind: ExprKind) -> Expr {
        Expr {
            id: NodeId::DUMMY,
            kind,
            span: SP,
        }
    }

    fn var(name: &str) -> Expr {
        mk(ExprKind::Path(path(name)))
    }

    fn cast(e: Expr, ty: &str) -> Expr {
        mk(ExprKind::Cast(
            Box::new(e),
            Type {
                id: NodeId::DUMMY,
                kind: TypeKind::Path(path(ty)),
                span: SP,
            },
        ))
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        mk(ExprKind::Binary(op, Box::new(l), Box::new(r)))
    }

    #[test]
    fn cast_before_lt_is_parenthesised() {
        let e = bin(BinOp::Lt, cast(var("x"), "i32"), var("y"));
        assert_eq!(pretty_expr(&e), "(x as i32) < y");
        let e = bin(BinOp::Le, cast(var("x"), "i32"), var("y"));
        assert_eq!(pretty_expr(&e), "(x as i32) <= y");
        let e = bin(BinOp::Shl, cast(var("x"), "u32"), var("y"));
        assert_eq!(pretty_expr(&e), "(x as u32) << y");
    }

    #[test]
    fn cast_on_right_spine_before_lt_is_parenthesised() {
        // `a * b as i32 < c`: the cast binds tighter than `*`, so it is the rightmost
        // token of the left operand without any parentheses of its own.
        let e = bin(
            BinOp::Lt,
            bin(BinOp::Mul, var("a"), cast(var("b"), "i32")),
            var("c"),
        );
        assert_eq!(pretty_expr(&e), "(a * b as i32) < c");
    }

    #[test]
    fn cast_not_exposed_needs_no_extra_parens() {
        // Other operators are unaffected.
        let e = bin(BinOp::Gt, cast(var("x"), "i32"), var("y"));
        assert_eq!(pretty_expr(&e), "x as i32 > y");
        // A cast on the right operand is harmless.
        let e = bin(BinOp::Lt, var("x"), cast(var("y"), "i32"));
        assert_eq!(pretty_expr(&e), "x < y as i32");
        // Precedence already wraps the cast under a unary operator, so it ends in `)`.
        let e = bin(
            BinOp::Lt,
            mk(ExprKind::Unary(UnOp::Neg, Box::new(cast(var("x"), "i32")))),
            var("y"),
        );
        assert_eq!(pretty_expr(&e), "-(x as i32) < y");
        // A nested comparison is parenthesised anyway.
        let e = bin(
            BinOp::Lt,
            bin(BinOp::Eq, var("a"), cast(var("b"), "i32")),
            var("c"),
        );
        assert_eq!(pretty_expr(&e), "(a == b as i32) < c");
    }

    #[test]
    fn rightmost_is_cast_walks_right_spine() {
        assert!(rightmost_is_cast(&cast(var("x"), "i32")));
        assert!(rightmost_is_cast(&bin(
            BinOp::Add,
            var("a"),
            cast(var("b"), "i32")
        )));
        assert!(rightmost_is_cast(&mk(ExprKind::Return(Some(Box::new(
            cast(var("x"), "i32")
        ))))));
        assert!(!rightmost_is_cast(&var("x")));
        assert!(!rightmost_is_cast(&bin(
            BinOp::Add,
            cast(var("a"), "i32"),
            var("b")
        )));
        assert!(!rightmost_is_cast(&mk(ExprKind::Paren(Box::new(cast(
            var("x"),
            "i32"
        ))))));
    }
}
