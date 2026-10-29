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
}

impl Item {
    /// The item's name; `None` for an `impl` block.
    #[must_use]
    pub fn name(&self) -> Option<&Ident> {
        match &self.kind {
            ItemKind::Fn(f) => Some(&f.name),
            ItemKind::Struct(s) => Some(&s.name),
            ItemKind::Const(c) => Some(&c.name),
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
    /// `x` or `mut x`
    Ident { mutable: bool, name: Ident },
    /// `_`
    Wild,
    /// `(a, (b, _))`, `(a,)`, `()`: irrefutable tuple destructuring. `(p)` without a comma
    /// is just `p`.
    Tuple(Vec<Pat>),
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
    Paren(Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `lhs = rhs`
    Assign(Box<Expr>, Box<Expr>),
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
    },
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
