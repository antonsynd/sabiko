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
pub mod lint;
pub mod mono;
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
    /// For a by-value binding of a `match` arm, `if let`, `while let` or (destructuring)
    /// `let` pattern: the span of the scrutinee or initialiser it is bound out of (rustc's
    /// `opt_match_place`), where a move out of it is reported.
    pub match_span: Option<Span>,
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

/// One drop a cleanup scope performs: `place`, when `flag` is `None` or holds `true` and
/// each enum in `variants` holds the discriminant paired with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupDrop {
    pub place: Place,
    pub flag: Option<Local>,
    /// For a place inside a variant of an enum: the enums on its path (outermost first)
    /// and the discriminant each must hold, read when the drop runs (decision 91).
    pub variants: Vec<(Place, i128)>,
}

/// The MIR of one function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub def: DefId,
    pub name: String,
    /// The generic arguments this body is an instance of (decision 82): empty for a
    /// function that is not generic, and for a generic function's own body, which only
    /// type checking, borrow checking and the lints see; set by [`mono`] on each
    /// instance, whose types then mention no type parameter.
    pub instance: Vec<Ty>,
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

    /// Calls `f` on every place the body's statements, terminators and cleanup scopes
    /// mention (operands, destinations, borrowed, dropped and mentioned places).
    pub fn for_each_place(&self, mut f: impl FnMut(&Place)) {
        let operand = |op: &Operand, f: &mut dyn FnMut(&Place)| {
            if let Some(p) = op.place() {
                f(p);
            }
        };
        for data in &self.blocks {
            for stmt in &data.statements {
                match &stmt.kind {
                    StatementKind::Assign(place, rvalue) => {
                        f(place);
                        match rvalue {
                            Rvalue::Use(op)
                            | Rvalue::UnaryOp(_, op)
                            | Rvalue::Cast(op, _)
                            | Rvalue::Repeat(op, _)
                            | Rvalue::PointerCoercion(op, _) => operand(op, &mut f),
                            Rvalue::BinaryOp(_, l, r) => {
                                operand(l, &mut f);
                                operand(r, &mut f);
                            }
                            Rvalue::Aggregate(_, ops) => {
                                for op in ops {
                                    operand(op, &mut f);
                                }
                            }
                            Rvalue::Subslice(base, bounds) => {
                                operand(base, &mut f);
                                for op in bounds.operands() {
                                    operand(op, &mut f);
                                }
                            }
                            Rvalue::Ref(_, p) | Rvalue::Discriminant(p) | Rvalue::Len(p) => f(p),
                        }
                    }
                    StatementKind::PlaceMention(p) => f(p),
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            if let Some(term) = &data.terminator {
                match &term.kind {
                    TerminatorKind::SwitchInt { discr, .. } => operand(discr, &mut f),
                    TerminatorKind::Call {
                        args, destination, ..
                    } => {
                        for a in args {
                            operand(a, &mut f);
                        }
                        f(destination);
                    }
                    TerminatorKind::Drop { place, .. } => f(place),
                    TerminatorKind::Goto { .. }
                    | TerminatorKind::FalseEdge { .. }
                    | TerminatorKind::Return
                    | TerminatorKind::Unreachable => {}
                }
            }
        }
        for cleanup in &self.cleanups {
            for drop in &cleanup.drops {
                f(&drop.place);
            }
        }
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

    /// `place[index]`: an element of the array or slice in `place`.
    #[must_use]
    pub fn index(mut self, index: Local) -> Self {
        self.projection.push(PlaceElem::Index(index));
        self
    }

    /// The locals holding the indices of the place's `Index` projections.
    pub fn index_locals(&self) -> impl Iterator<Item = Local> + '_ {
        self.projection.iter().filter_map(|e| match e {
            PlaceElem::Index(l) => Some(*l),
            _ => None,
        })
    }

    /// Whether the place goes through an `Index` projection (an element of an array or a
    /// slice): using it checks the index against the length, which may panic.
    #[must_use]
    pub fn has_index(&self) -> bool {
        self.projection
            .iter()
            .any(|e| matches!(e, PlaceElem::Index(_)))
    }

    /// `(place as V)`: the place viewed as the variant `index` of its enum.
    #[must_use]
    pub fn downcast(mut self, index: usize) -> Self {
        self.projection.push(PlaceElem::Downcast(index));
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
                PlaceElem::Downcast(_) => ty,
                PlaceElem::Index(_) => match ty {
                    Ty::Array(elem, _) | Ty::Slice(elem) => *elem,
                    other => panic!("index of non-array type {other:?}"),
                },
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
    /// 0), or an enum downcast to a variant (whose fields are numbered from 0). The
    /// field's type is recorded so places can be typed without consulting item tables.
    Field(usize, Ty),
    /// `(place as V)`: an enum viewed as its variant `V` (by index), only followed by a
    /// `Field` of the variant. The type stays the enum's. Reading through it is only
    /// meaningful when the place holds that variant, which match lowering guarantees.
    Downcast(usize),
    /// `[index]`: the element of an array (`[T; N]`) or a slice (`[T]`, only reached
    /// through a `Deref`) at the `usize` held by the local. Every use of a place with an
    /// index checks it against the length first and panics with rustc's message when it
    /// is out of bounds; the backend emits that check (as it does arithmetic checks).
    Index(Local),
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
    /// A *fake* borrow: it exists only for the borrow checker, creates no reference at
    /// run time (code generation emits nothing for it, and copy forwarding does not count
    /// it as borrowing its place), and its temporary is only ever mentioned.
    Fake(FakeBorrowKind),
    /// `&mut place`. A *two-phase* borrow (`two_phase`) is only reserved where it is
    /// created and activated at its single use, the call it is an argument of: until then
    /// it acts as a shared borrow, so the call's later arguments may still read the place.
    /// MIR building makes only the implicit `&mut` borrows of a call two-phase: an
    /// autoref'd `&mut self` receiver and the reborrow of a `&mut` argument (rustc's
    /// rules, #15).
    Mut { two_phase: bool },
}

/// What a [`BorrowKind::Fake`] borrow stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FakeBorrowKind {
    /// Made while a match guard runs, of a place the match inspects (a place a test reads,
    /// or a place before a `Deref` in it or in a binding's place; rustc's set, see
    /// `build::matches`): it conflicts with assignments to and mutable borrows of the
    /// place or of a prefix of it (rustc's E0510), not with those of places below it (it is
    /// shallow, as rustc's is), nor with reads or moves (decision 60). Taking it is no
    /// access: in rustc it conflicts with no loan, a live `&mut` of the place included.
    Guard,
    /// Of a reference place compared by `==`, `<`, ...: rustc passes `&r` to
    /// `PartialEq::eq`, borrowing the reference itself until the comparison, while MIR
    /// building reborrows `&*r` (decision 63). The reborrow already conflicts with every
    /// deep access (writes through `r`, moves of `r`, `&mut r`); this borrow adds the
    /// shallow writes of `r` and of its prefixes (`r = &y`, `t = ..` for `t.0`), which
    /// overwrite the reference without touching its referent. It conflicts with nothing
    /// else, so no access is reported twice, and it is printed `&fake shallow r`.
    Shallow,
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
            Self::Shared | Self::Fake(_) => Mutability::Not,
            Self::Mut { .. } => Mutability::Mut,
        }
    }

    #[must_use]
    pub fn fake(self) -> Option<FakeBorrowKind> {
        match self {
            Self::Fake(kind) => Some(kind),
            Self::Shared | Self::Mut { .. } => None,
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
    /// The discriminant of the enum in `place`, as an `isize` (decision 15). A read of
    /// the place for the borrow checker.
    Discriminant(Place),
    /// `[operand; count]`: an array of `count` copies of the operand (moved in when
    /// `count` is 1, never used when it is 0).
    Repeat(Operand, u64),
    /// The length of the array or slice in `place`, a `usize`: `N` for `[T; N]`, the
    /// slice's length for `[T]`. Built for `e.len()` from a shared reborrow of the
    /// receiver (`Len(*tmp)` with `tmp = &e`), so the borrow checker sees rustc's autoref.
    Len(Place),
    /// A reference coerced to another pointer representation: `&[T; N]` / `&mut [T; N]`
    /// to `&[T]` / `&mut [T]` (unsizing), or `&mut [T]` to `&[T]`. The result is the
    /// type given, and borrows what the operand borrows.
    PointerCoercion(Operand, Ty),
    /// `base[lo..hi]` (#10): the part of the string or slice the reference `base` points
    /// to, a reference of `base`'s type (`&str`, `&[T]` or `&mut [T]`) that borrows what
    /// `base` borrows, as rustc's `Index::index` call returns it. A missing bound is the
    /// start or the end; bounds out of range panic with rustc's messages, checked by the
    /// backend where the statement runs.
    Subslice(Operand, Box<SliceBounds>),
}

/// The bounds of an [`Rvalue::Subslice`], each a `usize`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceBounds {
    pub lo: Option<Operand>,
    pub hi: Option<Operand>,
    /// `lo..=hi`: `hi` is included (and always present).
    pub inclusive: bool,
}

impl SliceBounds {
    pub fn operands(&self) -> impl Iterator<Item = &Operand> {
        self.lo.iter().chain(&self.hi)
    }

    pub fn operands_mut(&mut self) -> impl Iterator<Item = &mut Operand> {
        self.lo.iter_mut().chain(&mut self.hi)
    }
}

impl Rvalue {
    /// The operands of the rvalue, in evaluation order; a `Ref`, `Discriminant` or `Len`
    /// names a place instead and has none.
    #[must_use]
    pub fn operands(&self) -> Vec<&Operand> {
        match self {
            Rvalue::Use(op)
            | Rvalue::UnaryOp(_, op)
            | Rvalue::Cast(op, _)
            | Rvalue::Repeat(op, _)
            | Rvalue::PointerCoercion(op, _) => vec![op],
            Rvalue::BinaryOp(_, l, r) => vec![l, r],
            Rvalue::Aggregate(_, ops) => ops.iter().collect(),
            Rvalue::Subslice(base, bounds) => {
                std::iter::once(base).chain(bounds.operands()).collect()
            }
            Rvalue::Ref(..) | Rvalue::Discriminant(_) | Rvalue::Len(_) => Vec::new(),
        }
    }
}

/// The places an rvalue reads, moves, borrows or measures.
pub fn rvalue_places(rvalue: &Rvalue) -> impl Iterator<Item = &Place> {
    let (one, many): (Option<&Place>, &[Operand]) = match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp(_, op)
        | Rvalue::Cast(op, _)
        | Rvalue::Repeat(op, _)
        | Rvalue::PointerCoercion(op, _) => (op.place(), &[]),
        Rvalue::BinaryOp(_, l, r) => {
            return l
                .place()
                .into_iter()
                .chain(r.place())
                .collect::<Vec<_>>()
                .into_iter();
        }
        Rvalue::Aggregate(_, ops) => (None, ops.as_slice()),
        Rvalue::Subslice(..) => {
            return rvalue
                .operands()
                .into_iter()
                .filter_map(Operand::place)
                .collect::<Vec<_>>()
                .into_iter();
        }
        Rvalue::Ref(_, p) | Rvalue::Discriminant(p) | Rvalue::Len(p) => (Some(p), &[]),
    };
    one.into_iter()
        .chain(many.iter().filter_map(Operand::place))
        .collect::<Vec<_>>()
        .into_iter()
}

/// What an [`Rvalue::Aggregate`] builds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AggregateKind {
    /// A struct (a unit struct has no operands), with its generic arguments (empty for one
    /// that is not generic).
    Adt(DefId, Box<[Ty]>),
    /// A tuple of at least one element; its type is the tuple of the operands' types
    /// (`()` is a constant, never an aggregate).
    Tuple,
    /// The variant `.1` of the enum `.0`, from the variant's fields, with the enum's
    /// generic arguments.
    Variant(DefId, usize, Box<[Ty]>),
    /// An array `[a, b, c]` of the given element type, from its elements in order (the
    /// type is needed for `[]`).
    Array(Ty),
}

impl Rvalue {
    /// The type the rvalue produces.
    #[must_use]
    pub fn ty(&self, body: &Body) -> Ty {
        match self {
            Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Subslice(op, _) => op.ty(body),
            Rvalue::Ref(kind, place) => Ty::Ref(kind.mutability(), Box::new(place.ty(body))),
            Rvalue::BinaryOp(op, lhs, _) => {
                if op.is_comparison() {
                    Ty::BOOL
                } else {
                    lhs.ty(body)
                }
            }
            Rvalue::Cast(_, ty) | Rvalue::PointerCoercion(_, ty) => ty.clone(),
            Rvalue::Aggregate(
                AggregateKind::Adt(def, args) | AggregateKind::Variant(def, _, args),
                _,
            ) => Ty::Adt(*def, args.clone()),
            Rvalue::Discriminant(_) => Ty::Prim(sabiko_resolve::PrimTy::Isize),
            Rvalue::Repeat(op, n) => Ty::Array(Box::new(op.ty(body)), *n),
            Rvalue::Len(_) => Ty::Prim(sabiko_resolve::PrimTy::Usize),
            Rvalue::Aggregate(AggregateKind::Array(elem), ops) => Ty::Array(
                Box::new(elem.clone()),
                u64::try_from(ops.len()).expect("fewer than 2^64 elements"),
            ),
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
        /// The callee's generic arguments, one per type parameter; empty when it is not
        /// generic. In a generic body they may mention its own type parameters.
        generic_args: Vec<Ty>,
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
    /// rustc's `FalseEdge`: control goes to `real_target`, but the borrow checker also
    /// follows `imaginary_target`. Match lowering puts one after each candidate's tests,
    /// to the next candidate (or to the `else` of an `if let`, `while let` or
    /// `let`-`else`), so code that patterns make unreachable is borrow-checked as in rustc
    /// (#47). [`simplify::remove_false_edges`] turns each into a `Goto` after borrow
    /// checking, before drop elaboration.
    FalseEdge {
        real_target: BasicBlock,
        imaginary_target: BasicBlock,
    },
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
            TerminatorKind::FalseEdge {
                real_target,
                imaginary_target,
            } => vec![*real_target, *imaginary_target],
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
            TerminatorKind::FalseEdge {
                real_target,
                imaginary_target,
            } => vec![real_target, imaginary_target],
            TerminatorKind::Return | TerminatorKind::Unreachable => Vec::new(),
        }
    }
}
