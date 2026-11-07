//! Abstract syntax tree for Sabiko.
//!
//! The tree is a faithful record of the source after parsing: no desugaring, no name
//! resolution. Every node that later passes attach information to (items, expressions,
//! patterns, types, blocks, statements) carries a [`NodeId`] that is unique within a
//! parsed file, so side tables can be keyed on it without storing pointers.
//!
//! Two printers live here: [`dump`] renders the tree structurally for snapshot tests and
//! `--emit=ast`; [`pretty`] renders it back to source form for round-trip tests.

pub mod dump;
pub mod pretty;

pub use sabiko_lexer::literal::{FloatSuffix, IntSuffix};
use sabiko_span::Span;

/// Identifies an AST node within one parsed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

impl NodeId {
    pub const DUMMY: NodeId = NodeId(u32::MAX);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// A `::`-separated path such as `Point` or `Geometry::Scale`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub segments: Vec<Ident>,
    pub span: Span,
}

impl Path {
    /// The last segment, which names the item.
    #[must_use]
    pub fn last(&self) -> &Ident {
        self.segments
            .last()
            .expect("paths have at least one segment")
    }

    #[must_use]
    pub fn is_single(&self) -> bool {
        self.segments.len() == 1
    }
}

/// `#[path(tokens)]`. The token text is kept verbatim; attributes are interpreted by
/// later phases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub path: Path,
    /// Everything after the path inside the brackets, trimmed, e.g. `(name = "x")`.
    pub args: String,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Public,
}

/// A parsed source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    /// `//!` doc lines at the top of the file.
    pub docs: Vec<String>,
    pub items: Vec<Item>,
    pub span: Span,
}

impl SourceFile {
    /// Every item in source order, the members of an `impl` block right after the block
    /// itself.
    pub fn all_items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().flat_map(|item| {
            let members: &[Item] = match &item.kind {
                ItemKind::Impl(imp) => &imp.items,
                _ => &[],
            };
            std::iter::once(item).chain(members)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: NodeId,
    pub attrs: Vec<Attribute>,
    /// `///` doc lines, with the `///` stripped.
    pub docs: Vec<String>,
    pub vis: Visibility,
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Fn(FnDecl),
    Struct(StructDecl),
    Const(ConstDecl),
    /// `impl Type { fn ... }`: an inherent impl block; `impl Drop for Type { .. }`.
    Impl(ImplDecl),
    /// `enum Name { A, B(T), C { F: T } }`
    Enum(EnumDecl),
}

impl Item {
    /// The item's name; `None` for an `impl` block.
    #[must_use]
    pub fn name(&self) -> Option<&Ident> {
        match &self.kind {
            ItemKind::Fn(f) => Some(&f.name),
            ItemKind::Struct(s) => Some(&s.name),
            ItemKind::Const(c) => Some(&c.name),
            ItemKind::Enum(e) => Some(&e.name),
            ItemKind::Impl(_) => None,
        }
    }
}

/// `impl Type { ... }`, or `impl Drop for Type { ... }`. Its members are items of their
/// own (each with a [`NodeId`]); only functions are allowed, with or without a `self`
/// parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplDecl {
    /// The trait of a trait impl: only `Drop` is accepted before Phase 6 (decision 46), as
    /// a language item. `None` for an inherent impl.
    pub trait_ref: Option<Ident>,
    /// The implemented type as written: a path naming a struct (or `Self`) in a valid
    /// program.
    pub self_ty: Type,
    /// The associated functions, each an [`ItemKind::Fn`] item.
    pub items: Vec<Item>,
}

/// `self`, `mut self`, `&self` or `&mut self`, the first parameter of a method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfParam {
    /// The node the resolver records the `self` binding under.
    pub id: NodeId,
    pub kind: SelfKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SelfKind {
    /// `self` or `mut self`: the receiver by value, the binding mutable with `mut`.
    Value { mutable: bool },
    /// `&self` or `&mut self`.
    Ref { mutable: bool },
}

impl SelfKind {
    /// The parameter as written: `self`, `mut self`, `&self`, `&mut self`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Value { mutable: false } => "self",
            Self::Value { mutable: true } => "mut self",
            Self::Ref { mutable: false } => "&self",
            Self::Ref { mutable: true } => "&mut self",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnDecl {
    pub name: Ident,
    /// The receiver of a method; only inside an `impl` block.
    pub self_param: Option<SelfParam>,
    /// The parameters after the receiver.
    pub params: Vec<Param>,
    /// `None` means the function returns `()`.
    pub ret: Option<Type>,
    pub body: Block,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub id: NodeId,
    pub pat: Pat,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDecl {
    pub name: Ident,
    pub kind: StructKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructKind {
    /// `struct Name { field: Type, ... }`
    Named(Vec<FieldDef>),
    /// `struct Name;`
    Unit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    pub id: NodeId,
    pub docs: Vec<String>,
    pub vis: Visibility,
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

/// `enum Name { .. }`: non-generic, with unit, tuple and struct variants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDecl {
    pub name: Ident,
    pub variants: Vec<Variant>,
}

/// One variant of an enum: `A`, `A = 3`, `B(i32, bool)` or `C { X: i32 }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub id: NodeId,
    pub attrs: Vec<Attribute>,
    pub docs: Vec<String>,
    pub name: Ident,
    pub data: VariantData,
    /// `= expr`, an explicit discriminant.
    pub discriminant: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VariantData {
    /// `A`
    Unit,
    /// `B(T, U)`: the field types, named `0`, `1`, ...
    Tuple(Vec<Type>),
    /// `C { X: T }`
    Named(Vec<FieldDef>),
}

impl VariantData {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Unit => 0,
            Self::Tuple(tys) => tys.len(),
            Self::Named(fields) => fields.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Type,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    pub id: NodeId,
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    /// `i32`, `Point`, `Geometry::Vector`. Primitive types are paths until resolution.
    Path(Path),
    /// `&T` or `&mut T`.
    Ref { mutable: bool, inner: Box<Type> },
    /// `()`
    Unit,
    /// `(A,)`, `(A, B)`, ...: at least one element (`()` is [`TypeKind::Unit`]). A
    /// parenthesised type without a comma, `(A)`, is just `A`.
    Tuple(Vec<Type>),
    /// `[T; N]`: the length is a constant expression of type `usize`.
    Array(Box<Type>, Box<Expr>),
    /// `[T]`: unsized, only valid behind a reference (`&[T]`, `&mut [T]`).
    Slice(Box<Type>),
    /// `_` in a type position: ask inference.
    Infer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pat {
    pub id: NodeId,
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatKind {
    /// `x`, `mut x`, `ref x`, `ref mut x`, each optionally `@ subpattern`. A lone
    /// identifier that names a constant or a unit struct is a path pattern, which the
    /// resolver decides (it binds nothing then).
    Ident {
        by_ref: bool,
        mutable: bool,
        name: Ident,
        sub: Option<Box<Pat>>,
    },
    /// `_`
    Wild,
    /// `..` inside a tuple or tuple-variant pattern: any number of elements.
    Rest,
    /// `(a, (b, _))`, `(a,)`, `()`, `(a, ..)`. `(p)` without a comma is just `p`.
    Tuple(Vec<Pat>),
    /// `E::V`, `Self::V`: a unit variant (or another path with `::`).
    Path(Path),
    /// `E::V(a, ..)`
    TupleStruct(Path, Vec<Pat>),
    /// `E::V { X, Y: p, .. }`, `S { X, .. }`; `rest` is the span of a trailing `..`.
    Struct {
        path: Path,
        fields: Vec<FieldPat>,
        rest: Option<Span>,
    },
    /// A literal, possibly negated: the expression is an `ExprKind::Lit` or a
    /// `ExprKind::Unary(Neg, Lit)`.
    Lit(Box<Expr>),
    /// `a..=b` or `a..b`; each bound is a (possibly negated) literal or a path.
    Range(Box<Expr>, Box<Expr>, RangeEnd),
    /// `p | q | ..`, at least two alternatives.
    Or(Vec<Pat>),
    /// A place on the left of a destructuring assignment (`a`, `s.F`, `*r`, `v[i]` in
    /// `(a, s.F) = e`; only in [`ExprKind::DestructuringAssign`]): it binds a fresh local,
    /// which is then assigned to the place. A path that names a unit struct or a unit
    /// variant is a path pattern instead, which the resolver decides, as in rustc.
    Assignee(Box<Expr>),
}

/// `Name: pat` or the shorthand `Name` (also `ref Name`, `mut Name`) in a struct pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldPat {
    pub name: Ident,
    pub pat: Pat,
    /// Written as `Name` (the pattern is a binding of that name).
    pub shorthand: bool,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RangeEnd {
    /// `..=`
    Included,
    /// `..`
    Excluded,
}

impl RangeEnd {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Included => "..=",
            Self::Excluded => "..",
        }
    }
}

impl Pat {
    /// Calls `f` for every identifier pattern in `self` that may introduce a binding, in
    /// source order (the first alternative of an or-pattern only), and for every
    /// [`PatKind::Assignee`], which binds a fresh local unless it names a unit struct.
    pub fn walk_idents<'a>(&'a self, f: &mut impl FnMut(&'a Pat)) {
        match &self.kind {
            PatKind::Ident { sub, .. } => {
                f(self);
                if let Some(sub) = sub {
                    sub.walk_idents(f);
                }
            }
            PatKind::Assignee(_) => f(self),
            PatKind::Tuple(elems) | PatKind::TupleStruct(_, elems) => {
                for p in elems {
                    p.walk_idents(f);
                }
            }
            PatKind::Struct { fields, .. } => {
                for fp in fields {
                    fp.pat.walk_idents(f);
                }
            }
            PatKind::Or(alts) => {
                if let Some(first) = alts.first() {
                    first.walk_idents(f);
                }
            }
            PatKind::Wild
            | PatKind::Rest
            | PatKind::Path(_)
            | PatKind::Lit(_)
            | PatKind::Range(..) => {}
        }
    }

    /// Calls `f` for every [`PatKind::Assignee`] in `self`, in source order: the places a
    /// destructuring assignment assigns, in the order it assigns them.
    pub fn walk_assignees<'a>(&'a self, f: &mut impl FnMut(&'a Pat, &'a Expr)) {
        match &self.kind {
            PatKind::Assignee(e) => f(self, e),
            PatKind::Tuple(elems) | PatKind::TupleStruct(_, elems) => {
                for p in elems {
                    p.walk_assignees(f);
                }
            }
            PatKind::Struct { fields, .. } => {
                for fp in fields {
                    fp.pat.walk_assignees(f);
                }
            }
            PatKind::Ident { .. }
            | PatKind::Or(_)
            | PatKind::Wild
            | PatKind::Rest
            | PatKind::Path(_)
            | PatKind::Lit(_)
            | PatKind::Range(..) => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub id: NodeId,
    pub stmts: Vec<Stmt>,
    /// Trailing expression without `;`, the block's value.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stmt {
    pub id: NodeId,
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StmtKind {
    Let(LetStmt),
    /// An expression statement; `semi` records whether it ended with `;`. Block-like
    /// expressions (`if`, `while`, `loop`, `{}`) may omit it.
    Expr {
        expr: Expr,
        semi: bool,
    },
    /// A lone `;`.
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LetStmt {
    pub pat: Pat,
    pub ty: Option<Type>,
    pub init: Option<Expr>,
    /// `let p = e else { .. };`: the block that runs when `p` does not match; it must
    /// diverge.
    pub els: Option<Box<Block>>,
}

/// `pat if guard => body` in a `match`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arm {
    pub id: NodeId,
    pub pat: Pat,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    pub id: NodeId,
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprKind {
    Lit(Lit),
    /// A variable or item reference.
    Path(Path),
    /// `()`
    Unit,
    /// `(a,)`, `(a, b)`, ...: at least one element (`()` is [`ExprKind::Unit`]).
    Tuple(Vec<Expr>),
    /// `[a, b, c]`, `[]`
    Array(Vec<Expr>),
    /// `[value; count]`: `count` is a constant expression of type `usize`.
    Repeat(Box<Expr>, Box<Expr>),
    Paren(Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `lhs = rhs`
    Assign(Box<Expr>, Box<Expr>),
    /// `(a, b) = rhs`, `S { X: a, .. } = rhs`, `E::V(a, _) = rhs`, `_ = rhs`: an
    /// assignment whose left side destructures (rustc's destructuring assignment). The
    /// parser reads the left side as a pattern whose places are [`PatKind::Assignee`]s; it
    /// means what rustc desugars it to, `{ let (x, y) = rhs; a = x; b = y; }`.
    DestructuringAssign(Box<Pat>, Box<Expr>),
    /// `_`: only on the left of a destructuring assignment, where the parser turns it into
    /// a pattern; anywhere else it is a syntax error, so no later pass sees one.
    Underscore,
    /// `lhs op= rhs`
    AssignOp(BinOp, Box<Expr>, Box<Expr>),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    MethodCall {
        receiver: Box<Expr>,
        method: Ident,
        args: Vec<Expr>,
    },
    /// `e.Name`, or `e.0` on a tuple: the name is the index as written (`0`, `01`, `0x1`;
    /// only a plain decimal one names a tuple field).
    Field(Box<Expr>, Ident),
    Index(Box<Expr>, Box<Expr>),
    /// `a..b`, `a..`, `..b`, `..`, `a..=b`, `..=b` (an inclusive range always has an end).
    /// Only the whole index of an [`ExprKind::Index`] is parsed as a range (#10); its span
    /// starts at the index's `[`, where rustc locates a panic of the slicing. A lone `..`
    /// element of a tuple or of call arguments is parsed too, for the rest pattern of a
    /// destructuring assignment (`(a, ..) = t`); anywhere else it is a syntax error.
    Range(Option<Box<Expr>>, Option<Box<Expr>>, RangeEnd),
    /// `&e` or `&mut e`
    Ref {
        mutable: bool,
        expr: Box<Expr>,
    },
    /// `*e`
    Deref(Box<Expr>),
    Cast(Box<Expr>, Type),
    Block(Block),
    If {
        cond: Box<Expr>,
        then: Block,
        els: Option<Box<Expr>>,
    },
    While {
        cond: Box<Expr>,
        body: Block,
    },
    Loop {
        body: Block,
    },
    Break(Option<Box<Expr>>),
    Continue,
    Return(Option<Box<Expr>>),
    Struct {
        path: Path,
        fields: Vec<FieldInit>,
        /// The span of a trailing `..` with no base expression: only on the left of a
        /// destructuring assignment (`S { X: a, .. } = s`); anywhere else it is a syntax
        /// error.
        rest: Option<Span>,
    },
    /// `match scrutinee { arms }`
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    /// `let pat = expr`: only as the whole condition of an `if` or `while` (`if let`,
    /// `while let`); let chains are edition 2024.
    Let(Box<Pat>, Box<Expr>),
}

impl Expr {
    /// Block-like expressions end a statement without needing `;`.
    #[must_use]
    pub fn is_block_like(&self) -> bool {
        matches!(
            self.kind,
            ExprKind::Block(_)
                | ExprKind::If { .. }
                | ExprKind::While { .. }
                | ExprKind::Loop { .. }
                | ExprKind::Match { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldInit {
    pub name: Ident,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lit {
    Int {
        value: u128,
        suffix: Option<IntSuffix>,
    },
    /// The source text is kept so printing is exact; the value is decoded by later phases.
    Float {
        text: String,
        suffix: Option<FloatSuffix>,
    },
    Bool(bool),
    Char(char),
    Str(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnOp {
    /// `-`
    Neg,
    /// `!`
    Not,
}

impl UnOp {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neg => "-",
            Self::Not => "!",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
            Self::And => "&&",
            Self::Or => "||",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    #[must_use]
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge
        )
    }

    /// Binding power, higher binds tighter. Mirrors Rust's precedence table.
    #[must_use]
    pub fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge => 3,
            Self::BitOr => 4,
            Self::BitXor => 5,
            Self::BitAnd => 6,
            Self::Shl | Self::Shr => 7,
            Self::Add | Self::Sub => 8,
            Self::Mul | Self::Div | Self::Rem => 9,
        }
    }
}

/// Precedence of `as`, above every binary operator.
pub const CAST_PRECEDENCE: u8 = 10;
/// Precedence of prefix operators, above `as`.
pub const UNARY_PRECEDENCE: u8 = 11;
