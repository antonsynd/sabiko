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
    Block, Expr, ExprKind, FieldDef, FnDecl, ImplDecl, Item, ItemKind, NodeId, Pat, PatKind, Path,
    SourceFile, Stmt, StmtKind, StructKind, Type, TypeKind, VariantData,
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
    Enum,
    /// A variant of an enum; its `parent` is the enum. Not in the item namespace: it is
    /// named `E::V` (or `Self::V`).
    Variant,
    /// A generic type parameter; its `parent` is the function that declares it. Only in
    /// scope in that function's signature and body. A trait's `Self` is one too, with the
    /// trait as parent.
    TyParam,
    /// `trait Name { .. }`, in the item namespace. Its members have it as parent.
    Trait,
    /// A trait `impl` block (`impl Show for i32`), not in any namespace: its members have
    /// it as parent, and its type parameters are its own (`impl<T> Show for Wrap<T>`).
    Impl,
}

impl DefKind {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Fn => "function",
            Self::Struct => "struct",
            Self::Const => "constant",
            Self::Enum => "enum",
            Self::Variant => "variant",
            Self::TyParam => "type parameter",
            Self::Trait => "trait",
            Self::Impl => "implementation",
        }
    }

    /// Whether the definition is a type: a struct or an enum.
    #[must_use]
    pub fn is_type(self) -> bool {
        matches!(self, Self::Struct | Self::Enum)
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

/// A trait the compiler knows (#116, decision 86): `Clone`, `Copy` and `Drop`, defined by
/// the compiler as ordinary traits of the file with no source item, after the builtins.
/// A user item of the same name shadows one, as it shadows a builtin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LangItem {
    /// `trait Clone { fn clone(&self) -> Self; }`
    Clone,
    /// `trait Copy: Clone {}`
    Copy,
    /// `trait Drop { fn drop(&mut self); }`
    Drop,
}

impl LangItem {
    pub const ALL: [LangItem; 3] = [Self::Clone, Self::Copy, Self::Drop];

    /// The trait's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Clone => "Clone",
            Self::Copy => "Copy",
            Self::Drop => "Drop",
        }
    }

    /// The trait's one method, which keeps Rust's lowercase name, and whether its
    /// receiver is `&mut self` (else `&self`).
    #[must_use]
    pub fn method(self) -> Option<(&'static str, bool)> {
        match self {
            Self::Clone => Some(("clone", false)),
            Self::Copy => None,
            Self::Drop => Some(("drop", true)),
        }
    }
}

/// A function of the `core` prelude that the compiler implements (decision 90): declared
/// in Sabiko with `#[intrinsic]` and a placeholder body, so resolution, type checking and
/// borrow checking treat it as an ordinary generic function; monomorphisation makes no
/// instance of it and code generation lowers each call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    /// `fn PanicDebug<T: Debug>(msg: &str, value: &T) -> !`: panics with `"{msg}:
    /// {value:?}"`, as `Result::unwrap` does (rustc's `unwrap_failed`).
    PanicDebug,
}

impl Intrinsic {
    pub const ALL: [Intrinsic; 1] = [Self::PanicDebug];

    /// The function's name in the prelude.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PanicDebug => "PanicDebug",
        }
    }
}

/// An associated constant of a primitive type (`i32::MAX`, `f64::EPSILON`, #48). Each one
/// used gets a [`DefKind::Const`] definition of its own, with no source item: the type
/// checker supplies its type and value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PrimConst {
    pub ty: PrimTy,
    pub name: &'static str,
}

impl PrimConst {
    const INT: &'static [&'static str] = &["MIN", "MAX", "BITS"];
    const FLOAT: &'static [&'static str] = &[
        "MIN",
        "MAX",
        "MIN_POSITIVE",
        "EPSILON",
        "INFINITY",
        "NEG_INFINITY",
        "NAN",
        "RADIX",
        "MANTISSA_DIGITS",
        "DIGITS",
        "MIN_EXP",
        "MAX_EXP",
        "MIN_10_EXP",
        "MAX_10_EXP",
    ];
    /// `char::MAX` is `'\u{10FFFF}'`, which a Sabiko `char` cannot hold (decision 31): it
    /// is known so that it can be rejected as such.
    const CHAR: &'static [&'static str] = &["MAX", "REPLACEMENT_CHARACTER"];

    /// The associated constant `name` of `ty`, as in Rust's standard library.
    #[must_use]
    pub fn lookup(ty: PrimTy, name: &str) -> Option<Self> {
        let names = if ty.int_bits().is_some() {
            Self::INT
        } else if ty.is_float() {
            Self::FLOAT
        } else if ty == PrimTy::Char {
            Self::CHAR
        } else {
            &[]
        };
        names
            .iter()
            .find(|n| **n == name)
            .map(|&name| PrimConst { ty, name })
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
    /// Index into `SourceFile::items` (of the `impl` block, for an associated item).
    /// `usize::MAX` for a builtin.
    pub item_index: usize,
    /// For an associated item (function or constant), its index among the members of its
    /// `impl` block.
    pub member_index: Option<usize>,
    /// For an associated item, the struct (or enum) its `impl` block is for; for a
    /// variant, its enum; `None` for other items, and for the members of an `impl` whose
    /// type did not resolve.
    pub parent: Option<DefId>,
    /// `Some` for a compiler builtin, `None` for every user item.
    pub builtin: Option<Builtin>,
}

impl DefInfo {
    /// Whether this is a member of an `impl` block (a method, an associated function or an
    /// associated constant).
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
    /// The `Pat` node that introduced it (the first alternative's, in an or-pattern, whose
    /// other alternatives' identifier patterns bind the same local).
    pub pat: NodeId,
    /// The function the binding lives in.
    pub owner: DefId,
    /// Whether the binding is a parameter.
    pub is_param: bool,
}

/// A bound on a type parameter, `param: trait_`: written after the parameter (`T: Show`),
/// in a `where` clause, or implied (a trait's `Self` implements the trait).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Predicate {
    pub param: DefId,
    pub trait_: DefId,
    /// The trait's path in the bound.
    pub span: Span,
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
    /// The type parameters of each generic function, struct and enum, in order. For a
    /// function of a generic `impl` block, its type's parameters come first (the block's
    /// own are aliases of them, decision 83), then the function's.
    generics: HashMap<DefId, Vec<DefId>>,
    /// For a function of a generic `impl` block: how many of its `generics` are its type's.
    parent_generics: HashMap<DefId, usize>,
    /// The associated items (functions and constants) of each struct or enum, by name.
    assoc_items: HashMap<(DefId, String), DefId>,
    /// The definitions made for the primitive associated constants the file uses.
    prim_consts: HashMap<DefId, PrimConst>,
    /// The `self` binding of each method, keyed by its function's definition.
    self_locals: HashMap<DefId, LocalId>,
    /// The `drop` method of the `impl Drop` block of each struct or enum (decision 86).
    drop_impls: HashMap<DefId, DefId>,
    /// The language traits (decision 86).
    lang_items: HashMap<LangItem, DefId>,
    /// The variants of each enum, in declaration order.
    variants: HashMap<DefId, Vec<DefId>>,
    /// The members of each trait, in declaration order.
    trait_members: HashMap<DefId, Vec<DefId>>,
    /// The required methods of the traits (declared without a body).
    required: std::collections::HashSet<DefId>,
    /// The supertraits of each trait, as written.
    supertraits: HashMap<DefId, Vec<DefId>>,
    /// The trait each trait `impl` block implements (by the block's definition); absent
    /// when it did not resolve.
    impl_traits: HashMap<DefId, DefId>,
    /// The members of each trait `impl` block, by name.
    impl_items: HashMap<(DefId, String), DefId>,
    /// The bounds of each function (with those of its inherent `impl` block), trait
    /// `impl` block and trait method, as written.
    predicates: HashMap<DefId, Vec<Predicate>>,
    /// For a path expression `A::Name` that names a trait's member: what `A` names in a
    /// type (a struct, a primitive, a type parameter, a trait `impl` block for `Self`), or
    /// the trait itself for `Trait::Name`. The type checker takes `Self` from it.
    assoc_self: HashMap<NodeId, Res>,
    /// The index of the first item of the `core` prelude in the file's items (decision
    /// 90); the number of items when there is no prelude.
    prelude_start: usize,
    /// The prelude's types the user's file has an `impl` block for that the orphan rule
    /// rejects (E0211, E0214). The block is not used, so a call of its method or a bound
    /// it would have met fails; the type checker does not report those again.
    orphan_targets: std::collections::HashSet<DefId>,
    /// The functions marked `#[track_caller]`: their panics report their caller's
    /// location, which code generation passes them as a hidden last argument.
    track_caller: std::collections::HashSet<DefId>,
    /// The prelude's intrinsics (`#[intrinsic]`), whose calls code generation lowers
    /// itself; their bodies are never used.
    #[allow(clippy::zero_sized_map_values)] // One intrinsic so far; more will come.
    intrinsics: HashMap<DefId, Intrinsic>,
    /// Identifier patterns that name a constant or a unit struct instead of binding
    /// (`LIMIT => ..`), and the path of every path, tuple-struct and struct pattern, keyed
    /// by the `Pat` node.
    pat_res: HashMap<NodeId, Res>,
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

    /// The definition an item node introduced, if any: `None` for an inherent `impl`
    /// block or `impl Drop` (a trait `impl` block has one).
    #[must_use]
    pub fn item_def(&self, item: NodeId) -> Option<DefId> {
        self.item_defs.get(&item).copied()
    }

    /// The AST item of a user definition (a member of an `impl` block for an associated
    /// item). Must not be called for a builtin (see [`ResolveResults::builtin`]):
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

    /// The name of a definition as a path: `Point::New` for an associated item, the plain
    /// name otherwise.
    #[must_use]
    pub fn generics(&self, def: DefId) -> &[DefId] {
        self.generics.get(&def).map_or(&[], Vec::as_slice)
    }

    /// How many of a function's `generics` are its `impl` block's type's (decision 83);
    /// the rest are its own, the ones a turbofish on its name gives.
    #[must_use]
    pub fn parent_generic_count(&self, def: DefId) -> usize {
        self.parent_generics.get(&def).copied().unwrap_or(0)
    }

    /// The type parameters a function declares itself (not its `impl` block's).
    #[must_use]
    pub fn own_generics(&self, def: DefId) -> &[DefId] {
        &self.generics(def)[self.parent_generic_count(def)..]
    }

    #[must_use]
    pub fn qualified_name(&self, def: DefId) -> String {
        let info = self.def(def);
        match info.parent {
            Some(parent) => format!("{}::{}", self.def(parent).name, info.name),
            None => info.name.clone(),
        }
    }

    /// The associated item (function or constant) `name` of the struct or enum `adt`, if it
    /// has one.
    #[must_use]
    pub fn assoc_item(&self, adt: DefId, name: &str) -> Option<DefId> {
        self.assoc_items.get(&(adt, name.to_owned())).copied()
    }

    /// The primitive associated constant (`i32::MAX`) a constant definition stands for.
    #[must_use]
    pub fn prim_const(&self, def: DefId) -> Option<PrimConst> {
        self.prim_consts.get(&def).copied()
    }

    /// Every primitive associated constant the file uses, by definition, in order.
    pub fn prim_consts(&self) -> impl Iterator<Item = (DefId, PrimConst)> + '_ {
        let mut all: Vec<(DefId, PrimConst)> =
            self.prim_consts.iter().map(|(d, c)| (*d, *c)).collect();
        all.sort_by_key(|(d, _)| *d);
        all.into_iter()
    }

    /// The associated function `name` of the struct or enum `adt`, if it has one.
    #[must_use]
    pub fn assoc_fn(&self, adt: DefId, name: &str) -> Option<DefId> {
        self.assoc_item(adt, name)
            .filter(|&def| self.def(def).kind == DefKind::Fn)
    }

    /// The `drop` method of the `impl Drop` block of the struct or enum `adt`, if it has
    /// one.
    #[must_use]
    pub fn drop_impl(&self, adt: DefId) -> Option<DefId> {
        self.drop_impls.get(&adt).copied()
    }

    /// Whether `def` is the `drop` method of an `impl Drop` block.
    #[must_use]
    pub fn is_drop_fn(&self, def: DefId) -> bool {
        self.drop_impls.values().any(|&d| d == def)
    }

    /// The definition of a language trait (decision 86).
    #[must_use]
    pub fn lang_item(&self, item: LangItem) -> Option<DefId> {
        self.lang_items.get(&item).copied()
    }

    /// Which language trait `def` is, if any.
    #[must_use]
    pub fn lang_of(&self, def: DefId) -> Option<LangItem> {
        self.lang_items
            .iter()
            .find_map(|(&item, &d)| (d == def).then_some(item))
    }

    /// Whether `def` is the method of the language trait `item` (`Clone::clone`,
    /// `Drop::drop`).
    #[must_use]
    pub fn is_lang_method(&self, def: DefId, item: LangItem) -> bool {
        self.lang_item(item)
            .is_some_and(|tr| self.def(def).parent == Some(tr))
    }

    /// The members of the trait `def`, in declaration order.
    #[must_use]
    pub fn trait_members(&self, def: DefId) -> &[DefId] {
        self.trait_members.get(&def).map_or(&[], Vec::as_slice)
    }

    /// The member `name` of the trait `def`.
    #[must_use]
    pub fn trait_item(&self, def: DefId, name: &str) -> Option<DefId> {
        self.trait_members(def)
            .iter()
            .copied()
            .find(|&m| self.def(m).name == name)
    }

    /// Every trait member named `name`, in source order: the candidates of `Type::Name`.
    #[must_use]
    pub fn trait_items_named(&self, name: &str) -> Vec<DefId> {
        let mut out: Vec<DefId> = self
            .trait_members
            .values()
            .flatten()
            .copied()
            .filter(|&m| self.def(m).name == name)
            .collect();
        out.sort();
        out
    }

    /// Whether `def` is a method a trait declares without a body.
    #[must_use]
    pub fn is_required(&self, def: DefId) -> bool {
        self.required.contains(&def)
    }

    /// The `Self` type parameter of the trait `def`.
    #[must_use]
    pub fn trait_self(&self, def: DefId) -> DefId {
        self.generics(def)[0]
    }

    /// The supertraits of the trait `def`, as written.
    #[must_use]
    pub fn supertraits(&self, def: DefId) -> &[DefId] {
        self.supertraits.get(&def).map_or(&[], Vec::as_slice)
    }

    /// The trait the trait `impl` block `def` implements.
    #[must_use]
    pub fn impl_trait(&self, def: DefId) -> Option<DefId> {
        self.impl_traits.get(&def).copied()
    }

    /// The member `name` of the trait `impl` block `def`.
    #[must_use]
    pub fn impl_item(&self, def: DefId, name: &str) -> Option<DefId> {
        self.impl_items.get(&(def, name.to_owned())).copied()
    }

    /// The bounds written on a function (and its inherent `impl` block), a trait `impl`
    /// block or a trait method; not the implied `Self: Trait` of a trait's members.
    #[must_use]
    pub fn predicates(&self, def: DefId) -> &[Predicate] {
        self.predicates.get(&def).map_or(&[], Vec::as_slice)
    }

    /// What the first segment of the path expression `expr` names, when the path names a
    /// trait's member (see the field).
    #[must_use]
    pub fn assoc_self(&self, expr: NodeId) -> Option<Res> {
        self.assoc_self.get(&expr).copied()
    }

    /// The kind of the definition `def`'s parent, if it has one.
    #[must_use]
    pub fn parent_kind(&self, def: DefId) -> Option<DefKind> {
        self.def(def).parent.map(|p| self.def(p).kind)
    }

    /// The struct or enum whose inherent `impl` block `def` is a member of, or whose
    /// `impl Drop` block's `drop` it is: code generation makes that a method of the
    /// type's class (decision 55).
    #[must_use]
    pub fn inherent_parent(&self, def: DefId) -> Option<DefId> {
        self.def(def)
            .parent
            .filter(|&p| self.def(p).kind.is_type())
            .or_else(|| {
                self.drop_impls
                    .iter()
                    .find_map(|(&adt, &d)| (d == def).then_some(adt))
            })
    }

    /// The variants of the enum `def`, in declaration order (empty for anything else).
    #[must_use]
    pub fn variants(&self, def: DefId) -> &[DefId] {
        self.variants.get(&def).map_or(&[], Vec::as_slice)
    }

    /// The index of a variant within its enum.
    #[must_use]
    pub fn variant_index(&self, variant: DefId) -> usize {
        let parent = self
            .def(variant)
            .parent
            .expect("a variant has its enum as parent");
        self.variants(parent)
            .iter()
            .position(|&v| v == variant)
            .expect("a variant is among its enum's variants")
    }

    /// What a pattern names, for an identifier pattern that compares (a constant or a unit
    /// struct: `None` for a binding) and for path, tuple-struct and struct patterns.
    #[must_use]
    pub fn pat_res(&self, pat: NodeId) -> Option<Res> {
        self.pat_res.get(&pat).copied()
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

    /// Whether `def` is defined by the `core` prelude (decision 90), not by the user's
    /// file. Builtins and language traits are neither.
    #[must_use]
    pub fn is_prelude(&self, def: DefId) -> bool {
        let index = self.def(def).item_index;
        index != usize::MAX && index >= self.prelude_start
    }

    /// The name of `def` in the names of generated code: its name, qualified as
    /// `core::Name` for an item of the prelude (decision 90), so a user item of the same
    /// name never collides with it.
    #[must_use]
    pub fn symbol_name(&self, def: DefId) -> String {
        let name = &self.def(def).name;
        if self.is_prelude(def) {
            format!("core::{name}")
        } else {
            name.clone()
        }
    }

    /// Whether the user's file has an `impl` block for `adt` (a type of the prelude) that
    /// the orphan rule rejects: errors that only say its methods or trait are missing
    /// follow from that one.
    #[must_use]
    pub fn has_orphan_impl(&self, adt: DefId) -> bool {
        self.orphan_targets.contains(&adt)
    }

    /// The index of the prelude's first item among the file's items.
    #[must_use]
    pub fn prelude_start(&self) -> usize {
        self.prelude_start
    }

    /// Whether the function `def` is `#[track_caller]`.
    #[must_use]
    pub fn is_track_caller(&self, def: DefId) -> bool {
        self.track_caller.contains(&def)
    }

    /// The intrinsic the prelude function `def` declares, if it is one.
    #[must_use]
    pub fn intrinsic(&self, def: DefId) -> Option<Intrinsic> {
        self.intrinsics.get(&def).copied()
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

    /// The local introduced by an identifier pattern; `None` for `_` and for an identifier
    /// that names a constant or a unit struct.
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
pub fn resolve(file: &mut SourceFile, sink: &mut DiagnosticSink) -> ResolveResults {
    let items = file.items.len();
    resolve_program(file, items, sink)
}

/// Resolves every name in `file`, whose items from `prelude_start` on are the `core`
/// prelude's (decision 90). The prelude's items resolve in a namespace of their own (its
/// items, its `use` imports, the builtins and the language traits), so a user item never
/// changes what prelude code means; the user's items see their own first, then the
/// prelude's public items and imports, then the builtins and language traits, so a user
/// item of the same name shadows a prelude item silently, as in rustc.
#[must_use]
pub fn resolve_program(
    file: &mut SourceFile,
    prelude_start: usize,
    sink: &mut DiagnosticSink,
) -> ResolveResults {
    let mut r = Resolver {
        results: ResolveResults {
            prelude_start,
            ..ResolveResults::default()
        },
        unit_structs: std::collections::HashSet::new(),
        items: HashMap::new(),
        other_items: HashMap::new(),
        in_prelude: false,
        scopes: Vec::new(),
        sink,
        current_fn: None,
        ty_params: Vec::new(),
        impl_params: HashMap::new(),
        impl_scope: ImplScope::Outside,
        const_floor: None,
        impl_adts: HashMap::new(),
        assoc_self: None,
    };
    r.collect_items(file);
    r.collect_impl_defs(file);
    r.collect_generics(file);
    r.collect_impls(file);
    r.enter_namespace(false);
    r.collect_bounds(file);
    r.enter_namespace(false);
    for (index, item) in file.items.iter().enumerate() {
        r.enter(index);
        r.resolve_item(item, index);
    }
    r.enter_namespace(false);
    r.collect_fn_attrs(file);
    let results = r.results;
    path_patterns(file, &results);
    results
}

/// How rustc names a kind of item in "cannot be used on ..." (E0224).
fn item_kind_plural(kind: &ItemKind) -> &'static str {
    match kind {
        ItemKind::Fn(_) => "functions",
        ItemKind::Struct(_) => "structs",
        ItemKind::Const(_) => "constants",
        ItemKind::Impl(imp) if imp.trait_ref.is_some() => "trait impl blocks",
        ItemKind::Impl(_) => "inherent impl blocks",
        ItemKind::Enum(_) => "enums",
        ItemKind::Trait(_) => "traits",
        ItemKind::Use(_) => "use statements",
    }
}

/// Rewrites every identifier pattern that names a constant, a unit struct or a variant
/// (`None`, `LIMIT`), and every place on the left of a destructuring assignment that names
/// a unit struct or a variant, into the path pattern it is, as rustc's resolver decides
/// it. The later stages then only ever see such a name as a [`PatKind::Path`], whose
/// resolution is the pattern's [`ResolveResults::pat_res`] as for any path pattern; the
/// patterns that bind stay identifier patterns and assignees.
fn path_patterns(file: &mut SourceFile, results: &ResolveResults) {
    file.for_each_pat_mut(&mut |pat| {
        if results.local_of_pat(pat.id).is_some() || results.pat_res(pat.id).is_none() {
            return;
        }
        let path = match &pat.kind {
            PatKind::Ident {
                by_ref: false,
                mutable: false,
                name,
                sub: None,
            } => Path::single(name.clone()),
            PatKind::Assignee(e) => {
                let mut target = e.as_ref();
                while let ExprKind::Paren(inner) = &target.kind {
                    target = inner;
                }
                let ExprKind::Path(path) = &target.kind else {
                    return;
                };
                path.clone()
            }
            _ => return,
        };
        pat.kind = PatKind::Path(path);
    });
}

/// A name a pattern binds.
#[derive(Clone, Debug)]
struct Binding {
    name: String,
    local: LocalId,
    span: Span,
    by_ref: bool,
    mutable: bool,
}

struct Resolver<'d> {
    results: ResolveResults,
    /// The unit structs of the file, which a lone identifier in a pattern names.
    unit_structs: std::collections::HashSet<DefId>,
    /// First definition of each item name, in the namespace of the item being resolved:
    /// the user's file or the prelude (decision 90).
    items: HashMap<String, DefId>,
    /// The other namespace; [`Resolver::enter`] swaps the two.
    other_items: HashMap<String, DefId>,
    /// Whether `items` is the prelude's namespace.
    in_prelude: bool,
    scopes: Vec<Vec<(String, LocalId)>>,
    sink: &'d mut DiagnosticSink,
    current_fn: Option<DefId>,
    /// The type parameters in scope: those of the item being resolved.
    ty_params: Vec<(String, DefId)>,
    /// The names an `impl` block (by item index) gives its type's parameters
    /// (`impl<U> S<U>` names `S`'s first parameter `U`), decision 83.
    impl_params: HashMap<usize, Vec<(String, DefId)>>,
    /// Whether the resolver is inside an `impl` block, and for which struct: what `Self`
    /// names.
    impl_scope: ImplScope,
    /// Inside an array length or repeat count (a constant of its own, rustc's anonymous
    /// constant): the number of scopes outside it, whose locals cannot be used there.
    const_floor: Option<usize>,
    /// The struct or enum each trait `impl` block's type names, if it names one: what
    /// `Self::Variant` and `Self { .. }` mean in it.
    impl_adts: HashMap<DefId, Option<DefId>>,
    /// Set by `resolve_assoc_path` when the path names a trait's member: what its first
    /// segment names (see `ResolveResults::assoc_self`).
    assoc_self: Option<Res>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImplScope {
    Outside,
    /// An `impl` block whose type did not resolve (already reported).
    Unresolved,
    Struct(DefId),
    /// A trait's members: `Self` is the trait's `Self` parameter.
    Trait(DefId),
    /// A trait `impl` block: `Self` is its type.
    TraitImpl(DefId),
}

impl Resolver<'_> {
    fn new_def(&mut self, info: DefInfo) -> DefId {
        let id = DefId(u32::try_from(self.results.defs.len()).expect("too many items"));
        self.results.defs.push(info);
        id
    }

    /// Reads the attributes of every function that change how it is compiled:
    /// `#[track_caller]` (any function but a trait's member and `Drop::drop`, which are
    /// E0107 for now) and, in the prelude, `#[intrinsic]`.
    fn collect_fn_attrs(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            let in_prelude = index >= self.results.prelude_start;
            let (members, in_trait): (&[Item], bool) = match &item.kind {
                ItemKind::Fn(_) => (std::slice::from_ref(item), false),
                ItemKind::Impl(imp) => (&imp.items, false),
                ItemKind::Trait(t) => (&t.items, true),
                _ => (&[], false),
            };
            if !matches!(item.kind, ItemKind::Fn(_)) {
                self.misplaced_fn_attrs(&item.attrs, item_kind_plural(&item.kind));
            }
            if let ItemKind::Enum(e) = &item.kind {
                for v in &e.variants {
                    self.misplaced_fn_attrs(&v.attrs, "enum variants");
                }
            }
            for member in members {
                if !matches!(member.kind, ItemKind::Fn(_)) {
                    self.misplaced_fn_attrs(&member.attrs, "associated consts");
                    continue;
                }
                for attr in &member.attrs {
                    if !attr.path.is_single() {
                        continue;
                    }
                    match attr.path.segments[0].name.as_str() {
                        "track_caller" => self.track_caller_attr(member, attr, in_trait),
                        "intrinsic" => self.intrinsic_attr(member, attr, in_prelude),
                        _ => {}
                    }
                }
            }
        }
    }

    /// E0224 (rustc's E0739): `#[track_caller]` or `#[intrinsic]` on an item that is not
    /// a function, which would otherwise be ignored.
    fn misplaced_fn_attrs(&mut self, attrs: &[sabiko_ast::Attribute], on: &str) {
        for attr in attrs {
            let name = match attr.path.segments.as_slice() {
                [seg] if matches!(seg.name.as_str(), "track_caller" | "intrinsic") => &seg.name,
                _ => continue,
            };
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::MISPLACED_ATTRIBUTE,
                    format!("the `{name}` attribute cannot be used on {on}"),
                )
                .with_label(attr.span, "can only be applied to functions"),
            );
        }
    }

    fn track_caller_attr(&mut self, f: &Item, attr: &sabiko_ast::Attribute, in_trait: bool) {
        let def = self.results.def_of_item(f.id);
        let is_drop = self
            .results
            .def(def)
            .parent
            .and_then(|p| self.results.impl_trait(p))
            .is_some_and(|tr| self.results.lang_of(tr) == Some(LangItem::Drop));
        if in_trait || is_drop {
            let what = if in_trait {
                "a trait's method"
            } else {
                "`Drop::drop`"
            };
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::UNSUPPORTED_SYNTAX,
                    format!("not yet supported: `#[track_caller]` on {what}"),
                )
                .with_label(attr.span, "only free functions and the methods of `impl` blocks can track their caller for now"),
            );
            return;
        }
        self.results.track_caller.insert(def);
    }

    fn intrinsic_attr(&mut self, f: &Item, attr: &sabiko_ast::Attribute, in_prelude: bool) {
        let def = self.results.def_of_item(f.id);
        let name = &self.results.def(def).name;
        match Intrinsic::ALL.iter().find(|i| i.name() == name) {
            Some(&intrinsic) if in_prelude => {
                self.results.intrinsics.insert(def, intrinsic);
            }
            _ => {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNSUPPORTED_SYNTAX,
                        "the `#[intrinsic]` attribute is reserved for the `core` library",
                    )
                    .with_label(attr.span, "not an intrinsic the compiler knows"),
                );
            }
        }
    }

    /// Makes `items` the namespace of the item at `index`: the user's or the prelude's.
    fn enter(&mut self, index: usize) {
        self.enter_namespace(index >= self.results.prelude_start);
    }

    /// Makes `items` the prelude's namespace (`prelude`) or the user's.
    fn enter_namespace(&mut self, prelude: bool) {
        if prelude != self.in_prelude {
            std::mem::swap(&mut self.items, &mut self.other_items);
            self.in_prelude = prelude;
        }
    }

    fn has_prelude(&self, file: &SourceFile) -> bool {
        self.results.prelude_start < file.items.len()
    }

    fn collect_items(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            self.enter(index);
            let kind = match &item.kind {
                ItemKind::Use(_) => continue,
                ItemKind::Fn(_) => DefKind::Fn,
                ItemKind::Struct(_) => DefKind::Struct,
                ItemKind::Const(_) => DefKind::Const,
                ItemKind::Enum(_) => DefKind::Enum,
                ItemKind::Trait(_) => DefKind::Trait,
                ItemKind::Impl(imp) => {
                    // The members are definitions of their own, in source order, but not in
                    // the item namespace: they are reached as `Type::Name` or as methods.
                    for (member_index, member) in imp.items.iter().enumerate() {
                        let (kind, name) = match &member.kind {
                            ItemKind::Fn(f) => (DefKind::Fn, &f.name),
                            ItemKind::Const(c) => (DefKind::Const, &c.name),
                            _ => continue,
                        };
                        let id = self.new_def(DefInfo {
                            kind,
                            name: name.name.clone(),
                            span: name.span,
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
            if let ItemKind::Enum(e) = &item.kind {
                self.collect_variants(id, index, e);
            }
            if let ItemKind::Trait(t) = &item.kind {
                self.collect_trait_members(id, index, t);
            }
            if let ItemKind::Struct(sabiko_ast::StructDecl {
                kind: StructKind::Unit,
                ..
            }) = &item.kind
            {
                self.unit_structs.insert(id);
            }
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
        // The prelude's imports, with its items known (decision 90).
        let prelude = self.has_prelude(file);
        if prelude {
            self.enter_namespace(true);
            self.collect_uses(file);
        }
        self.enter_namespace(false);
        // The user's namespace: its own items (above), then the prelude's exports, then
        // the builtins and the language traits, each only where the name is still free.
        if prelude {
            self.export_prelude(file);
        }
        self.collect_builtins(prelude);
        self.collect_lang_items();
    }

    /// The builtins (decision 28). They come after the items so user items keep the
    /// `DefId`s they would have without them. A user item of the same name shadows a
    /// builtin without a diagnostic; the prelude (`prelude`) still sees the builtin.
    fn collect_builtins(&mut self, prelude: bool) {
        for builtin in Builtin::ALL {
            let shadowed = self.items.contains_key(builtin.name());
            if shadowed && !prelude {
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
            if !shadowed {
                self.items.insert(builtin.name().to_owned(), id);
            }
            self.other_items
                .entry(builtin.name().to_owned())
                .or_insert(id);
        }
    }

    /// Brings the prelude's public items and imports into the user's namespace, under
    /// the user's own items (decision 90): rustc's `std` prelude is a glob import, which
    /// any item of the crate shadows.
    fn export_prelude(&mut self, file: &SourceFile) {
        debug_assert!(!self.in_prelude);
        let mut exports: Vec<(String, DefId)> = Vec::new();
        for item in &file.items[self.results.prelude_start..] {
            if item.vis != sabiko_ast::Visibility::Public {
                continue;
            }
            match &item.kind {
                ItemKind::Use(u) => {
                    for name in &u.names {
                        if let Some(&def) = self.other_items.get(&name.name) {
                            exports.push((name.name.clone(), def));
                        }
                    }
                }
                ItemKind::Impl(_) => {}
                _ => {
                    if let Some(def) = self.results.item_defs.get(&item.id) {
                        let name = item.name().expect("only `impl` and `use` have no name");
                        exports.push((name.name.clone(), *def));
                    }
                }
            }
        }
        for (name, def) in exports {
            self.items.entry(name).or_insert(def);
        }
    }

    /// The prelude's `use` declarations: each names the variants of an enum of the
    /// prelude (`use Option::{None, Some};`), which enter the prelude's namespace. The
    /// prelude is the compiler's own source, so a `use` it cannot resolve is an internal
    /// error, reported as an unresolved name.
    fn collect_uses(&mut self, file: &SourceFile) {
        debug_assert!(self.in_prelude);
        for item in &file.items[self.results.prelude_start..] {
            let ItemKind::Use(u) = &item.kind else {
                continue;
            };
            let owner = if u.prefix.is_single() {
                self.items
                    .get(&u.prefix.segments[0].name)
                    .copied()
                    .filter(|&d| self.results.def(d).kind == DefKind::Enum)
            } else {
                None
            };
            let Some(owner) = owner else {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::UNRESOLVED_TYPE,
                        format!(
                            "cannot find enum `{}` to import from",
                            sabiko_ast::dump::path_text(&u.prefix, "")
                        ),
                    )
                    .with_label(
                        u.prefix.span,
                        "only the variants of an enum can be imported",
                    ),
                );
                continue;
            };
            for name in &u.names {
                let Some(variant) = self.variant(owner, &name.name) else {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::UNRESOLVED_VALUE,
                            format!(
                                "no variant `{}` in enum `{}`",
                                name.name,
                                self.results.def(owner).name
                            ),
                        )
                        .with_label(name.span, "not found"),
                    );
                    continue;
                };
                if let Some(&first) = self.items.get(&name.name) {
                    let first_span = self.results.def(first).span;
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::DUPLICATE_DEFINITION,
                            format!("the name `{}` is defined multiple times", name.name),
                        )
                        .with_label(name.span, format!("`{}` reimported here", name.name))
                        .with_label(
                            first_span,
                            format!("previous definition of `{}` here", name.name),
                        ),
                    );
                    continue;
                }
                self.items.insert(name.name.clone(), variant);
            }
        }
    }

    /// The language traits (decision 86), each with its `Self` parameter and its method,
    /// both definitions without a source item; `Copy` has `Clone` as supertrait. A user
    /// item of the same name shadows one without a diagnostic.
    fn collect_lang_items(&mut self) {
        for item in LangItem::ALL {
            let tr = self.new_def(DefInfo {
                kind: DefKind::Trait,
                name: item.name().to_owned(),
                span: Span::dummy(),
                node: NodeId::DUMMY,
                item_index: usize::MAX,
                member_index: None,
                parent: None,
                builtin: None,
            });
            let self_param = self.new_def(DefInfo {
                kind: DefKind::TyParam,
                name: "Self".to_owned(),
                span: Span::dummy(),
                node: NodeId::DUMMY,
                item_index: usize::MAX,
                member_index: None,
                parent: Some(tr),
                builtin: None,
            });
            self.results.generics.insert(tr, vec![self_param]);
            let mut members = Vec::new();
            if let Some((name, _)) = item.method() {
                let m = self.new_def(DefInfo {
                    kind: DefKind::Fn,
                    name: name.to_owned(),
                    span: Span::dummy(),
                    node: NodeId::DUMMY,
                    item_index: usize::MAX,
                    member_index: None,
                    parent: Some(tr),
                    builtin: None,
                });
                self.results.required.insert(m);
                self.results.generics.insert(m, vec![self_param]);
                self.results.parent_generics.insert(m, 1);
                members.push(m);
            }
            self.results.trait_members.insert(tr, members);
            self.results.supertraits.insert(tr, Vec::new());
            self.results.lang_items.insert(item, tr);
            if !self.items.contains_key(item.name()) {
                self.items.insert(item.name().to_owned(), tr);
            }
            self.other_items.entry(item.name().to_owned()).or_insert(tr);
        }
        let clone = self.results.lang_items[&LangItem::Clone];
        let copy = self.results.lang_items[&LangItem::Copy];
        self.results.supertraits.insert(copy, vec![clone]);
    }

    /// The members of the trait `tr`, each a definition with the trait as parent. A second
    /// member of one name is E0210 (rustc's E0428) and gets no entry among its members.
    fn collect_trait_members(&mut self, tr: DefId, index: usize, t: &sabiko_ast::TraitDecl) {
        let mut members: Vec<DefId> = Vec::new();
        for (member_index, member) in t.items.iter().enumerate() {
            let (kind, name, required) = match &member.kind {
                ItemKind::Fn(f) => (DefKind::Fn, &f.name, f.body.is_none()),
                ItemKind::Const(c) => (DefKind::Const, &c.name, c.value.is_none()),
                _ => continue,
            };
            let id = self.new_def(DefInfo {
                kind,
                name: name.name.clone(),
                span: name.span,
                node: member.id,
                item_index: index,
                member_index: Some(member_index),
                parent: Some(tr),
                builtin: None,
            });
            self.results.item_defs.insert(member.id, id);
            if required {
                self.results.required.insert(id);
            }
            if let Some(&prev) = members
                .iter()
                .find(|&&m| self.results.def(m).name == name.name)
            {
                let prev_span = self.results.def(prev).span;
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_DEFINITION,
                        format!("the name `{}` is defined multiple times", name.name),
                    )
                    .with_label(name.span, format!("`{}` redefined here", name.name))
                    .with_label(
                        prev_span,
                        format!("previous definition of `{}` here", name.name),
                    )
                    .with_note("a trait's members must have distinct names"),
                );
                continue;
            }
            members.push(id);
        }
        self.results.trait_members.insert(tr, members);
    }

    /// A definition for every trait `impl` block, made after every item so the items keep
    /// their `DefId`s.
    fn collect_impl_defs(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            let ItemKind::Impl(imp) = &item.kind else {
                continue;
            };
            let Some(tr) = &imp.trait_ref else {
                continue;
            };
            let name = format!(
                "<{} as {}>",
                sabiko_ast::pretty::type_str(&imp.self_ty),
                sabiko_ast::dump::path_text(&tr.path, "")
            );
            let id = self.new_def(DefInfo {
                kind: DefKind::Impl,
                name,
                span: Span::new(item.span.file, item.span.start, imp.self_ty.span.end),
                node: item.id,
                item_index: index,
                member_index: None,
                parent: None,
                builtin: None,
            });
            self.results.item_defs.insert(item.id, id);
        }
    }

    /// The type parameters of every generic function, each a definition with its function
    /// as parent. They are made after every item (and builtin), so the items keep their
    /// `DefId`s. A second parameter of one name is E0221 (rustc's E0403) and gets no
    /// definition.
    fn collect_generics(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            match &item.kind {
                ItemKind::Fn(f) => self.collect_params(item.id, index, &f.generics),
                ItemKind::Struct(st) => self.collect_params(item.id, index, &st.generics),
                ItemKind::Enum(e) => self.collect_params(item.id, index, &e.generics),
                ItemKind::Impl(imp) => {
                    if self.results.item_defs.contains_key(&item.id) {
                        // A trait impl's parameters are its own.
                        self.collect_params(item.id, index, &imp.generics);
                    }
                    for member in &imp.items {
                        if let ItemKind::Fn(f) = &member.kind {
                            self.collect_params(member.id, index, &f.generics);
                        }
                    }
                }
                ItemKind::Trait(t) => {
                    let tr = self.results.def_of_item(item.id);
                    let self_param = self.new_def(DefInfo {
                        kind: DefKind::TyParam,
                        name: "Self".to_owned(),
                        span: t.name.span,
                        node: NodeId::DUMMY,
                        item_index: index,
                        member_index: None,
                        parent: Some(tr),
                        builtin: None,
                    });
                    self.results.generics.insert(tr, vec![self_param]);
                    for member in &t.items {
                        match &member.kind {
                            ItemKind::Fn(f) => {
                                self.collect_params(member.id, index, &f.generics);
                            }
                            ItemKind::Const(_) => {}
                            _ => continue,
                        }
                        let def = self.results.def_of_item(member.id);
                        self.inherit_generics_of(def, tr);
                    }
                }
                ItemKind::Const(_) | ItemKind::Use(_) => {}
            }
        }
    }

    /// The type parameters `params` of the item at `node` (a function, struct or enum).
    fn collect_params(&mut self, node: NodeId, index: usize, params: &[sabiko_ast::GenericParam]) {
        if params.is_empty() {
            return;
        }
        let parent = self.results.def_of_item(node);
        let mut seen: HashMap<&str, Span> = HashMap::new();
        let mut ids = Vec::new();
        for g in params {
            if let Some(&prev) = seen.get(g.name.name.as_str()) {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_GENERIC_PARAM,
                        format!(
                            "the name `{}` is already used for a generic parameter in this item's generic parameters",
                            g.name.name
                        ),
                    )
                    .with_label(g.name.span, "already used")
                    .with_label(prev, format!("first use of `{}`", g.name.name)),
                );
                continue;
            }
            seen.insert(&g.name.name, g.name.span);
            let id = self.new_def(DefInfo {
                kind: DefKind::TyParam,
                name: g.name.name.clone(),
                span: g.name.span,
                node: g.id,
                item_index: index,
                member_index: None,
                parent: Some(parent),
                builtin: None,
            });
            self.results.item_defs.insert(g.id, id);
            ids.push(id);
        }
        self.results.generics.insert(parent, ids);
    }

    /// The variants of the enum `adt`, each a definition with the enum as parent. A second
    /// variant of one name is E0200 (rustc's E0428) and gets no definition.
    fn collect_variants(&mut self, adt: DefId, index: usize, e: &sabiko_ast::EnumDecl) {
        let mut ids = Vec::new();
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for v in &e.variants {
            if let Some(&prev) = seen.get(v.name.name.as_str()) {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_DEFINITION,
                        format!("the name `{}` is defined multiple times", v.name.name),
                    )
                    .with_label(v.name.span, format!("`{}` redefined here", v.name.name))
                    .with_label(
                        prev,
                        format!("previous definition of `{}` here", v.name.name),
                    )
                    .with_note("a variant name must be unique within its enum"),
                );
                continue;
            }
            seen.insert(&v.name.name, v.name.span);
            let id = self.new_def(DefInfo {
                kind: DefKind::Variant,
                name: v.name.name.clone(),
                span: v.name.span,
                node: v.id,
                item_index: index,
                member_index: None,
                parent: Some(adt),
                builtin: None,
            });
            self.results.item_defs.insert(v.id, id);
            ids.push(id);
        }
        self.results.variants.insert(adt, ids);
    }

    /// The variant `name` of the enum `adt`.
    fn variant(&self, adt: DefId, name: &str) -> Option<DefId> {
        self.results
            .variants(adt)
            .iter()
            .copied()
            .find(|&v| self.results.def(v).name == name)
    }

    /// Resolves the type of every `impl` block, records each member's parent struct and
    /// registers it under `(struct, name)`. Two members of one name for the same struct are
    /// reported as rustc does: at the later one when both are in one block, at the earlier
    /// one when they are in different blocks.
    fn collect_impls(&mut self, file: &SourceFile) {
        // The first definition of each `(struct, name)`, with the index of its block.
        let mut first: HashMap<(DefId, String), (DefId, usize)> = HashMap::new();
        for (index, item) in file.items.iter().enumerate() {
            self.enter(index);
            let ItemKind::Impl(imp) = &item.kind else {
                continue;
            };
            if let Some(tr) = &imp.trait_ref {
                self.collect_trait_impl(item, index, imp, tr);
                continue;
            }
            let params = self.impl_param_map(imp);
            self.ty_params.clone_from(&params);
            self.impl_params.insert(index, params);
            let target = self.resolve_impl_target(&imp.self_ty);
            self.ty_params.clear();
            let Some(adt) = target else {
                continue;
            };
            let foreign = self.is_foreign(adt);
            if foreign {
                self.foreign_inherent_impl(adt, &imp.self_ty);
            }
            for member in &imp.items {
                let Some(ident) = member.name() else {
                    continue;
                };
                let def = self.results.def_of_item(member.id);
                self.results.defs[def.0 as usize].parent = Some(adt);
                self.inherit_generics_of(def, adt);
                if foreign {
                    // Its bodies are checked as any block's, but the type gets no new
                    // members.
                    continue;
                }
                let key = (adt, ident.name.clone());
                let Some(&(prev, prev_block)) = first.get(&key) else {
                    first.insert(key.clone(), (def, index));
                    self.results.assoc_items.insert(key, def);
                    continue;
                };
                let prev_span = self.results.def(prev).span;
                let name = &ident.name;
                let (primary, other) = if prev_block == index {
                    (ident.span, prev_span)
                } else {
                    (prev_span, ident.span)
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

    /// A trait `impl` block: its trait (reported when it is no trait of the file), its type,
    /// and its members, which get the block as parent and must be the trait's (E0217,
    /// rustc's E0407) and include each of its required methods (E0216, E0046). Two members
    /// of one name are E0210.
    fn collect_trait_impl(
        &mut self,
        item: &Item,
        index: usize,
        imp: &ImplDecl,
        tr: &sabiko_ast::TraitRef,
    ) {
        let imp_def = self.results.def_of_item(item.id);
        let params: Vec<(String, DefId)> = self
            .results
            .generics(imp_def)
            .iter()
            .map(|&p| (self.results.def(p).name.clone(), p))
            .collect();
        self.impl_params.insert(index, params.clone());
        self.ty_params = params;
        let trait_ = self.resolve_trait_ref(tr);
        self.impl_scope = ImplScope::TraitImpl(imp_def);
        self.resolve_type(&imp.self_ty, true);
        self.impl_scope = ImplScope::Outside;
        self.ty_params.clear();
        let adt = match &imp.self_ty.kind {
            TypeKind::Path(_) => match self.results.res(imp.self_ty.id) {
                Res::Def(d) if self.results.def(d).kind.is_type() => Some(d),
                _ => None,
            },
            _ => None,
        };
        self.impl_adts.insert(imp_def, adt);
        // The orphan rule (rustc's E0117): a trait that is not the file's own (a language
        // trait, or one of the prelude's) only for a type of the file. Such a block is
        // left without its trait, so nothing else is checked against it.
        let trait_ = trait_.filter(|&t| {
            let foreign_trait = self.results.lang_of(t).is_some() || self.is_foreign(t);
            match adt {
                Some(adt) if foreign_trait && self.is_foreign(adt) => {
                    self.foreign_trait_impl(t, adt, &imp.self_ty);
                    false
                }
                _ => true,
            }
        });
        if let Some(t) = trait_ {
            self.results.impl_traits.insert(imp_def, t);
            if let Some(lang) = self.results.lang_of(t) {
                // An unknown type name was reported already.
                let unknown = matches!(imp.self_ty.kind, TypeKind::Path(_))
                    && self.results.res(imp.self_ty.id) == Res::Err;
                if adt.is_none() && !unknown {
                    self.invalid_lang_impl(lang, &imp.self_ty);
                }
            }
        }
        let trait_name = sabiko_ast::dump::path_text(&tr.path, "");
        for member in &imp.items {
            let Some(ident) = member.name() else {
                continue;
            };
            let def = self.results.def_of_item(member.id);
            self.results.defs[def.0 as usize].parent = Some(imp_def);
            self.inherit_generics_of(def, imp_def);
            let key = (imp_def, ident.name.clone());
            if let Some(&prev) = self.results.impl_items.get(&key) {
                let prev_span = self.results.def(prev).span;
                let name = &ident.name;
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_ASSOC_ITEM,
                        format!("duplicate definitions with name `{name}`"),
                    )
                    .with_label(ident.span, format!("duplicate definitions for `{name}`"))
                    .with_label(prev_span, format!("previous definition of `{name}` here")),
                );
                continue;
            }
            self.results.impl_items.insert(key, def);
            let Some(t) = trait_ else {
                continue;
            };
            self.check_trait_impl_member(t, member, ident, &trait_name);
        }
        if let (Some(t), Some(adt)) = (trait_, adt) {
            if self.results.lang_of(t) == Some(LangItem::Drop) {
                if let Some(f) = self.results.impl_item(imp_def, "drop") {
                    if self.results.def(f).kind == DefKind::Fn {
                        self.results.drop_impls.entry(adt).or_insert(f);
                    }
                }
            }
        }
        let Some(t) = trait_ else {
            return;
        };
        self.report_missing_items(imp_def, t, &trait_name);
    }

    /// The required members of the trait `t` the trait `impl` block `imp_def` does not
    /// define (E0216, rustc's E0046).
    fn report_missing_items(&mut self, imp_def: DefId, t: DefId, trait_name: &str) {
        let missing: Vec<String> = self
            .results
            .trait_members(t)
            .iter()
            .filter(|&&m| self.results.is_required(m))
            .map(|&m| self.results.def(m).name.clone())
            .filter(|name| self.results.impl_item(imp_def, name).is_none())
            .collect();
        if !missing.is_empty() {
            let list: Vec<String> = missing.iter().map(|n| format!("`{n}`")).collect();
            let list = list.join(", ");
            let mut d = Diagnostic::error(
                ErrorCode::MISSING_TRAIT_ITEMS,
                format!("not all trait items implemented, missing: {list}"),
            )
            .with_label(
                self.results.def(imp_def).span,
                format!("missing {list} in implementation"),
            )
            .with_label(
                self.results.def(t).span,
                format!("trait `{trait_name}` declared here"),
            );
            // A language trait has no source to point at (decision 86).
            if let Some(lang) = self.results.lang_of(t) {
                if let Some((name, mutable)) = lang.method() {
                    let receiver = if mutable { "&mut self" } else { "&self" };
                    let ret = if mutable { "" } else { " -> Self" };
                    d = d.with_note(format!(
                        "`impl {trait_name}` must define `fn {name}({receiver}){ret}`"
                    ));
                }
            }
            self.sink.emit(d);
        }
    }

    /// A member `member` (named `ident`) of a trait `impl` block of the trait `t`: one the
    /// trait does not declare is E0217 (rustc's E0407), and so is a method where the trait
    /// has a constant or the reverse (rustc's E0323/E0324).
    fn check_trait_impl_member(
        &mut self,
        t: DefId,
        member: &Item,
        ident: &sabiko_ast::Ident,
        trait_name: &str,
    ) {
        let declared = self.results.trait_item(t, &ident.name);
        let (what, kind) = match &member.kind {
            ItemKind::Const(_) => ("const", DefKind::Const),
            _ => ("method", DefKind::Fn),
        };
        if let Some(d) = declared.filter(|&d| self.results.def(d).kind != kind) {
            // rustc's E0323/E0324.
            let (is, trait_has) = match kind {
                DefKind::Const => ("an associated const", "an associated method"),
                _ => ("an associated method", "an associated const"),
            };
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::NOT_A_TRAIT_MEMBER,
                    format!(
                        "item `{}` is {is}, which doesn't match its trait `{trait_name}`",
                        ident.name
                    ),
                )
                .with_label(ident.span, "does not match trait")
                .with_label(
                    self.results.def(d).span,
                    format!("item in trait is {trait_has}"),
                ),
            );
        } else if declared.is_none() {
            let mut d = Diagnostic::error(
                ErrorCode::NOT_A_TRAIT_MEMBER,
                format!(
                    "{what} `{}` is not a member of trait `{trait_name}`",
                    ident.name
                ),
            )
            .with_label(ident.span, format!("not a member of trait `{trait_name}`"));
            let lang_method = self.results.lang_of(t).and_then(LangItem::method);
            if let Some((name, _)) = lang_method.filter(|(n, _)| ident.name.eq_ignore_ascii_case(n))
            {
                d = d.with_note(format!(
                    "the trait's method is `{name}`, lowercase: it keeps Rust's name, unlike Sabiko's PascalCase methods"
                ));
            }
            self.sink.emit(d);
        }
    }

    /// Whether `def` is defined outside the file being resolved: by the `core` prelude,
    /// seen from the user's file (decision 90). The prelude is a crate of its own there.
    fn is_foreign(&self, def: DefId) -> bool {
        !self.in_prelude && self.results.is_prelude(def)
    }

    /// E0211 (rustc's E0116): an inherent `impl` block for a type of the prelude. Its
    /// members are not the type's, and calls of them are not reported again.
    fn foreign_inherent_impl(&mut self, adt: DefId, ty: &Type) {
        let name = self.results.def(adt).name.clone();
        self.results.orphan_targets.insert(adt);
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::INVALID_IMPL_TARGET,
                "cannot define inherent `impl` for a type outside of the crate where the type is defined",
            )
            .with_label(ty.span, "impl for type defined outside of this file")
            .with_note(format!(
                "`{name}` is defined in Sabiko's `core` library; define a trait and implement it for `{name}` instead"
            )),
        );
    }

    /// E0214 (rustc's E0117): a language trait implemented for a type of the prelude. The
    /// block is left without its trait, and what then fails for lack of it is not
    /// reported again.
    fn foreign_trait_impl(&mut self, tr: DefId, adt: DefId, ty: &Type) {
        self.results.orphan_targets.insert(adt);
        let (tr, adt) = (&self.results.def(tr).name, &self.results.def(adt).name);
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::INVALID_DROP_TARGET,
                "only traits defined in the current crate can be implemented for types defined outside of the crate",
            )
            .with_label(ty.span, format!("`{adt}` is not defined in this file"))
            .with_note(format!(
                "`{tr}` and `{adt}` both come from outside this file (`{adt}` from Sabiko's `core` library); implement a trait of this file for it, or wrap it in a struct of this file"
            )),
        );
    }

    /// A block of a language trait for a type that is no struct or enum of the file
    /// (E0214, rustc's E0120 for `Drop`, E0117 and E0206 for `Copy` and `Clone`): only
    /// those have `impl Drop`, and every other type has its `Copy` and `Clone` built in.
    fn invalid_lang_impl(&mut self, lang: LangItem, ty: &Type) {
        let name = lang.name();
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::INVALID_DROP_TARGET,
                format!(
                    "the `{name}` trait may only be implemented for structs and enums of this file"
                ),
            )
            .with_label(ty.span, "must be a struct or an enum defined in this file"),
        );
    }

    /// The trait a bound, a supertrait or an `impl` header names, recorded under the
    /// reference's node. Reports a name that is no trait of the file.
    fn resolve_trait_ref(&mut self, tr: &sabiko_ast::TraitRef) -> Option<DefId> {
        self.resolve_generic_args(&tr.path, true);
        let res = if tr.path.is_single() {
            let name = &tr.path.segments[0];
            match self.items.get(&name.name) {
                Some(&d) if self.results.def(d).kind == DefKind::Trait => Res::Def(d),
                Some(&d) => {
                    let info = self.results.def(d);
                    let d = Diagnostic::error(
                        ErrorCode::WRONG_NAMESPACE,
                        format!(
                            "expected trait, found {} `{}`",
                            info.kind.describe(),
                            name.name
                        ),
                    )
                    .with_label(name.span, "not a trait");
                    let d = Self::with_definition_label(d, info, &name.name);
                    self.sink.emit(d);
                    Res::Err
                }
                None if name.name == "Sized" => {
                    let note =
                        "every type parameter but a trait's `Self` is `Sized` already; see docs/PLAN.md";
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::UNSUPPORTED_SYNTAX,
                            format!(
                                "not yet supported: the `{}` trait in a bound or an `impl` header",
                                name.name
                            ),
                        )
                        .with_label(name.span, "a language trait")
                        .with_note(note),
                    );
                    Res::Err
                }
                None => {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::UNRESOLVED_TYPE,
                            format!("cannot find trait `{}` in this scope", name.name),
                        )
                        .with_label(name.span, "not found in this scope"),
                    );
                    Res::Err
                }
            }
        } else {
            self.unsupported_path(&tr.path)
        };
        self.check_generic_args(&tr.path, res, true);
        self.results.res.insert(tr.id, res);
        match res {
            Res::Def(d) => Some(d),
            _ => None,
        }
    }

    /// The bounds of every function, trait `impl` block and trait: on their type
    /// parameters (`T: Show`), in `where` clauses (`where T: Show`), and a trait's
    /// supertraits. An inherent `impl` block's bounds become each of its functions'.
    fn collect_bounds(&mut self, file: &SourceFile) {
        for (index, item) in file.items.iter().enumerate() {
            self.enter(index);
            match &item.kind {
                ItemKind::Fn(f) => {
                    let def = self.results.def_of_item(item.id);
                    self.fn_bounds(def, f, &[], Vec::new());
                }
                ItemKind::Impl(imp) => {
                    let params = self.impl_params.get(&index).cloned().unwrap_or_default();
                    let trait_impl =
                        imp.trait_ref.is_some() && self.results.item_defs.contains_key(&item.id);
                    self.impl_scope = if trait_impl {
                        ImplScope::TraitImpl(self.results.def_of_item(item.id))
                    } else {
                        ImplScope::Unresolved
                    };
                    self.ty_params.clone_from(&params);
                    let mut block = self.param_bounds(&imp.generics, &params);
                    block.extend(self.where_bounds(&imp.where_clause));
                    let inherited = if trait_impl {
                        let def = self.results.def_of_item(item.id);
                        self.results.predicates.insert(def, block);
                        Vec::new()
                    } else {
                        block
                    };
                    for member in &imp.items {
                        let def = self.results.def_of_item(member.id);
                        match &member.kind {
                            ItemKind::Fn(f) => {
                                self.fn_bounds(def, f, &params, inherited.clone());
                            }
                            // A constant of a generic struct's or enum's block has the
                            // block's bounds (decision 85).
                            ItemKind::Const(_) if !inherited.is_empty() => {
                                self.results.predicates.insert(def, inherited.clone());
                            }
                            _ => {}
                        }
                    }
                    self.impl_scope = ImplScope::Outside;
                }
                ItemKind::Trait(t) => {
                    let tr = self.results.def_of_item(item.id);
                    let supers: Vec<DefId> = t
                        .supertraits
                        .iter()
                        .filter_map(|s| self.resolve_trait_ref(s))
                        .collect();
                    self.results.supertraits.insert(tr, supers);
                    self.impl_scope = ImplScope::Trait(tr);
                    for member in &t.items {
                        if let ItemKind::Fn(f) = &member.kind {
                            let def = self.results.def_of_item(member.id);
                            self.fn_bounds(def, f, &[], Vec::new());
                        }
                    }
                    self.impl_scope = ImplScope::Outside;
                }
                ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::Const(_) | ItemKind::Use(_) => {
                }
            }
        }
        self.ty_params.clear();
    }

    /// The bounds of the function `def`: those of its block (`inherited`), of its own
    /// type parameters and of its `where` clause, which see the block's parameters
    /// `outer` and its own.
    fn fn_bounds(
        &mut self,
        def: DefId,
        f: &FnDecl,
        outer: &[(String, DefId)],
        inherited: Vec<Predicate>,
    ) {
        let mut params = outer.to_vec();
        params.extend(
            self.results
                .own_generics(def)
                .iter()
                .map(|&p| (self.results.def(p).name.clone(), p)),
        );
        self.ty_params = params;
        let mut preds = inherited;
        let own: Vec<(String, DefId)> = self
            .results
            .own_generics(def)
            .iter()
            .map(|&p| (self.results.def(p).name.clone(), p))
            .collect();
        preds.extend(self.param_bounds(&f.generics, &own));
        preds.extend(self.where_bounds(&f.where_clause));
        if !preds.is_empty() {
            self.results.predicates.insert(def, preds);
        }
        self.ty_params.clear();
    }

    /// The bounds written on the type parameters `generics`, whose definitions `params`
    /// names (by name: an inherent block's parameters are its type's, decision 83).
    fn param_bounds(
        &mut self,
        generics: &[sabiko_ast::GenericParam],
        params: &[(String, DefId)],
    ) -> Vec<Predicate> {
        let mut preds = Vec::new();
        for g in generics {
            let param = params
                .iter()
                .find(|(n, _)| *n == g.name.name)
                .map(|&(_, p)| p);
            for b in &g.bounds {
                let trait_ = self.resolve_trait_ref(b);
                if let (Some(param), Some(trait_)) = (param, trait_) {
                    preds.push(Predicate {
                        param,
                        trait_,
                        span: b.path.span,
                    });
                }
            }
        }
        preds
    }

    /// The bounds of a `where` clause. Its types must be type parameters for now (E0107).
    fn where_bounds(&mut self, clause: &[sabiko_ast::WherePredicate]) -> Vec<Predicate> {
        let mut preds = Vec::new();
        for p in clause {
            self.resolve_type(&p.ty, true);
            let res = match &p.ty.kind {
                TypeKind::Path(_) => self.results.res(p.ty.id),
                _ => Res::Local(LocalId(u32::MAX)),
            };
            let param = match res {
                Res::Def(d) if self.results.def(d).kind == DefKind::TyParam => Some(d),
                Res::Err => None,
                _ => {
                    self.unsupported_where(p.ty.span);
                    None
                }
            };
            for b in &p.bounds {
                let trait_ = self.resolve_trait_ref(b);
                if let (Some(param), Some(trait_)) = (param, trait_) {
                    preds.push(Predicate {
                        param,
                        trait_,
                        span: b.path.span,
                    });
                }
            }
        }
        preds
    }

    fn unsupported_where(&mut self, span: Span) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNSUPPORTED_SYNTAX,
                "not yet supported: a `where` clause on a type other than a type parameter",
            )
            .with_label(
                span,
                "bound a type parameter here, or write the bound on it: `T: Show`",
            )
            .with_note("see docs/PLAN.md"),
        );
    }

    /// The names a generic `impl` block gives its type's parameters (decision 83): its type
    /// must be a generic struct or enum whose arguments are exactly the block's
    /// parameters, each once (`impl<A, B> Pair<A, B>`, or `impl<U> S<U>` for `struct
    /// S<T>`); anything else (`impl<T> Pair<T, T>`, `impl Pair<i32, bool>` with arguments
    /// that are types) is E0107 for now. Empty for a block that is not generic.
    fn impl_param_map(&mut self, imp: &ImplDecl) -> Vec<(String, DefId)> {
        if imp.generics.is_empty() {
            // `impl Pair<i32, bool>`: a block for one instance.
            if let TypeKind::Path(path) = &imp.self_ty.kind {
                let generic = path.is_single()
                    && path.args_of(0).is_some_and(|a| !a.args.is_empty())
                    && self
                        .items
                        .get(&path.segments[0].name)
                        .is_some_and(|&d| self.results.def(d).kind.is_type());
                if generic {
                    self.unsupported_impl_params(imp.self_ty.span);
                }
            }
            return Vec::new();
        }
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for g in &imp.generics {
            if let Some(&prev) = seen.get(g.name.name.as_str()) {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_GENERIC_PARAM,
                        format!(
                            "the name `{}` is already used for a generic parameter in this item's generic parameters",
                            g.name.name
                        ),
                    )
                    .with_label(g.name.span, "already used")
                    .with_label(prev, format!("first use of `{}`", g.name.name)),
                );
                return self.stray_params(imp);
            }
            seen.insert(&g.name.name, g.name.span);
        }
        let target = match &imp.self_ty.kind {
            TypeKind::Path(path) if path.is_single() => self
                .items
                .get(&path.segments[0].name)
                .copied()
                .filter(|&d| self.results.def(d).kind.is_type())
                .map(|d| (d, path)),
            _ => None,
        };
        let Some((adt, path)) = target else {
            self.unsupported_impl_params(imp.self_ty.span);
            return self.stray_params(imp);
        };
        let adt_params = self.results.generics(adt).to_vec();
        let args: &[Type] = path.args_of(0).map_or(&[], |a| a.args.as_slice());
        let mut map: Vec<(String, DefId)> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            let name = match &arg.kind {
                TypeKind::Path(p) if p.is_single() && p.generic_args.is_empty() => {
                    &p.segments[0].name
                }
                _ => break,
            };
            if !imp.generics.iter().any(|g| &g.name.name == name)
                || map.iter().any(|(n, _)| n == name)
            {
                break;
            }
            if let Some(&param) = adt_params.get(i) {
                map.push((name.clone(), param));
            }
        }
        if map.len() != imp.generics.len()
            || map.len() != adt_params.len()
            || args.len() != map.len()
        {
            self.unsupported_impl_params(imp.self_ty.span);
            return self.stray_params(imp);
        }
        map
    }

    /// Definitions of their own for the parameters of an `impl` block that was reported,
    /// so that their uses resolve without more errors.
    fn stray_params(&mut self, imp: &ImplDecl) -> Vec<(String, DefId)> {
        let mut out: Vec<(String, DefId)> = Vec::new();
        for g in &imp.generics {
            if out.iter().any(|(n, _)| *n == g.name.name) {
                continue;
            }
            let id = self.new_def(DefInfo {
                kind: DefKind::TyParam,
                name: g.name.name.clone(),
                span: g.name.span,
                node: g.id,
                item_index: usize::MAX,
                member_index: None,
                parent: None,
                builtin: None,
            });
            out.push((g.name.name.clone(), id));
        }
        out
    }

    fn unsupported_impl_params(&mut self, span: Span) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNSUPPORTED_SYNTAX,
                "not yet supported: an `impl` block whose type arguments are not exactly its own generic parameters",
            )
            .with_label(span, "write the type's parameters in order: `impl<A, B> Pair<A, B>`")
            .with_note("blocks for one instance (`impl Pair<i32, bool>`) or for repeated parameters wait for traits (#116); see docs/PLAN.md"),
        );
    }

    /// Puts the type parameters of `adt` (a struct, an enum, a trait or a trait `impl`
    /// block) before those of its associated function or constant `def` (decision 85).
    fn inherit_generics_of(&mut self, def: DefId, adt: DefId) {
        if !matches!(self.results.def(def).kind, DefKind::Fn | DefKind::Const) {
            return;
        }
        let parent = self.results.generics(adt).to_vec();
        if parent.is_empty() {
            return;
        }
        let own = self.results.generics.remove(&def).unwrap_or_default();
        self.results.parent_generics.insert(def, parent.len());
        let mut all = parent;
        all.extend(own);
        self.results.generics.insert(def, all);
    }

    /// The struct an `impl` block is for, reporting anything else.
    fn resolve_impl_target(&mut self, ty: &Type) -> Option<DefId> {
        let TypeKind::Path(path) = &ty.kind else {
            self.resolve_type(ty, true);
            self.invalid_impl_target(ty.span);
            return None;
        };
        let res = self.resolve_type_path(path, true);
        self.results.res.insert(ty.id, res);
        match res {
            Res::Def(def) => Some(def),
            Res::Prim(_) => {
                self.invalid_impl_target(ty.span);
                None
            }
            Res::Local(_) | Res::Err => None,
        }
    }

    fn invalid_impl_target(&mut self, span: Span) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::INVALID_IMPL_TARGET,
                "cannot define inherent `impl` for primitive types",
            )
            .with_label(span, "not a struct of this file")
            .with_note(
                "an inherent `impl` block is only allowed for a struct defined in the same file; implement a trait for other types: `impl Show for i32`",
            ),
        );
    }

    fn resolve_item(&mut self, item: &Item, index: usize) {
        if let ItemKind::Use(_) = &item.kind {
            // Resolved with the items (`collect_uses`).
            return;
        }
        if let ItemKind::Trait(t) = &item.kind {
            let tr = self.results.def_of_item(item.id);
            self.impl_scope = ImplScope::Trait(tr);
            for member in &t.items {
                let def = self.results.def_of_item(member.id);
                match &member.kind {
                    ItemKind::Fn(f) => self.resolve_fn(def, f),
                    ItemKind::Const(c) => self.resolve_const_item(def, c),
                    _ => {}
                }
            }
            self.impl_scope = ImplScope::Outside;
            return;
        }
        if let ItemKind::Impl(imp) = &item.kind {
            let target = match imp.self_ty.kind {
                TypeKind::Path(_) => match self.results.res(imp.self_ty.id) {
                    Res::Def(def) => Some(def),
                    _ => None,
                },
                _ => None,
            };
            self.impl_scope = match self.results.item_defs.get(&item.id) {
                Some(&imp_def) => ImplScope::TraitImpl(imp_def),
                None => target.map_or(ImplScope::Unresolved, ImplScope::Struct),
            };
            for member in &imp.items {
                let def = self.results.def_of_item(member.id);
                match &member.kind {
                    ItemKind::Fn(f) => self.resolve_fn(def, f),
                    ItemKind::Const(c) => {
                        self.ty_params = self.impl_params.get(&index).cloned().unwrap_or_default();
                        self.resolve_const_item(def, c);
                    }
                    _ => {}
                }
            }
            self.ty_params.clear();
            self.impl_scope = ImplScope::Outside;
            return;
        }
        let def = self.results.def_of_item(item.id);
        debug_assert_eq!(self.results.def(def).item_index, index);
        // A struct's or enum's fields see its type parameters.
        self.ty_params = self
            .results
            .generics(def)
            .iter()
            .map(|&p| (self.results.def(p).name.clone(), p))
            .collect();
        match &item.kind {
            ItemKind::Impl(_) | ItemKind::Trait(_) | ItemKind::Use(_) => {
                unreachable!("handled above")
            }
            ItemKind::Fn(f) => self.resolve_fn(def, f),
            ItemKind::Struct(s) => {
                if let StructKind::Named(fields) = &s.kind {
                    self.resolve_fields(fields);
                }
            }
            ItemKind::Enum(e) => {
                for v in &e.variants {
                    match &v.data {
                        VariantData::Unit => {}
                        VariantData::Tuple(tys) => {
                            for t in tys {
                                self.resolve_type(t, true);
                            }
                        }
                        VariantData::Named(fields) => self.resolve_fields(fields),
                    }
                    if let Some(d) = &v.discriminant {
                        self.current_fn = Some(def);
                        self.scopes.push(Vec::new());
                        self.resolve_expr(d);
                        self.scopes.pop();
                        self.current_fn = None;
                    }
                }
            }
            ItemKind::Const(c) => self.resolve_const_item(def, c),
        }
        self.ty_params.clear();
    }

    /// A `const` item, at the top level, in an `impl` block or in a trait.
    fn resolve_const_item(&mut self, def: DefId, c: &sabiko_ast::ConstDecl) {
        self.resolve_type(&c.ty, true);
        let Some(value) = &c.value else {
            return;
        };
        self.current_fn = Some(def);
        self.scopes.push(Vec::new());
        self.resolve_expr(value);
        self.scopes.pop();
        self.current_fn = None;
    }

    /// The fields of a struct or a struct variant: unique names (E0204) and their types.
    fn resolve_fields(&mut self, fields: &[FieldDef]) {
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for field in fields {
            if let Some(&prev) = seen.get(field.name.name.as_str()) {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_FIELD_DEFINITION,
                        format!("field `{}` is already declared", field.name.name),
                    )
                    .with_label(field.name.span, "field already declared")
                    .with_label(prev, format!("`{}` first declared here", field.name.name)),
                );
            } else {
                seen.insert(&field.name.name, field.name.span);
            }
            self.resolve_type(&field.ty, true);
        }
    }

    fn resolve_fn(&mut self, def: DefId, f: &FnDecl) {
        self.current_fn = Some(def);
        // A function of an `impl` block also sees the block's names for its type's
        // parameters (decision 83).
        let info = self.results.def(def);
        let mut params = if info.member_index.is_some() {
            self.impl_params
                .get(&info.item_index)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        params.extend(
            self.results
                .own_generics(def)
                .iter()
                .map(|&p| (self.results.def(p).name.clone(), p)),
        );
        self.ty_params = params;
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
        if let Some(body) = &f.body {
            self.resolve_block_in_current_scope(body);
        }
        self.scopes.pop();
        self.current_fn = None;
        self.ty_params.clear();
    }

    /// Resolves `pat` and binds its names in the innermost scope, returning the bindings.
    fn bind_pat(&mut self, pat: &Pat, is_param: bool) -> Vec<Binding> {
        let mut out = Vec::new();
        self.walk_pat(pat, is_param, None, &mut out);
        let scope = self.scopes.last_mut().expect("a scope is open");
        for b in &out {
            scope.push((b.name.clone(), b.local));
        }
        out
    }

    /// Resolves the paths of `pat` and creates (or, inside an alternative of an
    /// or-pattern other than the first, reuses from `reuse`) the locals of its bindings,
    /// appending them to `out` in source order.
    #[allow(clippy::too_many_lines)]
    fn walk_pat(
        &mut self,
        pat: &Pat,
        is_param: bool,
        reuse: Option<&[Binding]>,
        out: &mut Vec<Binding>,
    ) {
        match &pat.kind {
            PatKind::Ident {
                by_ref,
                mutable,
                name,
                sub,
            } => {
                if !by_ref && !mutable && sub.is_none() {
                    if let Some(res) = self.ident_pat_path(&name.name) {
                        self.results.pat_res.insert(pat.id, res);
                        return;
                    }
                }
                let existing = reuse.and_then(|r| r.iter().find(|b| b.name == name.name));
                let local = if let Some(b) = existing {
                    if b.by_ref != *by_ref || b.mutable != *mutable {
                        self.sink.emit(
                            Diagnostic::error(
                                ErrorCode::OR_PATTERN_BINDING_MODE,
                                format!(
                                    "variable `{}` is bound inconsistently across `|` patterns",
                                    name.name
                                ),
                            )
                            .with_label(name.span, "bound in different ways")
                            .with_label(b.span, "first binding"),
                        );
                    }
                    b.local
                } else {
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
                    id
                };
                self.results.pat_locals.insert(pat.id, local);
                out.push(Binding {
                    name: name.name.clone(),
                    local,
                    span: name.span,
                    by_ref: *by_ref,
                    mutable: *mutable,
                });
                if let Some(sub) = sub {
                    self.walk_pat(sub, is_param, reuse, out);
                }
            }
            PatKind::Wild | PatKind::Rest => {}
            PatKind::Tuple(elems) => {
                for p in elems {
                    self.walk_pat(p, is_param, reuse, out);
                }
            }
            PatKind::Path(path) => {
                self.assoc_self = None;
                let res = self.resolve_value_path(path);
                self.results.pat_res.insert(pat.id, res);
                // `S::N` naming a trait's constant (#116).
                if let Some(ty_res) = self.assoc_self.take() {
                    self.results.assoc_self.insert(pat.id, ty_res);
                }
            }
            PatKind::TupleStruct(path, elems) => {
                let imported = path.is_single()
                    && self
                        .items
                        .get(&path.segments[0].name)
                        .is_some_and(|&d| self.results.def(d).kind == DefKind::Variant);
                let res = if path.is_single() && !imported {
                    self.unknown_tuple_variant(path)
                } else {
                    self.resolve_value_path(path)
                };
                self.results.pat_res.insert(pat.id, res);
                for p in elems {
                    self.walk_pat(p, is_param, reuse, out);
                }
            }
            PatKind::Struct { path, fields, .. } => {
                let res = self.resolve_struct_path(path);
                self.results.pat_res.insert(pat.id, res);
                for f in fields {
                    self.walk_pat(&f.pat, is_param, reuse, out);
                }
            }
            PatKind::Lit(e) => self.resolve_expr(e),
            PatKind::Range(lo, hi, _) => {
                for bound in [lo, hi].into_iter().flatten() {
                    self.resolve_expr(bound);
                }
            }
            PatKind::Ref { pat, .. } => self.walk_pat(pat, is_param, reuse, out),
            PatKind::Assignee(e) => self.walk_assignee(pat, e),
            PatKind::Or(alts) => {
                let mut first = Vec::new();
                self.walk_pat(&alts[0], is_param, reuse, &mut first);
                for alt in &alts[1..] {
                    let mut names = Vec::new();
                    self.walk_pat(alt, is_param, Some(&first), &mut names);
                    for b in &first {
                        if !names.iter().any(|n| n.name == b.name) {
                            self.missing_in_alternative(&b.name, alt.span, b.span);
                        }
                    }
                    for n in names {
                        if !first.iter().any(|b| b.name == n.name) {
                            self.missing_in_alternative(&n.name, alts[0].span, n.span);
                            first.push(n);
                        }
                    }
                }
                out.extend(first);
            }
        }
    }

    /// A place on the left of a destructuring assignment: it binds a fresh local, which no
    /// name reaches (rustc's `lhs`), unless it is a path naming a unit struct or a variant,
    /// which is a path pattern instead, as in rustc (a constant stays a place, E0070).
    fn walk_assignee(&mut self, pat: &Pat, e: &Expr) {
        let mut target = e;
        while let ExprKind::Paren(inner) = &target.kind {
            target = inner;
        }
        if let ExprKind::Path(path) = &target.kind {
            let res = self.resolve_value_path(path);
            self.results.res.insert(target.id, res);
            if let Res::Def(def) = res {
                let kind = self.results.def(def).kind;
                if kind == DefKind::Variant
                    || (kind == DefKind::Struct && self.unit_structs.contains(&def))
                {
                    self.results.pat_res.insert(pat.id, res);
                    return;
                }
            }
        } else {
            self.resolve_expr(e);
        }
        let id = LocalId(u32::try_from(self.results.locals.len()).expect("too many locals"));
        self.results.locals.push(LocalInfo {
            name: "lhs".to_owned(),
            mutable: false,
            span: e.span,
            pat: pat.id,
            owner: self.current_fn.expect("bindings only occur inside items"),
            is_param: false,
        });
        self.results.pat_locals.insert(pat.id, id);
    }

    /// E0218 (rustc's E0408): `name`, bound at `bound`, is missing from the alternative at
    /// `alt`.
    fn missing_in_alternative(&mut self, name: &str, alt: Span, bound: Span) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::OR_PATTERN_BINDING_MISSING,
                format!("variable `{name}` is not bound in all patterns"),
            )
            .with_label(alt, format!("pattern doesn't bind `{name}`"))
            .with_label(bound, "variable not in all patterns"),
        );
    }

    /// `V(..)` with a single-segment path: there are no tuple structs, and variants are
    /// named through their enum.
    fn unknown_tuple_variant(&mut self, path: &Path) -> Res {
        let name = &path.segments[0];
        let mut d = Diagnostic::error(
            ErrorCode::UNRESOLVED_VALUE,
            format!(
                "cannot find tuple struct or tuple variant `{}` in this scope",
                name.name
            ),
        )
        .with_label(name.span, "not found in this scope");
        let owners: Vec<String> = self
            .results
            .variants
            .iter()
            .filter(|(_, vs)| vs.iter().any(|&v| self.results.def(v).name == name.name))
            .map(|(e, _)| self.results.def(*e).name.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Some(owner) = owners.first() {
            d = d.with_note(format!(
                "`{}` is a variant of `{owner}`: write `{owner}::{}`",
                name.name, name.name
            ));
        }
        self.sink.emit(d);
        Res::Err
    }

    /// What a lone identifier in a pattern names when it does not bind: a constant or a
    /// unit struct of the file, unless a local of that name shadows it (rustc looks the
    /// name up the same way).
    fn ident_pat_path(&self, name: &str) -> Option<Res> {
        if self.lookup_local(name).is_some() {
            return None;
        }
        let &def = self.items.get(name)?;
        let info = self.results.def(def);
        match info.kind {
            // A variant is only in the namespace when a `use` imported it (the prelude's
            // `None`); one that is not a unit variant is then the type checker's E0326.
            DefKind::Const | DefKind::Variant => Some(Res::Def(def)),
            DefKind::Struct if self.unit_structs.contains(&def) => Some(Res::Def(def)),
            _ => None,
        }
    }

    /// Binds a whole pattern (of a `let`, a `match` arm, an `if let`): each name at most
    /// once per pattern (E0209, rustc's E0416), or-pattern alternatives aside.
    fn bind_let_pat(&mut self, pat: &Pat) {
        let bindings = self.bind_pat(pat, false);
        let mut seen: HashMap<&str, Span> = HashMap::new();
        for b in &bindings {
            if let Some(&prev) = seen.get(b.name.as_str()) {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::DUPLICATE_BINDING,
                        format!(
                            "identifier `{}` is bound more than once in the same pattern",
                            b.name
                        ),
                    )
                    .with_label(b.span, "used in a pattern more than once")
                    .with_label(prev, "first use of the name"),
                );
            } else {
                seen.insert(&b.name, b.span);
            }
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
                let res = self.resolve_type_path(path, in_signature);
                self.results.res.insert(ty.id, res);
            }
            TypeKind::Ref { inner, .. } | TypeKind::Slice(inner) => {
                self.resolve_type(inner, in_signature);
            }
            TypeKind::Array(elem, len) => {
                self.resolve_type(elem, in_signature);
                self.resolve_const_expr(len);
            }
            TypeKind::Tuple(elems) => {
                for t in elems {
                    self.resolve_type(t, in_signature);
                }
            }
            TypeKind::Unit | TypeKind::Never => {}
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

    /// `Self` as a type inside an `impl` block or a trait: an inherent block's struct, a
    /// trait `impl` block (its type), a trait's `Self` parameter, or `Res::Err` (silently)
    /// when the block's type did not resolve. Outside one it is an error, reported here.
    fn resolve_self_type(&mut self, span: Span) -> Res {
        match self.impl_scope {
            ImplScope::Struct(def) | ImplScope::TraitImpl(def) => Res::Def(def),
            ImplScope::Trait(tr) => Res::Def(self.results.trait_self(tr)),
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

    /// `Self` as a value, a struct literal's path or the type of `Self::Name`: the struct
    /// or enum a trait `impl` block is for (when its type is one), or what `Self` names as
    /// a type.
    fn resolve_self_adt(&mut self, span: Span) -> Res {
        let res = self.resolve_self_type(span);
        self.impl_adt(res)
    }

    /// For a trait `impl` block, the struct or enum its type names (or the block when it
    /// names none); anything else as it is.
    fn impl_adt(&self, res: Res) -> Res {
        match res {
            Res::Def(imp) if self.results.def(imp).kind == DefKind::Impl => self
                .impl_adts
                .get(&imp)
                .copied()
                .flatten()
                .map_or(Res::Def(imp), Res::Def),
            res => res,
        }
    }

    /// The types of the path's generic arguments.
    fn resolve_generic_args(&mut self, path: &Path, in_signature: bool) {
        for args in &path.generic_args {
            for ty in &args.args {
                self.resolve_type(ty, in_signature);
            }
        }
    }

    fn resolve_type_path(&mut self, path: &Path, in_signature: bool) -> Res {
        self.resolve_generic_args(path, in_signature);
        let res = self.resolve_type_path_inner(path);
        self.check_generic_args(path, res, true);
        res
    }

    /// Reports generic arguments the path's definitions do not take (rustc's E0109), and
    /// the wrong number of them (E0107): a generic function takes its own parameters (not
    /// its `impl` block's), a generic struct or enum, or a variant, its type's. In a type
    /// (`in_type`), a generic struct or enum needs all of them; in an expression or pattern
    /// none may be written, and inference finds them.
    fn check_generic_args(&mut self, path: &Path, res: Res, in_type: bool) {
        if matches!(res, Res::Err) {
            return;
        }
        let last = path.segments.len() - 1;
        for index in 0..path.segments.len() {
            let seg = &path.segments[index];
            let written = path.args_of(index);
            let def = if index == last {
                match res {
                    Res::Def(d) => Some(d),
                    _ => None,
                }
            } else if seg.name == "Self" {
                // `Self` already has its block's arguments.
                None
            } else {
                self.items
                    .get(&seg.name)
                    .copied()
                    .filter(|&d| self.results.def(d).kind.is_type())
            };
            let kind = def.map(|d| self.results.def(d).kind);
            // What the segment takes, and whether leaving them out is allowed.
            let takes = match (kind, def) {
                (Some(DefKind::Fn), Some(d)) => Some((self.results.own_generics(d).len(), true)),
                (Some(DefKind::Struct | DefKind::Enum), Some(d)) => {
                    let n = self.results.generics(d).len();
                    Some((n, !in_type || seg.name == "Self"))
                }
                (Some(DefKind::Variant), Some(d)) => {
                    let n = self
                        .results
                        .def(d)
                        .parent
                        .map_or(0, |e| self.results.generics(e).len());
                    Some((n, true))
                }
                _ => None,
            };
            let got = written.map(|a| a.args.len());
            match (takes, got) {
                (Some((want, may_omit)), None) => {
                    if want > 0 && !may_omit && index == last {
                        let info = self.results.def(def.expect("takes arguments"));
                        let what = info.kind.describe();
                        let plural = if want == 1 { "argument" } else { "arguments" };
                        self.sink.emit(
                            Diagnostic::error(
                                ErrorCode::WRONG_GENERIC_ARG_COUNT,
                                format!("missing generics for {what} `{}`", seg.name),
                            )
                            .with_label(seg.span, format!("expected {want} generic {plural}"))
                            .with_label(info.span, format!("{what} `{}` defined here", info.name)),
                        );
                    }
                }
                (Some((want, _)), Some(got)) if want == got => {}
                (Some((want, _)), Some(got)) if want > 0 => {
                    let args = written.expect("written");
                    let info = self.results.def(def.expect("takes arguments"));
                    let what = info.kind.describe();
                    let plural = |n: usize| if n == 1 { "argument" } else { "arguments" };
                    let mut d = Diagnostic::error(
                        ErrorCode::WRONG_GENERIC_ARG_COUNT,
                        format!(
                            "{what} takes {want} generic {} but {got} generic {} {} supplied",
                            plural(want),
                            plural(got),
                            if got == 1 { "was" } else { "were" }
                        ),
                    )
                    .with_label(
                        args.span,
                        format!("expected {want} generic {}", plural(want)),
                    );
                    if !info.span.is_dummy() {
                        d = d.with_label(info.span, format!("{what} `{}` defined here", info.name));
                    }
                    self.sink.emit(d);
                }
                (_, None) => {}
                (_, Some(_)) => {
                    let args = written.expect("written");
                    self.args_not_allowed(seg, args, kind, res, index == last);
                }
            }
        }
    }

    /// E0223: generic arguments `args` after the segment `seg`, which takes none (`kind`
    /// is its definition's, `res` the path's resolution when `seg` is its last segment).
    fn args_not_allowed(
        &mut self,
        seg: &sabiko_ast::Ident,
        args: &sabiko_ast::GenericArgs,
        kind: Option<DefKind>,
        res: Res,
        is_last: bool,
    ) {
        let what = match (kind, res) {
            (Some(kind), _) => format!("{} `{}`", kind.describe(), seg.name),
            (None, Res::Prim(_)) if is_last => format!("builtin type `{}`", seg.name),
            (None, Res::Local(_)) if is_last => format!("local variable `{}`", seg.name),
            (None, _) => format!("`{}`", seg.name),
        };
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::GENERIC_ARGS_NOT_ALLOWED,
                format!("type arguments are not allowed on {what}"),
            )
            .with_label(args.span, "type argument not allowed")
            .with_note("only generic functions, structs and enums take generic arguments"),
        );
    }

    fn resolve_type_path_inner(&mut self, path: &Path) -> Res {
        if !path.is_single() {
            return self.unsupported_path(path);
        }
        let name = &path.segments[0];
        if name.name == "Self" {
            return self.resolve_self_type(name.span);
        }
        if let Some(&(_, param)) = self.ty_params.iter().find(|(n, _)| *n == name.name) {
            return Res::Def(param);
        }
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                DefKind::Struct | DefKind::Enum => Res::Def(def),
                kind => {
                    let mut d = Diagnostic::error(
                        ErrorCode::WRONG_NAMESPACE,
                        format!("expected type, found {} `{}`", kind.describe(), name.name),
                    )
                    .with_label(name.span, "not a type");
                    if kind == DefKind::Trait {
                        d = d.with_note(format!(
                            "trait objects (`dyn {}`) are planned for Phase 7; a type parameter takes any type that implements it: `fn F<T: {}>(x: T)`",
                            name.name, name.name
                        ));
                    }
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
        self.resolve_generic_args(path, false);
        let res = self.resolve_value_path_inner(path);
        self.check_generic_args(path, res, false);
        res
    }

    /// Reports a type parameter named as a value (rustc's E0423); `false` when `name` is
    /// no type parameter in scope.
    fn type_param_as_value(&mut self, name: &sabiko_ast::Ident) -> bool {
        let Some(&(_, param)) = self.ty_params.iter().find(|(n, _)| *n == name.name) else {
            return false;
        };
        let d = Diagnostic::error(
            ErrorCode::WRONG_NAMESPACE,
            format!("expected value, found type parameter `{}`", name.name),
        )
        .with_label(name.span, "not a value")
        .with_label(self.results.def(param).span, "type parameter defined here");
        self.sink.emit(d);
        true
    }

    fn resolve_value_path_inner(&mut self, path: &Path) -> Res {
        match path.segments.len() {
            1 => {}
            2 => return self.resolve_assoc_path(path),
            _ => return self.unsupported_path(path),
        }
        let name = &path.segments[0];
        if let Some(local) = self.lookup_local(&name.name) {
            let outside = self.const_floor.is_some_and(|floor| {
                !self.scopes[floor..]
                    .iter()
                    .any(|scope| scope.iter().any(|(_, id)| *id == local))
            });
            if outside {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::NON_CONSTANT_VALUE,
                        "attempt to use a non-constant value in a constant",
                    )
                    .with_label(name.span, "non-constant value")
                    .with_note("an array length is a constant: use a `const` item"),
                );
                return Res::Err;
            }
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
            return self.resolve_self_adt(name.span);
        }
        if self.type_param_as_value(name) {
            return Res::Err;
        }
        if let Some(&def) = self.items.get(&name.name) {
            let info = self.results.def(def);
            return match info.kind {
                // Unit structs are values; the type checker rejects named structs used
                // as values, since it knows the struct's shape. A variant is only in the
                // namespace when a `use` imported it (the prelude's `Some`).
                DefKind::Fn | DefKind::Const | DefKind::Struct | DefKind::Variant => Res::Def(def),
                kind @ (DefKind::Enum | DefKind::Trait) => {
                    let mut d = Diagnostic::error(
                        ErrorCode::WRONG_NAMESPACE,
                        format!("expected value, found {} `{}`", kind.describe(), name.name),
                    )
                    .with_label(name.span, "not a value");
                    if kind != DefKind::Trait {
                        d = d.with_note(format!(
                            "a variant is named through its enum: `{}::Variant`",
                            name.name
                        ));
                    }
                    self.sink
                        .emit(Self::with_definition_label(d, info, &name.name));
                    Res::Err
                }
                DefKind::TyParam | DefKind::Impl => {
                    unreachable!("type parameters and `impl` blocks are not items")
                }
            };
        }
        self.unresolved_value(name);
        Res::Err
    }

    /// `Type::Name`: a variant, or an associated function or constant of a struct or enum
    /// (`Self` inside an `impl` block).
    /// The first segment is looked up as a type: a local, a function or a constant of
    /// that name does not count, as in rustc.
    fn resolve_assoc_path(&mut self, path: &Path) -> Res {
        let (ty, item) = (&path.segments[0], &path.segments[1]);
        self.assoc_self = None;
        if matches!(ty.name.as_str(), "self" | "crate" | "super") {
            return self.unsupported_path(path);
        }
        if let Some(res) = self.resolve_trait_member_path(ty, item) {
            return res;
        }
        let known = ty.name == "Self"
            || self.items.contains_key(&ty.name)
            || PrimTy::from_name(&ty.name).is_some()
            || self.ty_params.iter().any(|(n, _)| *n == ty.name);
        if !known {
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::UNRESOLVED_TYPE,
                    format!("cannot find type `{}` in this scope", ty.name),
                )
                .with_label(ty.span, "not found in this scope")
                .with_note("`A::B` names the associated item `B` of the type `A`; module paths are planned for Phase 9"),
            );
            return Res::Err;
        }
        // The type segment's own generic arguments are checked with the whole path. What
        // it names as a type is `Self` for a trait's member; its struct or enum (for
        // `Self` in a trait `impl` block, the one its type names) has the variants and
        // inherent items.
        let (ty_res, owner) = if ty.name == "Self" {
            let res = self.resolve_self_type(ty.span);
            (res, self.impl_adt(res))
        } else {
            let res = self.resolve_type_path_inner(&Path::single(ty.clone()));
            (res, res)
        };
        let trait_member = self.results.trait_items_named(&item.name).first().copied();
        let from_trait = |r: &mut Self| {
            r.assoc_self = Some(ty_res);
            trait_member.map(Res::Def)
        };
        match owner {
            Res::Def(def) if self.results.def(def).kind.is_type() => {
                if let Some(v) = self.variant(def, &item.name) {
                    return Res::Def(v);
                }
                if let Some(d) = self.results.assoc_item(def, &item.name) {
                    return Res::Def(d);
                }
                if let Some(res) = from_trait(self) {
                    return res;
                }
                let info = self.results.def(def);
                let what = format!("{} `{}`", info.kind.describe(), info.name);
                if info.kind == DefKind::Enum {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::UNKNOWN_ASSOC_ITEM,
                            format!(
                                "no variant or associated item named `{}` found for {what}",
                                item.name
                            ),
                        )
                        .with_label(
                            item.span,
                            format!("variant or associated item not found in `{}`", info.name),
                        ),
                    );
                } else {
                    self.unknown_assoc_item(item, &what);
                }
                Res::Err
            }
            Res::Prim(prim) => {
                if PrimConst::lookup(prim, &item.name).is_none() {
                    if let Some(res) = from_trait(self) {
                        return res;
                    }
                }
                self.resolve_prim_const(prim, item)
            }
            Res::Def(def) => {
                // A type parameter, a trait's `Self`, or the type of a trait `impl` block
                // that is not a struct or an enum: only traits have its members.
                if let Some(res) = from_trait(self) {
                    return res;
                }
                let what = match self.results.def(def).kind {
                    DefKind::TyParam => format!("type parameter `{}`", ty.name),
                    _ => format!("type `{}`", ty.name),
                };
                self.unknown_assoc_item(item, &what);
                Res::Err
            }
            Res::Local(_) | Res::Err => Res::Err,
        }
    }

    /// E0202: `name` names no value, with a note on an associated item of that name.
    fn unresolved_value(&mut self, name: &sabiko_ast::Ident) {
        let mut d = Diagnostic::error(
            ErrorCode::UNRESOLVED_VALUE,
            format!("cannot find value `{}` in this scope", name.name),
        )
        .with_label(name.span, "not found in this scope");
        if PrimTy::from_name(&name.name).is_some() {
            d = d.with_note(format!("`{}` is a type, not a value", name.name));
        }
        let owners: Vec<(String, bool)> = self
            .results
            .assoc_items
            .iter()
            .filter(|((_, n), _)| *n == name.name)
            .map(|((adt, _), def)| {
                let is_const = self.results.def(*def).kind == DefKind::Const;
                (self.results.def(*adt).name.clone(), is_const)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Some((owner, is_const)) = owners.first() {
            let n = &name.name;
            d = d.with_note(if *is_const {
                format!("`{n}` is an associated constant of `{owner}`: use it as `{owner}::{n}`")
            } else {
                format!(
                    "`{n}` is an associated function of `{owner}`: call it as `{owner}::{n}(..)`"
                )
            });
        }
        self.sink.emit(d);
    }

    /// `Trait::Name`: the trait's member, at a `Self` inference finds; `None` when `ty`
    /// names no trait.
    fn resolve_trait_member_path(
        &mut self,
        ty: &sabiko_ast::Ident,
        item: &sabiko_ast::Ident,
    ) -> Option<Res> {
        let tr = *self.items.get(&ty.name)?;
        if self.results.def(tr).kind != DefKind::Trait {
            return None;
        }
        if let Some(def) = self.results.trait_item(tr, &item.name) {
            self.assoc_self = Some(Res::Def(tr));
            return Some(Res::Def(def));
        }
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNKNOWN_ASSOC_ITEM,
                format!(
                    "cannot find method or associated constant `{}` in trait `{}`",
                    item.name, ty.name
                ),
            )
            .with_label(item.span, format!("not found in `{}`", ty.name)),
        );
        Some(Res::Err)
    }

    /// `i32::MAX`: a primitive associated constant, with a definition of its own (one per
    /// constant, however often it is used).
    fn resolve_prim_const(&mut self, prim: PrimTy, item: &sabiko_ast::Ident) -> Res {
        let Some(c) = PrimConst::lookup(prim, &item.name) else {
            self.unknown_assoc_item(item, &format!("type `{}`", prim.name()));
            return Res::Err;
        };
        if c.ty == PrimTy::Char && c.name == "MAX" {
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::NON_BMP_CHAR,
                    "`char::MAX` is outside the Basic Multilingual Plane",
                )
                .with_label(item.span, "`'\\u{10FFFF}'` does not fit a Sabiko `char`")
                .with_note("a `char` is one UTF-16 code unit, at most `'\\u{FFFF}'` (decision 31)"),
            );
            return Res::Err;
        }
        if let Some((&def, _)) = self.results.prim_consts.iter().find(|(_, k)| **k == c) {
            return Res::Def(def);
        }
        let def = self.new_def(DefInfo {
            kind: DefKind::Const,
            name: format!("{}::{}", prim.name(), c.name),
            span: Span::dummy(),
            node: NodeId::DUMMY,
            item_index: usize::MAX,
            member_index: None,
            parent: None,
            builtin: None,
        });
        self.results.prim_consts.insert(def, c);
        Res::Def(def)
    }

    fn unknown_assoc_item(&mut self, item: &sabiko_ast::Ident, owner: &str) {
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::UNKNOWN_ASSOC_ITEM,
                format!(
                    "no associated function or constant named `{}` found for {owner}",
                    item.name
                ),
            )
            .with_label(
                item.span,
                format!("associated function or constant not found in {owner}"),
            ),
        );
    }

    /// The path of a struct literal or pattern: a struct, or `E::V` naming a variant (any
    /// kind: the type checker reports a variant of the wrong shape).
    fn resolve_struct_path(&mut self, path: &Path) -> Res {
        self.resolve_generic_args(path, false);
        let res = self.resolve_struct_path_inner(path);
        self.check_generic_args(path, res, false);
        res
    }

    fn resolve_struct_path_inner(&mut self, path: &Path) -> Res {
        match path.segments.len() {
            1 => {}
            2 => {
                let res = self.resolve_assoc_path(path);
                return match res {
                    Res::Def(def)
                        if matches!(self.results.def(def).kind, DefKind::Fn | DefKind::Const) =>
                    {
                        let name = path.last();
                        let what = match self.results.def(def).kind {
                            DefKind::Fn => "associated function",
                            _ => "associated constant",
                        };
                        self.sink.emit(
                            Diagnostic::error(
                                ErrorCode::WRONG_NAMESPACE,
                                format!(
                                    "expected struct or variant, found {what} `{}`",
                                    self.results.qualified_name(def)
                                ),
                            )
                            .with_label(name.span, "not a struct or a variant"),
                        );
                        Res::Err
                    }
                    res => res,
                };
            }
            _ => return self.unsupported_path(path),
        }
        let name = &path.segments[0];
        if name.name == "Self" {
            return match self.resolve_self_adt(name.span) {
                Res::Def(def)
                    if matches!(self.results.def(def).kind, DefKind::TyParam | DefKind::Impl) =>
                {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::WRONG_NAMESPACE,
                            "expected struct, found `Self`",
                        )
                        .with_label(name.span, "`Self` is not a struct here"),
                    );
                    Res::Err
                }
                Res::Def(def) if self.results.def(def).kind == DefKind::Enum => {
                    self.sink.emit(
                        Diagnostic::error(
                            ErrorCode::WRONG_NAMESPACE,
                            "expected struct, found enum `Self`",
                        )
                        .with_label(
                            name.span,
                            "`Self` is an enum here: name a variant, `Self::V`",
                        ),
                    );
                    Res::Err
                }
                res => res,
            };
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

    /// The condition of an `if` or `while`, in a scope the caller opened: a `let` binds its
    /// pattern there, after its scrutinee is resolved. In a let chain, the operands after a
    /// `let` see its bindings.
    fn resolve_cond(&mut self, cond: &Expr) {
        if let Some(operands) = cond.let_chain() {
            for operand in operands {
                self.resolve_cond(operand);
            }
        } else if let ExprKind::Let(pat, scrutinee) = &cond.kind {
            self.resolve_expr(scrutinee);
            self.bind_let_pat(pat);
        } else {
            self.resolve_expr(cond);
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
                if let Some(els) = &l.els {
                    // The `else` block does not see the pattern's bindings.
                    self.resolve_block(els);
                }
                self.bind_let_pat(&l.pat);
            }
            StmtKind::Expr { expr, .. } => self.resolve_expr(expr),
            StmtKind::Empty => {}
        }
    }

    /// An array length or repeat count: locals of the enclosing body are out of reach.
    fn resolve_const_expr(&mut self, expr: &Expr) {
        let saved = self.const_floor.replace(self.scopes.len());
        self.resolve_expr(expr);
        self.const_floor = saved;
    }

    #[allow(clippy::too_many_lines)]
    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Lit(_) | ExprKind::Unit | ExprKind::Continue => {}
            ExprKind::Repeat(value, count) => {
                self.resolve_expr(value);
                self.resolve_const_expr(count);
            }
            ExprKind::Tuple(elems) | ExprKind::Array(elems) => {
                for e in elems {
                    self.resolve_expr(e);
                }
            }
            ExprKind::Path(path) => {
                self.assoc_self = None;
                let res = self.resolve_value_path(path);
                self.results.res.insert(expr.id, res);
                if let Some(ty_res) = self.assoc_self.take() {
                    self.results.assoc_self.insert(expr.id, ty_res);
                }
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
            ExprKind::Range(lo, hi, _) => {
                for bound in [lo, hi].into_iter().flatten() {
                    self.resolve_expr(bound);
                }
            }
            ExprKind::Call { callee, args } => {
                self.resolve_expr(callee);
                for a in args {
                    self.resolve_expr(a);
                }
            }
            ExprKind::MethodCall {
                receiver,
                generic_args,
                args,
                ..
            } => {
                self.resolve_expr(receiver);
                for ty in generic_args.iter().flat_map(|g| &g.args) {
                    self.resolve_type(ty, false);
                }
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
                // `if let`: the bindings are visible in the `then` block only.
                self.scopes.push(Vec::new());
                self.resolve_cond(cond);
                self.resolve_block(then);
                self.scopes.pop();
                if let Some(e) = els {
                    self.resolve_expr(e);
                }
            }
            ExprKind::While { cond, body } => {
                self.scopes.push(Vec::new());
                self.resolve_cond(cond);
                self.resolve_block(body);
                self.scopes.pop();
            }
            ExprKind::Let(pat, scrutinee) => {
                // Only in a condition, which `resolve_cond` handles; the parser never
                // produces one elsewhere.
                self.resolve_expr(scrutinee);
                self.scopes.push(Vec::new());
                self.bind_let_pat(pat);
                self.scopes.pop();
            }
            ExprKind::Match { scrutinee, arms } => {
                self.resolve_expr(scrutinee);
                for arm in arms {
                    self.scopes.push(Vec::new());
                    self.bind_let_pat(&arm.pat);
                    if let Some(g) = &arm.guard {
                        // A `let` in the guard binds for the rest of it and the arm (#98).
                        self.resolve_cond(g);
                    }
                    self.resolve_expr(&arm.body);
                    self.scopes.pop();
                }
            }
            ExprKind::Loop { body } => self.resolve_block(body),
            ExprKind::Break(e) | ExprKind::Return(e) => {
                if let Some(e) = e {
                    self.resolve_expr(e);
                }
            }
            ExprKind::DestructuringAssign(pat, rhs) => {
                // rustc's `{ let pat = rhs; a = lhs; .. }`: the fresh locals no name reaches.
                self.resolve_expr(rhs);
                self.bind_let_pat(pat);
            }
            ExprKind::Underscore => unreachable!("the parser rejects `_` outside an assignee"),
            ExprKind::Struct { path, fields, .. } => {
                let res = self.resolve_struct_path(path);
                self.results.res.insert(expr.id, res);
                for f in fields {
                    self.resolve_expr(&f.value);
                }
            }
        }
    }
}

/// The identifiers a pattern may bind, left to right (the first alternative of an
/// or-pattern only).
fn pat_bindings<'p>(pat: &'p Pat, out: &mut Vec<&'p sabiko_ast::Ident>) {
    pat.walk_idents(&mut |p| {
        if let PatKind::Ident { name, .. } = &p.kind {
            out.push(name);
        }
    });
}

#[cfg(test)]
mod tests {
    use sabiko_diagnostics::DiagnosticSink;
    use sabiko_span::FileId;

    use super::*;

    fn run(src: &str) -> (SourceFile, ResolveResults, Vec<String>) {
        let mut sink = DiagnosticSink::new();
        let mut ast = sabiko_parser::parse_file(FileId(0), src, &mut sink);
        assert!(!sink.has_errors(), "parse errors: {:?}", sink.diagnostics());
        let results = resolve(&mut ast, &mut sink);
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
            kind: ExprKind::Block(f.body.clone().expect("a body")),
            span: f.body.as_ref().expect("a body").span,
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
        // User items first, then the builtins the user did not define, then the language
        // traits with their `Self` parameters and methods (decision 86).
        let names: Vec<&str> = r.defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PrintLine",
                "F",
                "G",
                "Print",
                "Panic",
                "Clone",
                "Self",
                "clone",
                "Copy",
                "Self",
                "Drop",
                "Self",
                "drop"
            ]
        );
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
        assert_eq!(
            r.defs_of_kind(DefKind::Fn).count(),
            7,
            "with `clone` and `drop`"
        );
    }

    #[test]
    fn user_struct_shadows_primitive_and_wrong_namespace() {
        let (_, r, msgs) = run("struct i32;\nfn F(x: i32) {}\nconst K: i32 = 1;\nfn G() -> F {}");
        assert_eq!(msgs, ["E0203 expected type, found function `F`"]);
        assert_eq!(r.defs_of_kind(DefKind::Struct).count(), 1);
    }
}
