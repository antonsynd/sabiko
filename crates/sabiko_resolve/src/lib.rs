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
//!
//! A few functions are built into the compiler ([`Builtin`]): `Print`, `PrintLine` and
//! `Panic`, which code generation lowers to calls into the runtime library. Each gets a
//! [`DefInfo`] with `builtin: Some(_)` and no AST item, appended after every user item so
//! user [`DefId`]s are unaffected. A user item of the same name shadows the builtin
//! silently.

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

/// A function built into the compiler. Code generation lowers calls to these into the
/// runtime library; earlier phases treat them as ordinary function definitions whose
/// [`DefInfo::builtin`] is `Some`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// `Print(x)`: prints a printable value without a trailing newline; returns `()`.
    Print,
    /// `PrintLine(x)`: prints a printable value followed by a newline; returns `()`.
    PrintLine,
    /// `Panic(msg: &str) -> !`: aborts with the message.
    Panic,
}

impl Builtin {
    pub const ALL: [Builtin; 3] = [Self::Print, Self::PrintLine, Self::Panic];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Print => "Print",
            Self::PrintLine => "PrintLine",
            Self::Panic => "Panic",
        }
    }
}

/// An item definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefInfo {
    pub kind: DefKind,
    pub name: String,
    /// Span of the item's name. [`Span::dummy`] for a builtin, which has no source text;
    /// never put it in a diagnostic label.
    pub span: Span,
    /// The `Item` node. [`NodeId::DUMMY`] for a builtin.
    pub node: NodeId,
    /// Index into `SourceFile::items` (of the `impl` block, for an associated function).
    /// `usize::MAX` for a builtin.
    pub item_index: usize,
    /// For an associated function, its index among the members of its `impl` block.
    pub member_index: Option<usize>,
    /// For an associated function, the struct its `impl` block is for; `None` for other
    /// items, and for the members of an `impl` whose type did not resolve.
    pub parent: Option<DefId>,
    /// `Some` for a compiler builtin, `None` for every user item.
    pub builtin: Option<Builtin>,
}

impl DefInfo {
    /// Whether this is a function of an `impl` block (a method or an associated function).
    #[must_use]
    pub fn is_assoc(&self) -> bool {
        self.member_index.is_some()
    }
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
    /// The associated functions of each struct, by name.
    assoc_fns: HashMap<(DefId, String), DefId>,
    /// The `self` binding of each method, keyed by its function's definition.
    self_locals: HashMap<DefId, LocalId>,
}

impl ResolveResults {
    #[must_use]
    pub fn def(&self, id: DefId) -> &DefInfo {
        &self.defs[id.0 as usize]
    }

    /// The definition an item node introduced. Not defined for an `impl` block, whose
    /// members are the definitions.
    #[must_use]
    pub fn def_of_item(&self, item: NodeId) -> DefId {
        self.item_defs[&item]
    }

    /// The AST item of a user definition (a member of an `impl` block for an associated
    /// function). Must not be called for a builtin (see [`ResolveResults::builtin`]):
    /// builtins have no item.
    #[must_use]
    pub fn item<'a>(&self, file: &'a SourceFile, id: DefId) -> &'a Item {
        let info = self.def(id);
        assert!(
            info.builtin.is_none(),
            "builtin `{}` has no AST item",
            info.name
        );
        let item = &file.items[info.item_index];
        match (&item.kind, info.member_index) {
            (ItemKind::Impl(imp), Some(member)) => &imp.items[member],
            _ => item,
        }
    }

    /// The name of a definition as a path: `Point::New` for an associated function,
    /// the plain name otherwise.
    #[must_use]
    pub fn qualified_name(&self, def: DefId) -> String {
        let info = self.def(def);
        match info.parent {
            Some(parent) => format!("{}::{}", self.def(parent).name, info.name),
            None => info.name.clone(),
        }
    }

    /// The associated function `name` of the struct `adt`, if it has one.
    #[must_use]
    pub fn assoc_fn(&self, adt: DefId, name: &str) -> Option<DefId> {
        self.assoc_fns.get(&(adt, name.to_owned())).copied()
    }

    /// The `self` binding of a method; `None` for a function without a `self` parameter.
    #[must_use]
    pub fn self_local(&self, def: DefId) -> Option<LocalId> {
        self.self_locals.get(&def).copied()
    }

    /// `Some` when `def` is a compiler builtin rather than a user item.
    #[must_use]
    pub fn builtin(&self, def: DefId) -> Option<Builtin> {
        self.def(def).builtin
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

    /// All definitions of a kind, in source order, with the builtins (if `kind` is
    /// `DefKind::Fn`) after every user item.
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
        impl_scope: ImplScope::Outside,
    };
    r.collect_items(file);
    r.collect_impls(file);
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
    /// Whether the resolver is inside an `impl` block, and for which struct: what `Self`
    /// names.
    impl_scope: ImplScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImplScope {
    Outside,
    /// An `impl` block whose type did not resolve (already reported).
    Unresolved,
    Struct(DefId),
}

impl Resolver<'_> {
    fn new_def(&mut self, info: DefInfo) -> DefId {
        let id = DefId(u32::try_from(self.results.defs.len()).expect("too many items"));
        self.results.defs.push(info);
        id
    }

    fn collect_items(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            let kind = match &item.kind {
                ItemKind::Fn(_) => DefKind::Fn,
                ItemKind::Struct(_) => DefKind::Struct,
                ItemKind::Const(_) => DefKind::Const,
                ItemKind::Impl(imp) => {
                    // The members are definitions of their own, in source order, but not in
                    // the item namespace: they are reached as `Type::Name` or as methods.
                    for (member_index, member) in imp.items.iter().enumerate() {
                        let ItemKind::Fn(f) = &member.kind else {
                            continue;
                        };
                        let id = self.new_def(DefInfo {
                            kind: DefKind::Fn,
                            name: f.name.name.clone(),
                            span: f.name.span,
                            node: member.id,
                            item_index: index,
                            member_index: Some(member_index),
                            parent: None,
                            builtin: None,
                        });
                        self.results.item_defs.insert(member.id, id);
                    }
                    continue;
                }
            };
            let name = item.name().expect("only `impl` blocks have no name");
            let id = self.new_def(DefInfo {
                kind,
                name: name.name.clone(),
                span: name.span,
                node: item.id,
                item_index: index,
                member_index: None,
                parent: None,
                builtin: None,
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
        // Builtins come last so user items keep the `DefId`s they would have without
        // them. A user item of the same name shadows the builtin without a diagnostic.
        for builtin in Builtin::ALL {
            if self.items.contains_key(builtin.name()) {
                continue;
            }
            let id = self.new_def(DefInfo {
                kind: DefKind::Fn,
                name: builtin.name().to_owned(),
                span: Span::dummy(),
                node: NodeId::DUMMY,
                item_index: usize::MAX,
                member_index: None,
                parent: None,
                builtin: Some(builtin),
            });
            self.items.insert(builtin.name().to_owned(), id);
        }
    }

    /// Resolves the type of every `impl` block, records each member's parent struct and
    /// registers it under `(struct, name)`. Two members of one name for the same struct are
    /// reported as rustc does: at the later one when both are in one block, at the earlier
    /// one when they are in different blocks.
    fn collect_impls(&mut self, file: &SourceFile) {
        // The first definition of each `(struct, name)`, with the index of its block.
        let mut first: HashMap<(DefId, String), (DefId, usize)> = HashMap::new();
        for (index, item) in file.items.iter().enumerate() {
            let ItemKind::Impl(imp) = &item.kind else {
                continue;
            };
            let Some(adt) = self.resolve_impl_target(&imp.self_ty) else {
                continue;
            };
            for member in &imp.items {
                let ItemKind::Fn(f) = &member.kind else {
                    continue;
                };
                let def = self.results.def_of_item(member.id);
                self.results.defs[def.0 as usize].parent = Some(adt);
                let key = (adt, f.name.name.clone());
                let Some(&(prev, prev_block)) = first.get(&key) else {
                    first.insert(key.clone(), (def, index));
                    self.results.assoc_fns.insert(key, def);
                    continue;
                };
                let prev_span = self.results.def(prev).span;
                let name = &f.name.name;
                let (primary, other) = if prev_block == index {
                    (f.name.span, prev_span)
                } else {
                    (prev_span, f.name.span)
                };
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_ASSOC_ITEM,
                        format!("duplicate definitions with name `{name}`"),
                    )
                    .with_label(primary, format!("duplicate definitions for `{name}`"))
                    .with_label(other, format!("other definition for `{name}`")),
                );
            }
        }
    }

    /// The struct an `impl` block is for, reporting anything else.
    fn resolve_impl_target(&mut self, ty: &Type) -> Option<DefId> {
        let TypeKind::Path(path) = &ty.kind else {
            self.resolve_type(ty, true);
            self.invalid_impl_target(ty.span, "this type");
            return None;
        };
        let res = self.resolve_type_path(path);
        self.results.res.insert(ty.id, res);
        match res {
            Res::Def(def) => Some(def),
            Res::Prim(_) => {
                self.invalid_impl_target(ty.span, "primitive types");
                None
            }
            Res::Local(_) | Res::Err => None,
        }
    }

    fn invalid_impl_target(&mut self, span: Span, what: &str) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::INVALID_IMPL_TARGET,
                format!("cannot define inherent `impl` for {what}"),
            )
            .with_label(span, "not a struct of this file")
            .with_note(
                "an inherent `impl` block is only allowed for a struct defined in the same file",
            ),
        );
    }

    fn resolve_item(&mut self, item: &Item, index: usize) {
        if let ItemKind::Impl(imp) = &item.kind {
            let target = match imp.self_ty.kind {
                TypeKind::Path(_) => match self.results.res(imp.self_ty.id) {
                    Res::Def(def) => Some(def),
                    _ => None,
                },
                _ => None,
            };
            self.impl_scope = target.map_or(ImplScope::Unresolved, ImplScope::Struct);
            for member in &imp.items {
                if let ItemKind::Fn(f) = &member.kind {
                    let def = self.results.def_of_item(member.id);
                    self.resolve_fn(def, f);
                }
            }
            self.impl_scope = ImplScope::Outside;
            return;
        }
        let def = self.results.def_of_item(item.id);
        debug_assert_eq!(self.results.def(def).item_index, index);
        match &item.kind {
            ItemKind::Impl(_) => unreachable!("handled above"),
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
        if let Some(sp) = &f.self_param {
            // `self` cannot be a `let` binding (it is a keyword), so a lookup of it finds
            // this binding and nothing else.
            let id = LocalId(u32::try_from(self.results.locals.len()).expect("too many locals"));
            self.results.locals.push(LocalInfo {
                name: "self".to_owned(),
                mutable: matches!(sp.kind, sabiko_ast::SelfKind::Value { mutable: true }),
                span: sp.span,
                pat: sp.id,
                owner: def,
                is_param: true,
            });
            self.results.pat_locals.insert(sp.id, id);
            self.results.self_locals.insert(def, id);
            self.scopes
                .last_mut()
                .expect("a scope is open")
                .push(("self".to_owned(), id));
        }
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for p in &f.params {
            self.resolve_type(&p.ty, true);
            let mut names = Vec::new();
            pat_bindings(&p.pat, &mut names);
            for name in names {
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
            PatKind::Tuple(elems) => {
                for p in elems {
                    self.bind_pat(p, is_param);
                }
            }
        }
    }

    /// Binds a `let` pattern. A name bound twice in one pattern (`let (x, x) = t;`) is an
    /// error; both bindings are still made, the second shadowing the first.
    fn bind_let_pat(&mut self, pat: &Pat) {
        let mut names = Vec::new();
        pat_bindings(pat, &mut names);
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for name in names {
            if let Some(&prev) = seen.get(name.name.as_str()) {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_BINDING,
                        format!(
                            "identifier `{}` is bound more than once in the same pattern",
                            name.name
                        ),
                    )
                    .with_label(name.span, "used in a pattern more than once")
                    .with_label(prev, "first use of the name"),
                );
            } else {
                seen.insert(&name.name, name.span);
            }
        }
        self.bind_pat(pat, false);
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
            TypeKind::Tuple(elems) => {
                for t in elems {
                    self.resolve_type(t, in_signature);
                }
            }
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

    /// Adds a "`name` defined here" label unless the definition is a builtin, which has
    /// no source to point at.
    fn with_definition_label(d: Diagnostic, info: &DefInfo, name: &str) -> Diagnostic {
        if info.span.is_dummy() {
            d.with_note(format!("`{name}` is a built-in function"))
        } else {
            d.with_label(info.span, format!("`{name}` defined here"))
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

    /// `Self` inside an `impl` block: its struct, or `Res::Err` (silently) when the block's
    /// type did not resolve. Outside one it is an error, reported here.
    fn resolve_self_type(&mut self, span: Span) -> Res {
        match self.impl_scope {
            ImplScope::Struct(def) => Res::Def(def),
            ImplScope::Unresolved => Res::Err,
            ImplScope::Outside => {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNRESOLVED_TYPE,
                        "cannot find type `Self` in this scope",
                    )
                    .with_label(span, "`Self` is only available in impls"),
                );
                Res::Err
            }
        }
    }

    fn resolve_type_path(&mut self, path: &Path) -> Res {
        if !path.is_single() {
            return self.unsupported_path(path);
        }
        let name = &path.segments[0];
        if name.name == "Self" {
            return self.resolve_self_type(name.span);
        }
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                DefKind::Struct => Res::Def(def),
                kind => {
                    let d = Diagnostic::error(
                        ErrorCode::WRONG_NAMESPACE,
                        format!("expected type, found {} `{}`", kind.describe(), name.name),
                    )
                    .with_label(name.span, "not a type");
                    self.sink
                        .emit(Self::with_definition_label(d, info, &name.name));
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
        match path.segments.len() {
            1 => {}
            2 => return self.resolve_assoc_path(path),
            _ => return self.unsupported_path(path),
        }
        let name = &path.segments[0];
        if let Some(local) = self.lookup_local(&name.name) {
            return Res::Local(local);
        }
        if name.name == "self" {
            let in_assoc_fn = self.impl_scope != ImplScope::Outside;
            let mut d = Diagnostic::error(
                ErrorCode::SELF_OUTSIDE_METHOD,
                "cannot find value `self` in this scope",
            )
            .with_label(
                name.span,
                "`self` value is a keyword only available in methods with a `self` parameter",
            );
            if in_assoc_fn {
                d = d.with_note("add a `self` receiver parameter to make this a method: `&self`, `&mut self` or `self`");
            }
            self.sink.emit(d);
            return Res::Err;
        }
        if name.name == "Self" {
            // A unit struct as a value; the type checker rejects any other struct.
            return self.resolve_self_type(name.span);
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
        let owners: Vec<String> = self
            .results
            .assoc_fns
            .keys()
            .filter(|(_, n)| *n == name.name)
            .map(|(adt, _)| self.results.def(*adt).name.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Some(owner) = owners.first() {
            d = d.with_note(format!(
                "`{}` is an associated function of `{owner}`: call it as `{owner}::{}(..)`",
                name.name, name.name
            ));
        }
        self.sink.emit(d);
        Res::Err
    }

    /// `Type::Name`: an associated function of a struct (`Self` inside an `impl` block).
    /// The first segment is looked up as a type: a local, a function or a constant of
    /// that name does not count, as in rustc.
    fn resolve_assoc_path(&mut self, path: &Path) -> Res {
        let (ty, item) = (&path.segments[0], &path.segments[1]);
        if matches!(ty.name.as_str(), "self" | "crate" | "super") {
            return self.unsupported_path(path);
        }
        let known = ty.name == "Self"
            || self.items.contains_key(&ty.name)
            || PrimTy::from_name(&ty.name).is_some();
        if !known {
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::UNRESOLVED_TYPE,
                    format!("cannot find type `{}` in this scope", ty.name),
                )
                .with_label(ty.span, "not found in this scope")
                .with_note("`A::B` names the associated function `B` of the struct `A`; module paths are planned for Phase 9"),
            );
            return Res::Err;
        }
        let adt = match self.resolve_type_path(&Path {
            segments: vec![ty.clone()],
            span: ty.span,
        }) {
            Res::Def(def) => def,
            Res::Prim(prim) => {
                self.unknown_assoc_item(item, &format!("type `{}`", prim.name()));
                return Res::Err;
            }
            Res::Local(_) | Res::Err => return Res::Err,
        };
        if let Some(def) = self.results.assoc_fn(adt, &item.name) {
            return Res::Def(def);
        }
        let what = format!("struct `{}`", self.results.def(adt).name);
        self.unknown_assoc_item(item, &what);
        Res::Err
    }

    fn unknown_assoc_item(&mut self, item: &sabiko_ast::Ident, owner: &str) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNKNOWN_ASSOC_ITEM,
                format!(
                    "no associated function named `{}` found for {owner}",
                    item.name
                ),
            )
            .with_label(
                item.span,
                format!("associated function not found in {owner}"),
            ),
        );
    }

    fn resolve_struct_path(&mut self, path: &Path) -> Res {
        if !path.is_single() {
            return self.unsupported_path(path);
        }
        let name = &path.segments[0];
        if name.name == "Self" {
            return self.resolve_self_type(name.span);
        }
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                DefKind::Struct => Res::Def(def),
                kind => {
                    let d = Diagnostic::error(
                        ErrorCode::WRONG_NAMESPACE,
                        format!("expected struct, found {} `{}`", kind.describe(), name.name),
                    )
                    .with_label(name.span, "not a struct");
                    self.sink
                        .emit(Self::with_definition_label(d, info, &name.name));
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
                self.bind_let_pat(&l.pat);
            }
            StmtKind::Expr { expr, .. } => self.resolve_expr(expr),
            StmtKind::Empty => {}
        }
    }

    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Lit(_) | ExprKind::Unit | ExprKind::Continue => {}
            ExprKind::Tuple(elems) => {
                for e in elems {
                    self.resolve_expr(e);
                }
            }
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

/// The identifiers a pattern binds, left to right.
fn pat_bindings<'p>(pat: &'p Pat, out: &mut Vec<&'p sabiko_ast::Ident>) {
    match &pat.kind {
        PatKind::Ident { name, .. } => out.push(name),
        PatKind::Wild => {}
        PatKind::Tuple(elems) => {
            for p in elems {
                pat_bindings(p, out);
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
        let item = ast
            .items
            .iter()
            .find(|i| i.name().is_some_and(|n| n.name == name))
            .unwrap();
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
    fn builtins_resolve_and_are_shadowed_by_user_items() {
        let (ast, r, msgs) = run(
            "fn PrintLine(x: i32) {}\nfn F() { PrintLine(1); Print(2); Panic(\"x\"); }\nfn G() -> Print {}",
        );
        assert_eq!(msgs, ["E0203 expected type, found function `Print`"]);
        // User items first, then the builtins the user did not define.
        let names: Vec<&str> = r.defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["PrintLine", "F", "G", "Print", "Panic"]);
        assert_eq!(r.builtin(DefId(0)), None);
        assert_eq!(r.builtin(DefId(3)), Some(Builtin::Print));
        assert_eq!(r.builtin(DefId(4)), Some(Builtin::Panic));
        assert!(r.def(DefId(3)).span.is_dummy());
        let paths = fn_paths(&ast, "F");
        let res: Vec<Res> = paths.iter().map(|(_, id)| r.res(*id)).collect();
        assert_eq!(
            res,
            [Res::Def(DefId(0)), Res::Def(DefId(3)), Res::Def(DefId(4))]
        );
        assert_eq!(r.defs_of_kind(DefKind::Fn).count(), 5);
    }

    #[test]
    fn user_struct_shadows_primitive_and_wrong_namespace() {
        let (_, r, msgs) = run("struct i32;\nfn F(x: i32) {}\nconst K: i32 = 1;\nfn G() -> F {}");
        assert_eq!(msgs, ["E0203 expected type, found function `F`"]);
        assert_eq!(r.defs_of_kind(DefKind::Struct).count(), 1);
    }
}
