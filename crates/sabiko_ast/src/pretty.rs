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
            let _ = write!(self.out, "#[{}{}]", path_str(&a.path), a.args);
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
                for (i, p) in f.params.iter().enumerate() {
                    if i > 0 {
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
                self.expr(l, lp);
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
