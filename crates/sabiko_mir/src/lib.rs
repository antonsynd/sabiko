//! Sabiko's mid-level IR.
//!
//! One [`Body`] per function: a control-flow graph of [`BasicBlockData`] over a flat table
//! of typed [`LocalDecl`]s. Statements assign [`Rvalue`]s to [`Place`]s or mark storage
//! live/dead; every block ends in a [`Terminator`]. Expressions are fully flattened:
//! operands are locals or constants, places are a local plus field/deref projections.
//!
//! The shape follows rustc's MIR closely so that the borrow checker (Phase 3) and the CIL
//! backend (Phase 4) can lean on well-understood designs. Deliberate simplifications: no
//! `Assert` terminators (overflow, division and shift checks are the backend's job: it
//! emits them explicitly, located at the statement's span, decision 43), and no unwind
//! edges: what a panic drops is described by nested [`CleanupData`] scopes over blocks
//! instead (decision 55), which code generation turns into `try`/`fault` regions. A
//! statement lowered from a parenthesised expression carries the span of the outermost
//! parentheses, as rustc's HIR does, so panic locations agree.
//!
//! Drops (decision 55): MIR building emits a [`TerminatorKind::Drop`] wherever a value
//! with drop glue goes out of scope, whether or not it still holds a value, and nests a
//! cleanup scope around the blocks where a panic must drop it. The borrow checker sees
//! that MIR. [`elaborate`] then decides, from initialisation dataflow, which drops run
//! unconditionally, which need a drop flag and which open into their fields, drops the old
//! value of every assignment to an initialised place, and fills in the cleanup scopes.
//!
//! Module map:
//! - [`build`]: AST to MIR lowering (`build::build_bodies`).
//! - [`validate`]: structural and type well-formedness checks, run after every pass.
//! - [`simplify`]: removes unreachable blocks and merges trivial gotos.
//! - [`elaborate`]: drop elaboration, after borrow checking (decision 55).
//! - [`forward`]: copy forwarding, after drop elaboration (decision 51).
//! - [`pretty`]: the `--emit=mir` printer.

pub mod build;
pub mod elaborate;
pub mod forward;
pub mod pretty;
pub mod simplify;
pub mod validate;

pub use sabiko_ast::{BinOp, UnOp};
use sabiko_resolve::{DefId, LocalId};
use sabiko_span::Span;
pub use sabiko_types::{ConstValue, Mutability, Ty};

/// The passes between borrow checking and code generation, in order: drop elaboration
/// (decision 55), simplification, copy forwarding (decision 51) and the removal of
/// storage markers, with the validator after each. Returns the validator's complaints, each
/// prefixed with the pass it followed.
#[must_use]
pub fn lower_for_codegen(bodies: &mut [Body], typeck: &sabiko_types::TypeckResults) -> Vec<String> {
    let mut failures = Vec::new();
    let mut check = |bodies: &[Body], pass: &str| {
        failures.extend(
            validate::validate_all(bodies, typeck)
                .into_iter()
                .map(|f| format!("after {pass}: {f}")),
        );
    };
    elaborate::elaborate_all(bodies, typeck);
    check(bodies, "drop elaboration");
    simplify::simplify_all(bodies);
    check(bodies, "simplification");
    forward::forward_all(bodies);
    check(bodies, "copy forwarding");
    simplify::strip_storage_markers(bodies);
    check(bodies, "removing storage markers");
    failures
}

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

/// Index into [`Body::cleanups`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CleanupScope(pub u32);

impl CleanupScope {
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A cleanup scope: the blocks whose [`BasicBlockData::cleanup`] chain contains it are
/// protected by it. A panic in one of them runs the innermost scope's drops first, then its
/// parent's, and so on outwards (decision 55). MIR building makes one scope per stretch of
/// blocks in which a local with drop glue may hold a value; the blocks of a scope have a
/// single entry from outside it, and scopes nest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupData {
    /// The enclosing scope, whose drops run after this one's.
    pub parent: Option<CleanupScope>,
    /// The local the scope protects.
    pub local: Local,
    /// What unwinding drops, in order. As built: the local, whatever part of it holds a
    /// value. After [`elaborate`]: the exact places, each unconditionally or behind its
    /// drop flag; empty when the local never holds a value at a point that may panic
    /// inside the scope, and then the scope needs no handler.
    pub drops: Vec<CleanupDrop>,
}

/// One drop a cleanup scope performs: `place`, when `flag` is `None` or holds `true`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupDrop {
    pub place: Place,
    pub flag: Option<Local>,
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
    /// The cleanup scopes blocks refer to; a parent always precedes its children.
    pub cleanups: Vec<CleanupData>,
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

    #[must_use]
    pub fn cleanup(&self, scope: CleanupScope) -> &CleanupData {
        &self.cleanups[scope.index()]
    }

    /// The cleanup scopes protecting `bb`, innermost first.
    #[must_use]
    pub fn cleanup_chain(&self, bb: BasicBlock) -> Vec<CleanupScope> {
        let mut out = Vec::new();
        let mut next = self.block(bb).cleanup;
        while let Some(scope) = next {
            out.push(scope);
            next = self.cleanup(scope).parent;
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BasicBlockData {
    pub statements: Vec<Statement>,
    /// `None` only while a block is under construction.
    pub terminator: Option<Terminator>,
    /// The innermost cleanup scope protecting the block, if any (decision 55).
    pub cleanup: Option<CleanupScope>,
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
    /// `let _ = place;`: the place is only mentioned (rustc's `PlaceMention`). Nothing is
    /// read, moved or required to be initialised; the borrow checker counts it as a use
    /// of the local for liveness only, so a loan held by a reference that is mentioned is
    /// live there. Code generation emits nothing.
    PlaceMention(Place),
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
    /// `.field`; the base must be a struct or a tuple (whose elements are numbered from
    /// 0). The field's type is recorded so places can be typed without consulting item
    /// tables.
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

/// What kind of reference an [`Rvalue::Ref`] creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BorrowKind {
    /// `&place`.
    Shared,
    /// `&mut place`. A *two-phase* borrow (`two_phase`) is only reserved where it is
    /// created and activated at its single use, the call it is an argument of: until then
    /// it acts as a shared borrow, so the call's later arguments may still read the place.
    /// MIR building makes only the implicit `&mut` borrows of a call two-phase: an
    /// autoref'd `&mut self` receiver and the reborrow of a `&mut` argument (rustc's
    /// rules, #15).
    Mut { two_phase: bool },
}

impl BorrowKind {
    /// A plain (not two-phase) borrow of the given mutability.
    #[must_use]
    pub fn new(mutability: Mutability) -> Self {
        match mutability {
            Mutability::Not => Self::Shared,
            Mutability::Mut => Self::Mut { two_phase: false },
        }
    }

    #[must_use]
    pub fn mutability(self) -> Mutability {
        match self {
            Self::Shared => Mutability::Not,
            Self::Mut { .. } => Mutability::Mut,
        }
    }

    #[must_use]
    pub fn is_two_phase(self) -> bool {
        matches!(self, Self::Mut { two_phase: true })
    }
}

impl From<Mutability> for BorrowKind {
    fn from(mutability: Mutability) -> Self {
        Self::new(mutability)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rvalue {
    Use(Operand),
    /// `&place` / `&mut place`.
    Ref(BorrowKind, Place),
    UnaryOp(UnOp, Operand),
    /// `BinOp::And` and `BinOp::Or` never appear: lowering short-circuits them.
    BinaryOp(BinOp, Operand, Operand),
    /// `operand as ty`, primitive casts only.
    Cast(Operand, Ty),
    /// A struct value from its fields, in declaration order, or a tuple from its elements.
    Aggregate(AggregateKind, Vec<Operand>),
}

/// What an [`Rvalue::Aggregate`] builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AggregateKind {
    /// A struct (a unit struct has no operands).
    Adt(DefId),
    /// A tuple of at least one element; its type is the tuple of the operands' types
    /// (`()` is a constant, never an aggregate).
    Tuple,
}

impl Rvalue {
    /// The type the rvalue produces.
    #[must_use]
    pub fn ty(&self, body: &Body) -> Ty {
        match self {
            Rvalue::Use(op) | Rvalue::UnaryOp(_, op) => op.ty(body),
            Rvalue::Ref(kind, place) => Ty::Ref(kind.mutability(), Box::new(place.ty(body))),
            Rvalue::BinaryOp(op, lhs, _) => {
                if op.is_comparison() {
                    Ty::BOOL
                } else {
                    lhs.ty(body)
                }
            }
            Rvalue::Cast(_, ty) => ty.clone(),
            Rvalue::Aggregate(AggregateKind::Adt(def), _) => Ty::Adt(*def),
            Rvalue::Aggregate(AggregateKind::Tuple, ops) => {
                Ty::Tuple(ops.iter().map(|op| op.ty(body)).collect::<Vec<_>>().into())
            }
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
    /// Drop the value in `place`, whose type has drop glue, and continue at `target`. As
    /// built (and as the borrow checker sees it), it drops whatever part of the place holds
    /// a value, if any; after [`elaborate`], the whole place holds a value.
    Drop {
        place: Place,
        target: BasicBlock,
    },
    Return,
    Unreachable,
}

impl TerminatorKind {
    /// Every block this terminator may jump to.
    #[must_use]
    pub fn successors(&self) -> Vec<BasicBlock> {
        match self {
            TerminatorKind::Goto { target } | TerminatorKind::Drop { target, .. } => vec![*target],
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
            TerminatorKind::Goto { target } | TerminatorKind::Drop { target, .. } => vec![target],
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
