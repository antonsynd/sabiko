//! Name resolution for Sabiko.
//!
//! Walks a parsed [`SourceFile`] and produces [`ResolveResults`]: a table of item
//! definitions ([`DefId`]), a table of local bindings ([`LocalId`]), and a map from every
//! path-bearing AST node (types, path expressions, struct literals) to what it names
//! ([`Res`]). Later phases consume the AST plus these side tables; there is no separate
//! resolved tree.
//!
//! Rules implemented here (matching Rust where the subset overlaps):
//! - items are visible everywhere in the file, including before their definition;
//! - a `let` binding is visible from the statement after it until the end of its block,
//!   and shadows earlier bindings and items of the same name;
//! - a `let` initialiser is resolved before its own binding is introduced
//!   (`let x = x + 1;` refers to the outer `x`);
//! - parameters live in the function's outermost scope;
//! - primitive type names (`i32`, `bool`, ...) are resolved last, so a user struct may
//!   shadow them (as in Rust).
//!
//! Sabiko has a single namespace for items: a struct and a function may not share a
//! name. Rust separates type and value namespaces; the restriction is deliberate (one
//! name, one meaning) and recorded in `docs/DECISIONS.md`.

use std::collections::HashMap;

use sabiko_ast::{
    Block, Expr, ExprKind, FnDecl, Item, ItemKind, NodeId, Pat, PatKind, Path, SourceFile, Stmt,
    StmtKind, StructKind, Type, TypeKind,
};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_span::Span;

/// Identifies an item definition within one compilation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

/// Identifies a local binding (parameter or `let`) within one compilation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

/// The built-in primitive types. `str` is only usable behind a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PrimTy {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    Bool,
    Char,
    Str,
}

impl PrimTy {
    pub const ALL: [PrimTy; 17] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::I128,
        Self::Isize,
        Self::U8,
        Self::U16,
        Self::U32,
        Self::U64,
        Self::U128,
        Self::Usize,
        Self::F32,
        Self::F64,
        Self::Bool,
        Self::Char,
        Self::Str,
    ];

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.name() == name)
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::I128 => "i128",
            Self::Isize => "isize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::U128 => "u128",
            Self::Usize => "usize",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Char => "char",
            Self::Str => "str",
        }
    }

    #[must_use]
    pub fn is_signed_int(self) -> bool {
        matches!(
            self,
            Self::I8 | Self::I16 | Self::I32 | Self::I64 | Self::I128 | Self::Isize
        )
    }

    #[must_use]
    pub fn is_unsigned_int(self) -> bool {
        matches!(
            self,
            Self::U8 | Self::U16 | Self::U32 | Self::U64 | Self::U128 | Self::Usize
        )
    }

    #[must_use]
    pub fn is_integer(self) -> bool {
        self.is_signed_int() || self.is_unsigned_int()
    }

    #[must_use]
    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    #[must_use]
    pub fn is_numeric(self) -> bool {
        self.is_integer() || self.is_float()
    }

    /// Bit width of an integer type. `isize`/`usize` are 64 bits: Sabiko targets 64-bit
    /// .NET runtimes only (recorded in `docs/DECISIONS.md`).
    #[must_use]
    pub fn int_bits(self) -> Option<u32> {
        Some(match self {
            Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 => 32,
            Self::I64 | Self::U64 | Self::Isize | Self::Usize => 64,
            Self::I128 | Self::U128 => 128,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefKind {
    Fn,
    Struct,
    Const,
}

impl DefKind {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Fn => "function",
            Self::Struct => "struct",
            Self::Const => "constant",
        }
    }
}

/// An item definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefInfo {
    pub kind: DefKind,
    pub name: String,
    /// Span of the item's name.
    pub span: Span,
    /// The `Item` node.
    pub node: NodeId,
    /// Index into `SourceFile::items`.
    pub item_index: usize,
}

/// A local binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalInfo {
    pub name: String,
    pub mutable: bool,
    /// Span of the binding's name.
    pub span: Span,
    /// The `Pat` node that introduced it.
    pub pat: NodeId,
    /// The function the binding lives in.
    pub owner: DefId,
    /// Whether the binding is a parameter.
    pub is_param: bool,
}

/// What a path resolved to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Res {
    Local(LocalId),
    Def(DefId),
    Prim(PrimTy),
    /// Resolution failed; a diagnostic has been emitted. Later phases stay quiet.
    Err,
}

/// The output of name resolution.
#[derive(Clone, Debug, Default)]
pub struct ResolveResults {
    pub defs: Vec<DefInfo>,
    pub locals: Vec<LocalInfo>,
    item_defs: HashMap<NodeId, DefId>,
    /// Keyed by the `Type` node of a `TypeKind::Path`, the `Expr` node of an
    /// `ExprKind::Path` or `ExprKind::Struct`.
    res: HashMap<NodeId, Res>,
    pat_locals: HashMap<NodeId, LocalId>,
}

impl ResolveResults {
    #[must_use]
    pub fn def(&self, id: DefId) -> &DefInfo {
        &self.defs[id.0 as usize]
    }

    /// The definition an item node introduced.
    #[must_use]
    pub fn def_of_item(&self, item: NodeId) -> DefId {
        self.item_defs[&item]
    }

    #[must_use]
    pub fn item<'a>(&self, file: &'a SourceFile, id: DefId) -> &'a Item {
        &file.items[self.def(id).item_index]
    }

    /// The resolution recorded for a type, path expression or struct literal node.
    /// Panics if the node is not one of those: every such node gets an entry, `Res::Err`
    /// when resolution failed.
    #[must_use]
    pub fn res(&self, node: NodeId) -> Res {
        *self
            .res
            .get(&node)
            .unwrap_or_else(|| panic!("no resolution recorded for {node:?}"))
    }

    #[must_use]
    pub fn local(&self, id: LocalId) -> &LocalInfo {
        &self.locals[id.0 as usize]
    }

    /// The local introduced by an identifier pattern; `None` for `_`.
    #[must_use]
    pub fn local_of_pat(&self, pat: NodeId) -> Option<LocalId> {
        self.pat_locals.get(&pat).copied()
    }

    /// All definitions of a kind, in source order.
    pub fn defs_of_kind(&self, kind: DefKind) -> impl Iterator<Item = DefId> + '_ {
        self.defs
            .iter()
            .enumerate()
            .filter(move |(_, d)| d.kind == kind)
            .map(|(i, _)| DefId(u32::try_from(i).expect("fewer than 2^32 items")))
    }
}

/// Resolves every name in `file`. Errors go to `sink`; the results are complete even when
/// errors occurred (failed resolutions are `Res::Err`).
#[must_use]
pub fn resolve(file: &SourceFile, sink: &mut DiagnosticSink) -> ResolveResults {
    let mut r = Resolver {
        results: ResolveResults::default(),
        items: HashMap::new(),
        scopes: Vec::new(),
        sink,
        current_fn: None,
    };
    r.collect_items(file);
    for (index, item) in file.items.iter().enumerate() {
        r.resolve_item(item, index);
    }
    r.results
}

struct Resolver<'d> {
    results: ResolveResults,
    /// First definition of each item name.
    items: HashMap<String, DefId>,
    scopes: Vec<Vec<(String, LocalId)>>,
    sink: &'d mut DiagnosticSink,
    current_fn: Option<DefId>,
}

impl Resolver<'_> {
    fn collect_items(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            let kind = match &item.kind {
                ItemKind::Fn(_) => DefKind::Fn,
                ItemKind::Struct(_) => DefKind::Struct,
                ItemKind::Const(_) => DefKind::Const,
            };
            let name = item.name();
            let id = DefId(u32::try_from(self.results.defs.len()).expect("too many items"));
            self.results.defs.push(DefInfo {
                kind,
                name: name.name.clone(),
                span: name.span,
                node: item.id,
                item_index: index,
            });
            self.results.item_defs.insert(item.id, id);
            if let Some(&first) = self.items.get(&name.name) {
                let first_span = self.results.def(first).span;
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_DEFINITION,
                        format!("the name `{}` is defined multiple times", name.name),
                    )
                    .with_label(name.span, format!("`{}` redefined here", name.name))
                    .with_label(
                        first_span,
                        format!("previous definition of `{}` here", name.name),
                    )
                    .with_note("items share one namespace: a struct, function and constant may not have the same name"),
                );
            } else {
                self.items.insert(name.name.clone(), id);
            }
        }
    }

    fn resolve_item(&mut self, item: &Item, index: usize) {
        let def = self.results.def_of_item(item.id);
        debug_assert_eq!(self.results.def(def).item_index, index);
        match &item.kind {
            ItemKind::Fn(f) => self.resolve_fn(def, f),
            ItemKind::Struct(s) => {
                if let StructKind::Named(fields) = &s.kind {
                    let mut seen: HashMap<&str, Span> = HashMap::new();
                    for field in fields {
                        if let Some(&prev) = seen.get(field.name.name.as_str()) {
                            self.sink.emit(
                                Diagnostic::error(
                                    ErrorCode::DUPLICATE_FIELD_DEFINITION,
                                    format!("field `{}` is already declared", field.name.name),
                                )
                                .with_label(field.name.span, "field already declared")
                                .with_label(
                                    prev,
                                    format!("`{}` first declared here", field.name.name),
                                ),
                            );
                        } else {
                            seen.insert(&field.name.name, field.name.span);
                        }
                        self.resolve_type(&field.ty, true);
                    }
                }
            }
            ItemKind::Const(c) => {
                self.resolve_type(&c.ty, true);
                self.current_fn = Some(def);
                self.scopes.push(Vec::new());
                self.resolve_expr(&c.value);
                self.scopes.pop();
                self.current_fn = None;
            }
        }
    }

    fn resolve_fn(&mut self, def: DefId, f: &FnDecl) {
        self.current_fn = Some(def);
        self.scopes.push(Vec::new());
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for p in &f.params {
            self.resolve_type(&p.ty, true);
            if let PatKind::Ident { name, .. } = &p.pat.kind {
                if let Some(&prev) = seen.get(name.name.as_str()) {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::DUPLICATE_PARAMETER,
                            format!(
                                "identifier `{}` is bound more than once in this parameter list",
                                name.name
                            ),
                        )
                        .with_label(name.span, "used as parameter more than once")
                        .with_label(prev, "first use of the name"),
                    );
                } else {
                    seen.insert(&name.name, name.span);
                }
            }
            self.bind_pat(&p.pat, true);
        }
        if let Some(ret) = &f.ret {
            self.resolve_type(ret, true);
        }
        // The body block shares the parameter scope, as in Rust.
        self.resolve_block_in_current_scope(&f.body);
        self.scopes.pop();
        self.current_fn = None;
    }

    fn bind_pat(&mut self, pat: &Pat, is_param: bool) {
        match &pat.kind {
            PatKind::Ident { mutable, name } => {
                let id =
                    LocalId(u32::try_from(self.results.locals.len()).expect("too many locals"));
                self.results.locals.push(LocalInfo {
                    name: name.name.clone(),
                    mutable: *mutable,
                    span: name.span,
                    pat: pat.id,
                    owner: self.current_fn.expect("bindings only occur inside items"),
                    is_param,
                });
                self.results.pat_locals.insert(pat.id, id);
                self.scopes
                    .last_mut()
                    .expect("a scope is open")
                    .push((name.name.clone(), id));
            }
            PatKind::Wild => {}
        }
    }

    fn lookup_local(&self, name: &str) -> Option<LocalId> {
        self.scopes.iter().rev().find_map(|scope| {
            scope
                .iter()
                .rev()
                .find(|(n, _)| n == name)
                .map(|(_, id)| *id)
        })
    }

    /// `in_signature`: `_` is rejected (it is only meaningful inside bodies).
    fn resolve_type(&mut self, ty: &Type, in_signature: bool) {
        match &ty.kind {
            TypeKind::Path(path) => {
                let res = self.resolve_type_path(path);
                self.results.res.insert(ty.id, res);
            }
            TypeKind::Ref { inner, .. } => self.resolve_type(inner, in_signature),
            TypeKind::Unit => {}
            TypeKind::Infer => {
                if in_signature {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::PLACEHOLDER_IN_SIGNATURE,
                            "the placeholder `_` is not allowed within types on item signatures",
                        )
                        .with_label(ty.span, "not allowed in type signatures")
                        .with_note("inference only runs inside function bodies"),
                    );
                }
            }
        }
    }

    fn unsupported_path(&mut self, path: &Path) -> Res {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNSUPPORTED_SYNTAX,
                "not yet supported: paths with `::`",
            )
            .with_label(path.span, "module paths are not yet supported")
            .with_note("planned for Phase 9; see docs/PLAN.md"),
        );
        Res::Err
    }

    fn resolve_type_path(&mut self, path: &Path) -> Res {
        if !path.is_single() {
            return self.unsupported_path(path);
        }
        let name = &path.segments[0];
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                DefKind::Struct => Res::Def(def),
                kind => {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::WRONG_NAMESPACE,
                            format!("expected type, found {} `{}`", kind.describe(), name.name),
                        )
                        .with_label(name.span, "not a type")
                        .with_label(info.span, format!("`{}` defined here", name.name)),
                    );
                    Res::Err
                }
            };
        }
        if let Some(prim) = PrimTy::from_name(&name.name) {
            return Res::Prim(prim);
        }
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNRESOLVED_TYPE,
                format!("cannot find type `{}` in this scope", name.name),
            )
            .with_label(name.span, "not found in this scope"),
        );
        Res::Err
    }

    fn resolve_value_path(&mut self, path: &Path) -> Res {
        if !path.is_single() {
            return self.unsupported_path(path);
        }
        let name = &path.segments[0];
        if let Some(local) = self.lookup_local(&name.name) {
            return Res::Local(local);
        }
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                // Unit structs are values; the type checker rejects named structs used
                // as values, since it knows the struct's shape.
                DefKind::Fn | DefKind::Const | DefKind::Struct => Res::Def(def),
            };
        }
        let mut d = Diagnostic::error(
            ErrorCode::UNRESOLVED_VALUE,
            format!("cannot find value `{}` in this scope", name.name),
        )
        .with_label(name.span, "not found in this scope");
        if PrimTy::from_name(&name.name).is_some() {
            d = d.with_note(format!("`{}` is a type, not a value", name.name));
        }
        self.sink.emit(d);
        Res::Err
    }

    fn resolve_struct_path(&mut self, path: &Path) -> Res {
        if !path.is_single() {
            return self.unsupported_path(path);
        }
        let name = &path.segments[0];
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                DefKind::Struct => Res::Def(def),
                kind => {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::WRONG_NAMESPACE,
                            format!("expected struct, found {} `{}`", kind.describe(), name.name),
                        )
                        .with_label(name.span, "not a struct")
                        .with_label(info.span, format!("`{}` defined here", name.name)),
                    );
                    Res::Err
                }
            };
        }
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNRESOLVED_TYPE,
                format!("cannot find struct `{}` in this scope", name.name),
            )
            .with_label(name.span, "not found in this scope"),
        );
        Res::Err
    }

    fn resolve_block(&mut self, block: &Block) {
        self.scopes.push(Vec::new());
        self.resolve_block_in_current_scope(block);
        self.scopes.pop();
    }

    fn resolve_block_in_current_scope(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.resolve_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.resolve_expr(tail);
        }
    }

    fn resolve_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let(l) => {
                if let Some(ty) = &l.ty {
                    self.resolve_type(ty, false);
                }
                if let Some(init) = &l.init {
                    self.resolve_expr(init);
                }
                self.bind_pat(&l.pat, false);
            }
            StmtKind::Expr { expr, .. } => self.resolve_expr(expr),
            StmtKind::Empty => {}
        }
    }

    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Lit(_) | ExprKind::Unit | ExprKind::Continue => {}
            ExprKind::Path(path) => {
                let res = self.resolve_value_path(path);
                self.results.res.insert(expr.id, res);
            }
            ExprKind::Paren(e)
            | ExprKind::Unary(_, e)
            | ExprKind::Deref(e)
            | ExprKind::Field(e, _)
            | ExprKind::Ref { expr: e, .. } => self.resolve_expr(e),
            ExprKind::Binary(_, a, b)
            | ExprKind::Assign(a, b)
            | ExprKind::AssignOp(_, a, b)
            | ExprKind::Index(a, b) => {
                self.resolve_expr(a);
                self.resolve_expr(b);
            }
            ExprKind::Call { callee, args } => {
                self.resolve_expr(callee);
                for a in args {
                    self.resolve_expr(a);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.resolve_expr(receiver);
                for a in args {
                    self.resolve_expr(a);
                }
            }
            ExprKind::Cast(e, ty) => {
                self.resolve_expr(e);
                self.resolve_type(ty, false);
            }
            ExprKind::Block(b) => self.resolve_block(b),
            ExprKind::If { cond, then, els } => {
                self.resolve_expr(cond);
                self.resolve_block(then);
                if let Some(e) = els {
                    self.resolve_expr(e);
                }
            }
            ExprKind::While { cond, body } => {
                self.resolve_expr(cond);
                self.resolve_block(body);
            }
            ExprKind::Loop { body } => self.resolve_block(body),
            ExprKind::Break(e) | ExprKind::Return(e) => {
                if let Some(e) = e {
                    self.resolve_expr(e);
                }
            }
            ExprKind::Struct { path, fields } => {
                let res = self.resolve_struct_path(path);
                self.results.res.insert(expr.id, res);
                for f in fields {
                    self.resolve_expr(&f.value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use sabiko_diagnostics::DiagnosticSink;
    use sabiko_span::FileId;

    use super::*;

    fn run(src: &str) -> (SourceFile, ResolveResults, Vec<String>) {
        let mut sink = DiagnosticSink::new();
        let ast = sabiko_parser::parse_file(FileId(0), src, &mut sink);
        assert!(!sink.has_errors(), "parse errors: {:?}", sink.diagnostics());
        let results = resolve(&ast, &mut sink);
        let msgs = sink
            .into_vec()
            .into_iter()
            .map(|d| format!("{} {}", d.code, d.message))
            .collect();
        (ast, results, msgs)
    }

    fn path_exprs(expr: &Expr, out: &mut Vec<(String, NodeId)>) {
        match &expr.kind {
            ExprKind::Path(p) => out.push((p.last().name.clone(), expr.id)),
            ExprKind::Paren(e) => path_exprs(e, out),
            ExprKind::Binary(_, a, b) | ExprKind::Assign(a, b) => {
                path_exprs(a, out);
                path_exprs(b, out);
            }
            ExprKind::Call { callee, args } => {
                path_exprs(callee, out);
                for a in args {
                    path_exprs(a, out);
                }
            }
            ExprKind::Block(b) => {
                for s in &b.stmts {
                    match &s.kind {
                        StmtKind::Let(l) => {
                            if let Some(i) = &l.init {
                                path_exprs(i, out);
                            }
                        }
                        StmtKind::Expr { expr, .. } => path_exprs(expr, out),
                        StmtKind::Empty => {}
                    }
                }
                if let Some(t) = &b.tail {
                    path_exprs(t, out);
                }
            }
            _ => {}
        }
    }

    fn fn_paths(ast: &SourceFile, name: &str) -> Vec<(String, NodeId)> {
        let item = ast.items.iter().find(|i| i.name().name == name).unwrap();
        let ItemKind::Fn(f) = &item.kind else {
            panic!()
        };
        let mut out = Vec::new();
        let body = Expr {
            id: NodeId::DUMMY,
            kind: ExprKind::Block(f.body.clone()),
            span: f.body.span,
        };
        path_exprs(&body, &mut out);
        out
    }

    #[test]
    fn forward_references_and_shadowing() {
        let (ast, r, msgs) = run(
            "fn A(x: i32) -> i32 { let y = B(x); let x = y + x; ({ let x = 1; x }) + x }\nfn B(v: i32) -> i32 { v }",
        );
        assert!(msgs.is_empty(), "{msgs:?}");
        let paths = fn_paths(&ast, "A");
        let names: Vec<&str> = paths.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["B", "x", "y", "x", "x", "x"]);
        let res: Vec<Res> = paths.iter().map(|(_, id)| r.res(*id)).collect();
        // B is the second item.
        assert_eq!(res[0], Res::Def(DefId(1)));
        // Parameter x, then the initialiser `y + x` still sees the parameter.
        assert_eq!(res[1], Res::Local(LocalId(0)));
        assert_eq!(res[2], Res::Local(LocalId(1)));
        assert_eq!(res[3], Res::Local(LocalId(0)));
        // Inner block binding shadows; after the block the second `x` is visible again.
        assert_eq!(res[4], Res::Local(LocalId(3)));
        assert_eq!(res[5], Res::Local(LocalId(2)));
        assert!(r.local(LocalId(0)).is_param);
        assert!(!r.local(LocalId(2)).is_param);
    }

    #[test]
    fn errors() {
        let (_, _, msgs) = run(
            "struct S { a: i32, a: bool }\nfn S() {}\nfn F(p: i32, p: i32, q: _) -> Nope { let v: i32 = zz; let w = S { a: 1 }; let t = F { }; i32 }\nconst C: Q = 1;",
        );
        assert_eq!(
            msgs,
            [
                "E0200 the name `S` is defined multiple times",
                "E0204 field `a` is already declared",
                "E0205 identifier `p` is bound more than once in this parameter list",
                "E0207 the placeholder `_` is not allowed within types on item signatures",
                "E0201 cannot find type `Nope` in this scope",
                "E0202 cannot find value `zz` in this scope",
                "E0203 expected struct, found function `F`",
                "E0202 cannot find value `i32` in this scope",
                "E0201 cannot find type `Q` in this scope",
            ]
        );
    }

    #[test]
    fn user_struct_shadows_primitive_and_wrong_namespace() {
        let (_, r, msgs) = run("struct i32;\nfn F(x: i32) {}\nconst K: i32 = 1;\nfn G() -> F {}");
        assert_eq!(msgs, ["E0203 expected type, found function `F`"]);
        assert_eq!(r.defs_of_kind(DefKind::Struct).count(), 1);
    }
}
