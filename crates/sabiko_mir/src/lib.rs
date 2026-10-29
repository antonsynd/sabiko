//! Sabiko's mid-level IR.
//!
//! One [`Body`] per function: a control-flow graph of [`BasicBlockData`] over a flat table
//! of typed [`LocalDecl`]s. Statements assign [`Rvalue`]s to [`Place`]s or mark storage
//! live/dead; every block ends in a [`Terminator`]. Expressions are fully flattened:
//! operands are locals or constants, places are a local plus field/deref projections.
//!
//! The shape follows rustc's MIR closely so that the borrow checker (Phase 3) and the CIL
//! backend (Phase 4) can lean on well-understood designs. Deliberate simplifications for
//! Phase 2: no `Assert` terminators (overflow, division and shift checks are the backend's
//! job: it emits them explicitly, located at the statement's span, decision 43), no unwind
//! edges, no drops yet. A statement lowered from a parenthesised expression carries the
//! span of the outermost parentheses, as rustc's HIR does, so panic locations agree.
//!
//! Module map:
//! - [`build`]: AST to MIR lowering (`build::build_bodies`).
//! - [`validate`]: structural and type well-formedness checks, run after every pass.
//! - [`simplify`]: removes unreachable blocks and merges trivial gotos.
//! - [`forward`]: copy forwarding, after borrow checking (decision 51).
//! - [`pretty`]: the `--emit=mir` printer.

pub mod build;
pub mod forward;
pub mod pretty;
pub mod simplify;
pub mod validate;

pub use sabiko_ast::{BinOp, UnOp};
use sabiko_resolve::{DefId, LocalId};
use sabiko_span::Span;
pub use sabiko_types::{ConstValue, Mutability, Ty};

/// Index into [`Body::locals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Local(pub u32);

impl Local {
    /// The return place, `_0`.
    pub const RETURN: Local = Local(0);

    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index into [`Body::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BasicBlock(pub u32);

impl BasicBlock {
    pub const START: BasicBlock = BasicBlock(0);

    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LocalKind {
    /// `_0`.
    Return,
    /// `_1 ..= _arg_count`.
    Arg,
    /// A user `let` binding (or `_`-pattern parameter slot).
    User,
    /// Compiler-introduced temporary.
    Temp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDecl {
    pub ty: Ty,
    /// Temporaries and the return place are mutable; user locals follow their binding.
    pub mutability: Mutability,
    pub kind: LocalKind,
    /// The source-level name, for user locals and named parameters.
    pub name: Option<String>,
    pub span: Span,
    /// The resolver's binding, for user locals and parameters.
    pub source: Option<LocalId>,
}

/// The MIR of one function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub def: DefId,
    pub name: String,
    /// `locals[0]` is the return place, then `arg_count` arguments, then the rest.
    pub locals: Vec<LocalDecl>,
    pub arg_count: usize,
    pub blocks: Vec<BasicBlockData>,
    pub span: Span,
}

impl Body {
    #[must_use]
    pub fn local_decl(&self, local: Local) -> &LocalDecl {
        &self.locals[local.index()]
    }

    #[must_use]
    pub fn block(&self, bb: BasicBlock) -> &BasicBlockData {
        &self.blocks[bb.index()]
    }

    pub fn args(&self) -> impl Iterator<Item = Local> {
        (1..=self.arg_count).map(|i| Local(u32::try_from(i).expect("fewer than 2^32 locals")))
    }

    pub fn block_ids(&self) -> impl Iterator<Item = BasicBlock> {
        (0..self.blocks.len())
            .map(|i| BasicBlock(u32::try_from(i).expect("fewer than 2^32 blocks")))
    }

    #[must_use]
    pub fn return_ty(&self) -> &Ty {
        &self.locals[0].ty
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BasicBlockData {
    pub statements: Vec<Statement>,
    /// `None` only while a block is under construction.
    pub terminator: Option<Terminator>,
}

impl BasicBlockData {
    #[must_use]
    pub fn terminator(&self) -> &Terminator {
        self.terminator.as_ref().expect("block has a terminator")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementKind {
    Assign(Place, Rvalue),
    /// The local's storage becomes valid; it is uninitialised until assigned.
    StorageLive(Local),
    /// The local's storage ends. Drop elaboration (Phase 3) inserts drops before this.
    StorageDead(Local),
    Nop,
}

/// A memory location: a local with zero or more projections applied left to right.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub local: Local,
    pub projection: Vec<PlaceElem>,
}

impl Place {
    #[must_use]
    pub fn local(local: Local) -> Self {
        Self {
            local,
            projection: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_local(&self) -> bool {
        self.projection.is_empty()
    }

    #[must_use]
    pub fn deref(mut self) -> Self {
        self.projection.push(PlaceElem::Deref);
        self
    }

    #[must_use]
    pub fn field(mut self, index: usize, ty: Ty) -> Self {
        self.projection.push(PlaceElem::Field(index, ty));
        self
    }

    /// The type of the place, from the local's type and the projections.
    #[must_use]
    pub fn ty(&self, body: &Body) -> Ty {
        let mut ty = body.local_decl(self.local).ty.clone();
        for elem in &self.projection {
            ty = match elem {
                PlaceElem::Deref => match ty {
                    Ty::Ref(_, inner) => *inner,
                    other => panic!("deref of non-reference type {other:?}"),
                },
                PlaceElem::Field(_, field_ty) => field_ty.clone(),
            };
        }
        ty
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PlaceElem {
    /// `*place`; the base must have a reference type.
    Deref,
    /// `.field`; the base must be a struct. The field's type is recorded so places can be
    /// typed without consulting item tables.
    Field(usize, Ty),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    /// Read a `Copy` place.
    Copy(Place),
    /// Read and move out of a non-`Copy` place.
    Move(Place),
    Constant(Constant),
}

impl Operand {
    #[must_use]
    pub fn place(&self) -> Option<&Place> {
        match self {
            Operand::Copy(p) | Operand::Move(p) => Some(p),
            Operand::Constant(_) => None,
        }
    }

    #[must_use]
    pub fn ty(&self, body: &Body) -> Ty {
        match self {
            Operand::Copy(p) | Operand::Move(p) => p.ty(body),
            Operand::Constant(c) => c.ty.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Constant {
    pub ty: Ty,
    pub value: ConstValue,
    pub span: Span,
}

// `ConstValue` holds floats; constants compare by bit pattern for the derived `Eq`s above.
impl Eq for Constant {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rvalue {
    Use(Operand),
    /// `&place` / `&mut place`.
    Ref(Mutability, Place),
    UnaryOp(UnOp, Operand),
    /// `BinOp::And` and `BinOp::Or` never appear: lowering short-circuits them.
    BinaryOp(BinOp, Operand, Operand),
    /// `operand as ty`, primitive casts only.
    Cast(Operand, Ty),
    /// A struct value from its fields, in declaration order.
    Aggregate(DefId, Vec<Operand>),
}

impl Rvalue {
    /// The type the rvalue produces.
    #[must_use]
    pub fn ty(&self, body: &Body) -> Ty {
        match self {
            Rvalue::Use(op) | Rvalue::UnaryOp(_, op) => op.ty(body),
            Rvalue::Ref(m, place) => Ty::Ref(*m, Box::new(place.ty(body))),
            Rvalue::BinaryOp(op, lhs, _) => {
                if op.is_comparison() {
                    Ty::BOOL
                } else {
                    lhs.ty(body)
                }
            }
            Rvalue::Cast(_, ty) => ty.clone(),
            Rvalue::Aggregate(def, _) => Ty::Adt(*def),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminator {
    pub kind: TerminatorKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminatorKind {
    Goto {
        target: BasicBlock,
    },
    /// Branch on an integer or boolean operand. `bool` discriminants use `0`/`1`.
    SwitchInt {
        discr: Operand,
        targets: Vec<(u128, BasicBlock)>,
        otherwise: BasicBlock,
    },
    /// Call a function item. `target` is `None` when the callee never returns.
    Call {
        func: DefId,
        args: Vec<Operand>,
        destination: Place,
        target: Option<BasicBlock>,
    },
    Return,
    Unreachable,
}

impl TerminatorKind {
    /// Every block this terminator may jump to.
    #[must_use]
    pub fn successors(&self) -> Vec<BasicBlock> {
        match self {
            TerminatorKind::Goto { target } => vec![*target],
            TerminatorKind::SwitchInt {
                targets, otherwise, ..
            } => {
                let mut out: Vec<BasicBlock> = targets.iter().map(|(_, bb)| *bb).collect();
                out.push(*otherwise);
                out
            }
            TerminatorKind::Call { target, .. } => target.iter().copied().collect(),
            TerminatorKind::Return | TerminatorKind::Unreachable => Vec::new(),
        }
    }

    pub fn successors_mut(&mut self) -> Vec<&mut BasicBlock> {
        match self {
            TerminatorKind::Goto { target } => vec![target],
            TerminatorKind::SwitchInt {
                targets, otherwise, ..
            } => {
                let mut out: Vec<&mut BasicBlock> = targets.iter_mut().map(|(_, bb)| bb).collect();
                out.push(otherwise);
                out
            }
            TerminatorKind::Call { target, .. } => target.iter_mut().collect(),
            TerminatorKind::Return | TerminatorKind::Unreachable => Vec::new(),
        }
    }
}
