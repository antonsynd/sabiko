//! Polonius fact generation for one body.
//!
//! The encoding follows rustc's NLL facts. Every MIR location has two points, `Start`
//! and `Mid`: invalidations happen at `Start`, everything else (definitions, uses, loan
//! issue, kills, subset constraints) at `Mid`, so an access and the loan it conflicts with
//! meet at `Start` while a loan issued by a statement only exists after it.
//!
//! - Each `&place` / `&mut place` is a loan with a fresh origin; the reference's type is
//!   `&'fresh <place type>`. Dereferencing a `&'r T` on the way to the borrowed place adds
//!   `'r: 'fresh` (a reborrow; the walk stops at the innermost shared reference, as in
//!   rustc, since a shared reference's referent is only copied out).
//! - Assignments, call arguments and call results relate the source's [`RegionTy`] to the
//!   destination's, covariantly, with both directions below a `&mut`.
//! - An access invalidates every live loan whose place it conflicts with (see
//!   [`places_conflict`]); the kind of access is remembered for the diagnostic.
//! - Overwriting a local or its `*local`, a call result and `StorageDead` kill every loan
//!   rooted at that local; a deeper write kills the loans it conflicts with. Kills follow
//!   invalidations (at `Mid` versus `Start`), so an error is still reported once and then
//!   the stale loan stops producing more.
//! - `Return` invalidates every loan of local data (a place without `Deref`): data owned
//!   by the frame is gone once the function returns.

use std::collections::HashMap;

use polonius_engine::{AllFacts, FactTypes};
use sabiko_mir::{
    BasicBlock, Body, Local, LocalKind, Mutability, Operand, Place, PlaceElem, Rvalue,
    StatementKind, TerminatorKind,
};
use sabiko_span::Span;
use sabiko_types::TypeckResults;

use super::regions::{
    signature_regions, Loan, Origin, OriginSupply, Path, Point, RegionTy, SigRegions, Variable,
};
use crate::dataflow::Location;

/// The atom types the engine is instantiated with.
#[derive(Clone, Copy, Debug)]
pub struct SabikoFacts;

impl FactTypes for SabikoFacts {
    type Origin = Origin;
    type Loan = Loan;
    type Point = Point;
    type Variable = Variable;
    type Path = Path;
}

/// Maps MIR locations to points and back. Locations are numbered block by block
/// (`statements.len() + 1` per block, the last one being the terminator); location `i`
/// has points `Start = 2i` and `Mid = 2i + 1`.
#[derive(Clone, Debug)]
pub struct LocationTable {
    /// The first location index of each block, plus the total as a sentinel.
    block_starts: Vec<usize>,
}

impl LocationTable {
    #[must_use]
    pub fn new(body: &Body) -> Self {
        let mut block_starts = Vec::with_capacity(body.blocks.len() + 1);
        let mut next = 0;
        for data in &body.blocks {
            block_starts.push(next);
            next += data.statements.len() + 1;
        }
        block_starts.push(next);
        Self { block_starts }
    }

    fn index(&self, location: Location) -> usize {
        self.block_starts[location.block.index()] + location.statement_index
    }

    #[must_use]
    pub fn start(&self, location: Location) -> Point {
        Point(2 * self.index(location))
    }

    #[must_use]
    pub fn mid(&self, location: Location) -> Point {
        Point(2 * self.index(location) + 1)
    }

    #[must_use]
    pub fn is_mid(point: Point) -> bool {
        point.0 % 2 == 1
    }

    /// The location a point belongs to.
    #[must_use]
    pub fn location(&self, point: Point) -> Location {
        let index = point.0 / 2;
        let block = self.block_starts.partition_point(|&start| start <= index) - 1;
        Location::new(
            BasicBlock(u32::try_from(block).expect("fewer than 2^32 blocks")),
            index - self.block_starts[block],
        )
    }
}

/// One borrow in the body.
#[derive(Clone, Debug)]
pub struct LoanData {
    pub place: Place,
    pub mutability: Mutability,
    /// The origin of the reference the borrow creates.
    pub origin: Origin,
    pub location: Location,
    /// The span of the borrowing statement.
    pub span: Span,
}

/// What a statement does to a place, for conflict detection and error wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    /// A `copy` operand (deep read).
    Read,
    /// A `move` operand (deep write).
    Move,
    /// An assignment or call destination (shallow write).
    Write,
    /// `&place` (deep read).
    SharedBorrow,
    /// `&mut place` (deep write).
    MutBorrow,
    /// `StorageDead(local)` (shallow write).
    StorageDead,
    /// `return`, which ends every local's storage.
    Return,
}

impl AccessKind {
    /// A shallow access touches the place itself but nothing behind a `Deref` inside it:
    /// overwriting a reference does not touch what it used to point to.
    #[must_use]
    pub fn is_shallow(self) -> bool {
        matches!(self, Self::Write | Self::StorageDead)
    }

    /// Whether the access conflicts with a loan of the same (or overlapping) place.
    #[must_use]
    pub fn conflicts_with(self, loan_mutability: Mutability) -> bool {
        match self {
            // Readers only clash with mutable loans.
            Self::Read | Self::SharedBorrow => loan_mutability.is_mut(),
            Self::Move | Self::Write | Self::MutBorrow | Self::StorageDead | Self::Return => true,
        }
    }
}

/// The access that invalidated a loan, for the diagnostic.
#[derive(Clone, Debug)]
pub struct Invalidation {
    pub kind: AccessKind,
    pub place: Place,
}

/// A universal (signature) origin and where it comes from.
#[derive(Clone, Copy, Debug)]
pub struct UniversalOrigin {
    pub origin: Origin,
    /// The parameter local and how many references enclose the one with this origin.
    pub param: Local,
    pub depth: usize,
}

/// Everything the diagnostics need next to the engine's output.
#[derive(Debug)]
pub struct Facts {
    pub all: AllFacts<SabikoFacts>,
    pub table: LocationTable,
    pub loans: Vec<LoanData>,
    /// The region type of every local.
    pub locals: Vec<RegionTy>,
    pub universal: Vec<UniversalOrigin>,
    /// The first access conflicting with each loan at each point.
    pub invalidations: HashMap<(Point, Loan), Invalidation>,
    /// The locals read at each point (`var_used_at`, indexed).
    pub uses_at: HashMap<Point, Vec<Local>>,
}

impl Facts {
    /// The origins of the return place.
    #[must_use]
    pub fn return_origins(&self) -> Vec<Origin> {
        self.locals[Local::RETURN.index()].origins()
    }
}

/// Why facts could not be generated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactsError {
    /// The return type contains a reference but the parameters offer `input_origins`
    /// origins, not exactly one.
    MissingLifetime { input_origins: usize },
}

/// Generates the facts of `body`.
pub fn generate(body: &Body, typeck: &TypeckResults) -> Result<Facts, FactsError> {
    let mut supply = OriginSupply::default();
    let sig = signature_regions(typeck.items.fn_sig(body.def), &mut supply);
    let Some(output) = sig.output.clone() else {
        return Err(FactsError::MissingLifetime {
            input_origins: sig.input_origins,
        });
    };

    let mut locals = Vec::with_capacity(body.locals.len());
    for (index, decl) in body.locals.iter().enumerate() {
        let region_ty = match decl.kind {
            LocalKind::Return => output.clone(),
            LocalKind::Arg => sig
                .inputs
                .get(index - 1)
                .cloned()
                .unwrap_or_else(|| RegionTy::fresh(&decl.ty, &mut supply)),
            LocalKind::User | LocalKind::Temp => RegionTy::fresh(&decl.ty, &mut supply),
        };
        locals.push(region_ty);
    }

    let mut generator = Generator {
        body,
        typeck,
        supply,
        table: LocationTable::new(body),
        loans: Vec::new(),
        loan_at: HashMap::new(),
        locals,
        all: AllFacts::default(),
        invalidations: HashMap::new(),
        uses_at: HashMap::new(),
    };
    generator.collect_loans();
    let universal = generator.universal_regions(&sig);
    generator.visit_body();
    Ok(Facts {
        all: generator.all,
        table: generator.table,
        loans: generator.loans,
        locals: generator.locals,
        universal,
        invalidations: generator.invalidations,
        uses_at: generator.uses_at,
    })
}

struct Generator<'a> {
    body: &'a Body,
    typeck: &'a TypeckResults,
    supply: OriginSupply,
    table: LocationTable,
    loans: Vec<LoanData>,
    loan_at: HashMap<Location, Loan>,
    locals: Vec<RegionTy>,
    all: AllFacts<SabikoFacts>,
    invalidations: HashMap<(Point, Loan), Invalidation>,
    uses_at: HashMap<Point, Vec<Local>>,
}

impl Generator<'_> {
    /// Numbers every borrow in the body; accesses anywhere may invalidate any of them.
    fn collect_loans(&mut self) {
        for bb in self.body.block_ids() {
            for (index, statement) in self.body.block(bb).statements.iter().enumerate() {
                if let StatementKind::Assign(_, Rvalue::Ref(mutability, place)) = &statement.kind {
                    let location = Location::new(bb, index);
                    self.loan_at.insert(location, Loan(self.loans.len()));
                    self.loans.push(LoanData {
                        place: place.clone(),
                        mutability: *mutability,
                        origin: self.supply.fresh(),
                        location,
                        span: statement.span,
                    });
                }
            }
        }
    }

    /// Declares the signature origins as universal, with their placeholder loans and
    /// implied bounds.
    fn universal_regions(&mut self, sig: &SigRegions) -> Vec<UniversalOrigin> {
        let mut universal = Vec::new();
        for (origin, param, depth) in sig.input_origin_sites() {
            let loan = Loan(self.loans.len() + universal.len());
            self.all.universal_region.push(origin);
            self.all.placeholder.push((origin, loan));
            universal.push(UniversalOrigin {
                origin,
                param: Local(u32::try_from(param + 1).expect("fewer than 2^32 locals")),
                depth,
            });
        }
        self.all.known_placeholder_subset = sig.implied_bounds();
        universal
    }

    fn visit_body(&mut self) {
        for (local, region_ty) in self.locals.iter().enumerate() {
            for origin in region_ty.origins() {
                self.all
                    .use_of_var_derefs_origin
                    .push((Variable(local), origin));
            }
        }
        for bb in self.body.block_ids() {
            let data = self.body.block(bb);
            for (index, statement) in data.statements.iter().enumerate() {
                let location = Location::new(bb, index);
                self.cfg_edges(location, &[]);
                self.visit_statement(&statement.kind, location);
            }
            let location = Location::terminator_of(self.body, bb);
            let Some(terminator) = &data.terminator else {
                continue;
            };
            self.cfg_edges(location, &terminator.kind.successors());
            self.visit_terminator(&terminator.kind, location);
        }
    }

    /// `Start -> Mid` and `Mid -> Start` of the next statement, or of each successor
    /// block for a terminator.
    fn cfg_edges(&mut self, location: Location, successors: &[BasicBlock]) {
        let start = self.table.start(location);
        let mid = self.table.mid(location);
        self.all.cfg_edge.push((start, mid));
        let is_terminator =
            location.statement_index == self.body.block(location.block).statements.len();
        if is_terminator {
            for &succ in successors {
                let next = self.table.start(Location::new(succ, 0));
                self.all.cfg_edge.push((mid, next));
            }
        } else {
            let next = self
                .table
                .start(Location::new(location.block, location.statement_index + 1));
            self.all.cfg_edge.push((mid, next));
        }
    }

    fn visit_statement(&mut self, kind: &StatementKind, location: Location) {
        match kind {
            StatementKind::Assign(place, rvalue) => {
                let src = self.rvalue(rvalue, location);
                self.write_dest(place, location);
                let dst = self.place_region_ty(place);
                self.relate(&src, &dst, self.table.mid(location), false);
            }
            StatementKind::StorageLive(local) => {
                self.define(*local, location);
            }
            StatementKind::StorageDead(local) => {
                self.define(*local, location);
                self.access(&Place::local(*local), AccessKind::StorageDead, location);
                self.kill_loans_of_local(*local, location);
            }
            StatementKind::Nop => {}
        }
    }

    fn visit_terminator(&mut self, kind: &TerminatorKind, location: Location) {
        match kind {
            TerminatorKind::SwitchInt { discr, .. } => {
                self.operand(discr, location);
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                ..
            } => {
                let arg_tys: Vec<RegionTy> =
                    args.iter().map(|arg| self.operand(arg, location)).collect();
                let callee = signature_regions(self.typeck.items.fn_sig(*func), &mut self.supply);
                let mid = self.table.mid(location);
                for (arg, param) in arg_tys.iter().zip(&callee.inputs) {
                    self.relate(arg, param, mid, false);
                }
                self.write_dest(destination, location);
                // A callee whose own signature failed elision constrains nothing: its
                // error is reported with its body.
                if let Some(ret) = &callee.output {
                    let dst = self.place_region_ty(destination);
                    self.relate(ret, &dst, mid, false);
                }
            }
            TerminatorKind::Return => {
                self.use_local(Local::RETURN, location);
                let start = self.table.start(location);
                for (index, loan) in self.loans.iter().enumerate() {
                    let is_local_data = !loan
                        .place
                        .projection
                        .iter()
                        .any(|elem| matches!(elem, PlaceElem::Deref));
                    if is_local_data {
                        self.all.loan_invalidated_at.push((start, Loan(index)));
                        self.invalidations
                            .entry((start, Loan(index)))
                            .or_insert_with(|| Invalidation {
                                kind: AccessKind::Return,
                                place: loan.place.clone(),
                            });
                    }
                }
            }
            TerminatorKind::Goto { .. } | TerminatorKind::Unreachable => {}
        }
    }

    /// Reads the operand, returning its region type.
    fn operand(&mut self, operand: &Operand, location: Location) -> RegionTy {
        match operand {
            Operand::Copy(place) => {
                self.use_local(place.local, location);
                self.access(place, AccessKind::Read, location);
                self.place_region_ty(place)
            }
            Operand::Move(place) => {
                self.use_local(place.local, location);
                self.access(place, AccessKind::Move, location);
                self.place_region_ty(place)
            }
            Operand::Constant(constant) => RegionTy::fresh(&constant.ty, &mut self.supply),
        }
    }

    /// Evaluates the rvalue's reads and borrows, returning its region type.
    fn rvalue(&mut self, rvalue: &Rvalue, location: Location) -> RegionTy {
        match rvalue {
            Rvalue::Use(operand) => self.operand(operand, location),
            Rvalue::UnaryOp(_, operand) | Rvalue::Cast(operand, _) => {
                self.operand(operand, location);
                RegionTy::Other
            }
            Rvalue::BinaryOp(_, lhs, rhs) => {
                self.operand(lhs, location);
                self.operand(rhs, location);
                RegionTy::Other
            }
            Rvalue::Aggregate(_, fields) => {
                for field in fields {
                    self.operand(field, location);
                }
                RegionTy::Other
            }
            Rvalue::Ref(mutability, place) => self.borrow(*mutability, place, location),
        }
    }

    /// `&place` / `&mut place`: issues the loan and adds the reborrow constraints.
    fn borrow(&mut self, mutability: Mutability, place: &Place, location: Location) -> RegionTy {
        self.use_local(place.local, location);
        let kind = if mutability.is_mut() {
            AccessKind::MutBorrow
        } else {
            AccessKind::SharedBorrow
        };
        self.access(place, kind, location);
        let loan = self.loan_at[&location];
        let origin = self.loans[loan.index()].origin;
        let mid = self.table.mid(location);
        self.all.loan_issued_at.push((origin, loan, mid));

        // Reborrow constraints, innermost dereference first: the reference being
        // dereferenced must outlive the new one. A `&mut` must stay valid for the whole
        // borrow (someone else could use the deref after it is gone), so the walk goes on
        // to the next dereference; a shared reference's referent is only copied out, so
        // the walk stops there.
        let base = &self.locals[place.local.index()];
        for index in (0..place.projection.len()).rev() {
            if place.projection[index] != PlaceElem::Deref {
                continue;
            }
            let RegionTy::Ref(base_mutability, base_origin, _) =
                base.project(&place.projection[..index])
            else {
                break;
            };
            self.all.subset_base.push((base_origin, origin, mid));
            if !base_mutability.is_mut() {
                break;
            }
        }
        RegionTy::Ref(mutability, origin, Box::new(self.place_region_ty(place)))
    }

    /// The destination of an assignment or call: defines a bare local (or reads the root
    /// of a projected place), writes shallowly and kills the loans the write makes stale.
    fn write_dest(&mut self, place: &Place, location: Location) {
        if place.is_local() {
            self.define(place.local, location);
        } else {
            self.use_local(place.local, location);
        }
        self.access(place, AccessKind::Write, location);
        match place.projection[..] {
            [] | [PlaceElem::Deref] => self.kill_loans_of_local(place.local, location),
            _ => {
                let mid = self.table.mid(location);
                for (index, loan) in self.loans.iter().enumerate() {
                    if places_conflict(place, AccessKind::Move, &loan.place, loan.mutability) {
                        self.all.loan_killed_at.push((Loan(index), mid));
                    }
                }
            }
        }
    }

    /// `subset_base(src, dst)` for each pair of corresponding references: the value's
    /// origin must outlive the destination's. Below a `&mut` the referent is invariant.
    fn relate(&mut self, src: &RegionTy, dst: &RegionTy, point: Point, invariant: bool) {
        if let (RegionTy::Ref(m, src_origin, src_inner), RegionTy::Ref(_, dst_origin, dst_inner)) =
            (src, dst)
        {
            self.all.subset_base.push((*src_origin, *dst_origin, point));
            if invariant {
                self.all.subset_base.push((*dst_origin, *src_origin, point));
            }
            self.relate(src_inner, dst_inner, point, invariant || m.is_mut());
        }
    }

    fn place_region_ty(&self, place: &Place) -> RegionTy {
        self.locals[place.local.index()].project(&place.projection)
    }

    fn define(&mut self, local: Local, location: Location) {
        self.all
            .var_defined_at
            .push((Variable(local.index()), self.table.mid(location)));
    }

    fn use_local(&mut self, local: Local, location: Location) {
        let mid = self.table.mid(location);
        self.all.var_used_at.push((Variable(local.index()), mid));
        self.uses_at.entry(mid).or_default().push(local);
    }

    /// Invalidates every loan the access conflicts with, at `Start`.
    fn access(&mut self, place: &Place, kind: AccessKind, location: Location) {
        let start = self.table.start(location);
        for (index, loan) in self.loans.iter().enumerate() {
            if places_conflict(place, kind, &loan.place, loan.mutability) {
                self.all.loan_invalidated_at.push((start, Loan(index)));
                self.invalidations
                    .entry((start, Loan(index)))
                    .or_insert_with(|| Invalidation {
                        kind,
                        place: place.clone(),
                    });
            }
        }
    }

    fn kill_loans_of_local(&mut self, local: Local, location: Location) {
        let mid = self.table.mid(location);
        for (index, loan) in self.loans.iter().enumerate() {
            if loan.place.local == local {
                self.all.loan_killed_at.push((Loan(index), mid));
            }
        }
    }
}

/// Whether an access of `access` with `kind` conflicts with a loan of `borrowed`.
///
/// Two places rooted at different locals never conflict, nor do two that reach different
/// fields of the same struct. Otherwise one is a prefix of the other: a shallow access
/// cannot reach a loan that extends past it through a `Deref`, and the remaining cases are
/// decided by [`AccessKind::conflicts_with`].
#[must_use]
pub fn places_conflict(
    access: &Place,
    kind: AccessKind,
    borrowed: &Place,
    loan_mutability: Mutability,
) -> bool {
    if access.local != borrowed.local {
        return false;
    }
    for (a, b) in access.projection.iter().zip(&borrowed.projection) {
        if let (PlaceElem::Field(fa, _), PlaceElem::Field(fb, _)) = (a, b) {
            if fa != fb {
                return false;
            }
        }
    }
    if kind.is_shallow() && borrowed.projection.len() > access.projection.len() {
        let rest = &borrowed.projection[access.projection.len()..];
        if rest.contains(&PlaceElem::Deref) {
            return false;
        }
    }
    kind.conflicts_with(loan_mutability)
}
