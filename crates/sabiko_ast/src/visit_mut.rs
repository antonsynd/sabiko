//! A mutable walk over every pattern of a file, for the rewrites name resolution makes
//! (an identifier pattern that names a constant, a unit struct or a variant becomes a
//! path pattern, as rustc's resolver decides it).

use crate::{
    Block, Expr, ExprKind, FnDecl, Item, ItemKind, Pat, PatKind, Path, SourceFile, StmtKind,
    StructKind, Type, TypeKind, VariantData,
};

impl SourceFile {
    /// Calls `f` on every pattern of the file, outer patterns before the patterns inside
    /// them (after `f` returns, the walk descends into what the pattern then holds): in
    /// parameters, `let`s, `match` arms, `let` expressions and destructuring assignments,
    /// wherever an expression may stand (bodies, constants, discriminants, array lengths).
    pub fn for_each_pat_mut(&mut self, f: &mut impl FnMut(&mut Pat)) {
        for item in &mut self.items {
            item_pats(item, f);
        }
    }
}

fn item_pats(item: &mut Item, f: &mut impl FnMut(&mut Pat)) {
    match &mut item.kind {
        ItemKind::Fn(func) => fn_pats(func, f),
        ItemKind::Const(c) => {
            ty_pats(&mut c.ty, f);
            if let Some(v) = &mut c.value {
                expr_pats(v, f);
            }
        }
        ItemKind::Struct(s) => {
            if let StructKind::Named(fields) = &mut s.kind {
                for field in fields {
                    ty_pats(&mut field.ty, f);
                }
            }
        }
        ItemKind::Enum(e) => {
            for v in &mut e.variants {
                match &mut v.data {
                    VariantData::Unit => {}
                    VariantData::Tuple(tys) => {
                        for t in tys {
                            ty_pats(t, f);
                        }
                    }
                    VariantData::Named(fields) => {
                        for field in fields {
                            ty_pats(&mut field.ty, f);
                        }
                    }
                }
                if let Some(d) = &mut v.discriminant {
                    expr_pats(d, f);
                }
            }
        }
        ItemKind::Impl(imp) => {
            ty_pats(&mut imp.self_ty, f);
            for member in &mut imp.items {
                item_pats(member, f);
            }
        }
        ItemKind::Trait(t) => {
            for member in &mut t.items {
                item_pats(member, f);
            }
        }
        ItemKind::Use(_) => {}
    }
}

fn fn_pats(func: &mut FnDecl, f: &mut impl FnMut(&mut Pat)) {
    for p in &mut func.params {
        ty_pats(&mut p.ty, f);
        pat_pats(&mut p.pat, f);
    }
    if let Some(ret) = &mut func.ret {
        ty_pats(ret, f);
    }
    if let Some(body) = &mut func.body {
        block_pats(body, f);
    }
}

fn pat_pats(pat: &mut Pat, f: &mut impl FnMut(&mut Pat)) {
    f(pat);
    match &mut pat.kind {
        PatKind::Ident { sub, .. } => {
            if let Some(sub) = sub {
                pat_pats(sub, f);
            }
        }
        PatKind::Tuple(elems) | PatKind::TupleStruct(_, elems) | PatKind::Or(elems) => {
            for p in elems {
                pat_pats(p, f);
            }
        }
        PatKind::Struct { fields, .. } => {
            for fp in fields {
                pat_pats(&mut fp.pat, f);
            }
        }
        PatKind::Ref { pat, .. } => pat_pats(pat, f),
        PatKind::Lit(e) | PatKind::Assignee(e) => expr_pats(e, f),
        PatKind::Range(lo, hi, _) => {
            for e in [lo, hi].into_iter().flatten() {
                expr_pats(e, f);
            }
        }
        PatKind::Wild | PatKind::Rest | PatKind::Path(_) => {}
    }
}

fn block_pats(block: &mut Block, f: &mut impl FnMut(&mut Pat)) {
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            StmtKind::Let(l) => {
                pat_pats(&mut l.pat, f);
                if let Some(t) = &mut l.ty {
                    ty_pats(t, f);
                }
                if let Some(init) = &mut l.init {
                    expr_pats(init, f);
                }
                if let Some(els) = &mut l.els {
                    block_pats(els, f);
                }
            }
            StmtKind::Expr { expr, .. } => expr_pats(expr, f),
            StmtKind::Empty => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        expr_pats(tail, f);
    }
}

fn path_pats(path: &mut Path, f: &mut impl FnMut(&mut Pat)) {
    for args in &mut path.generic_args {
        for t in &mut args.args {
            ty_pats(t, f);
        }
    }
}

fn ty_pats(ty: &mut Type, f: &mut impl FnMut(&mut Pat)) {
    match &mut ty.kind {
        TypeKind::Path(p) => path_pats(p, f),
        TypeKind::Ref { inner, .. } | TypeKind::Slice(inner) => ty_pats(inner, f),
        TypeKind::Tuple(elems) => {
            for t in elems {
                ty_pats(t, f);
            }
        }
        TypeKind::Array(elem, len) => {
            ty_pats(elem, f);
            expr_pats(len, f);
        }
        TypeKind::Unit | TypeKind::Infer | TypeKind::Never => {}
    }
}

fn exprs_pats(exprs: &mut [Expr], f: &mut impl FnMut(&mut Pat)) {
    for e in exprs {
        expr_pats(e, f);
    }
}

fn expr_pats(expr: &mut Expr, f: &mut impl FnMut(&mut Pat)) {
    match &mut expr.kind {
        ExprKind::Lit(_) | ExprKind::Unit | ExprKind::Underscore | ExprKind::Continue => {}
        ExprKind::Path(p) => path_pats(p, f),
        ExprKind::Tuple(es) | ExprKind::Array(es) => exprs_pats(es, f),
        ExprKind::Repeat(a, b)
        | ExprKind::Binary(_, a, b)
        | ExprKind::Assign(a, b)
        | ExprKind::AssignOp(_, a, b)
        | ExprKind::Index(a, b) => {
            expr_pats(a, f);
            expr_pats(b, f);
        }
        ExprKind::Paren(e)
        | ExprKind::Unary(_, e)
        | ExprKind::Field(e, _)
        | ExprKind::Try(e)
        | ExprKind::Ref { expr: e, .. }
        | ExprKind::Deref(e) => expr_pats(e, f),
        ExprKind::DestructuringAssign(p, e) | ExprKind::Let(p, e) => {
            pat_pats(p, f);
            expr_pats(e, f);
        }
        ExprKind::Call { callee, args } => {
            expr_pats(callee, f);
            exprs_pats(args, f);
        }
        ExprKind::MethodCall {
            receiver,
            generic_args,
            args,
            ..
        } => {
            expr_pats(receiver, f);
            if let Some(g) = generic_args {
                for t in &mut g.args {
                    ty_pats(t, f);
                }
            }
            exprs_pats(args, f);
        }
        ExprKind::Range(lo, hi, _) => {
            for e in [lo, hi].into_iter().flatten() {
                expr_pats(e, f);
            }
        }
        ExprKind::Break(e) | ExprKind::Return(e) => {
            if let Some(e) = e {
                expr_pats(e, f);
            }
        }
        ExprKind::Cast(e, t) => {
            expr_pats(e, f);
            ty_pats(t, f);
        }
        ExprKind::Block(b) | ExprKind::Loop { body: b } => block_pats(b, f),
        ExprKind::If { cond, then, els } => {
            expr_pats(cond, f);
            block_pats(then, f);
            if let Some(e) = els {
                expr_pats(e, f);
            }
        }
        ExprKind::While { cond, body } => {
            expr_pats(cond, f);
            block_pats(body, f);
        }
        ExprKind::Struct { path, fields, .. } => {
            path_pats(path, f);
            for field in fields {
                expr_pats(&mut field.value, f);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_pats(scrutinee, f);
            for arm in arms {
                pat_pats(&mut arm.pat, f);
                if let Some(g) = &mut arm.guard {
                    expr_pats(g, f);
                }
                expr_pats(&mut arm.body, f);
            }
        }
    }
}
