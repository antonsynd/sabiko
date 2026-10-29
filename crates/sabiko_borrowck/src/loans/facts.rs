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
//! - A two-phase borrow (#15) is *reserved* where it is created and *activated* at its one
//!   use, the call it is an argument of. The reservation is checked like a shared borrow
//!   that also conflicts with every other mutable loan (rustc's `Reservation` access); in
//!   between, the loan tolerates reads and shared borrows of its place (they see it as a
//!   shared loan) but nothing else; the activation is checked as a mutable borrow of the
//!   place against every other live loan. "In between" is rustc's dominance test: the
//!   point is dominated by the reservation's successor but not by the activation.

use std::collections::HashMap;

use polonius_engine::{AllFacts, FactTypes};
use sabiko_mir::{
    AggregateKind, BasicBlock, Body, Local, LocalKind, Mutability, Operand, Place, PlaceElem,
    Rvalue, StatementKind, TerminatorKind,
};
use sabiko_resolve::Builtin;
use sabiko_span::Span;
use sabiko_types::TypeckResults;

use super::regions::{
    signature_regions, Loan, Origin, OriginSupply, Path, Point, RegionTy, SigRegions, SiteElem,
    Variable,
};
use crate::dataflow::{self, Dominators, Location};

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
    /// For a two-phase borrow, the location of the use that activates it (a call). `None`
    /// for an ordinary borrow, which is active from its creation.
    pub activation: Option<Location>,
}

impl LoanData {
    /// Whether the loan is only reserved at `location`: a two-phase borrow between its
    /// creation and its activation. As in rustc, that is every point the reservation's
    /// successor dominates and the activation does not.
    #[must_use]
    pub fn is_reserved_at(&self, location: Location, dominators: &Dominators) -> bool {
        let Some(activation) = self.activation else {
            return false;
        };
        if dominators.dominates(activation, location) {
            return false;
        }
        let after = Location::new(self.location.block, self.location.statement_index + 1);
        dominators.dominates(after, location)
    }
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
    /// The creation of a two-phase borrow: a deep read that also conflicts with every
    /// other mutable loan, reserved or active.
    Reservation,
    /// The use that activates a two-phase borrow: a mutable borrow of its place.
    Activation,
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
            // Readers (and reservations) only clash with mutable loans.
            Self::Read | Self::SharedBorrow | Self::Reservation => loan_mutability.is_mut(),
            Self::Move
            | Self::Write
            | Self::MutBorrow
            | Self::StorageDead
            | Self::Return
            | Self::Activation => true,
        }
    }
}

/// The access that invalidated a loan, for the diagnostic.
#[derive(Clone, Debug)]
pub struct Invalidation {
    pub kind: AccessKind,
    pub place: Place,
    /// For an [`AccessKind::Activation`], the two-phase loan being activated.
    pub activated: Option<Loan>,
}

/// A universal (signature) origin and where it comes from.
#[derive(Clone, Debug)]
pub struct UniversalOrigin {
    pub origin: Origin,
    /// The parameter local and the path from it to the reference with this origin.
    pub param: Local,
    pub path: Vec<SiteElem>,
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

    let predecessors = dataflow::predecessors(body);
    let mut generator = Generator {
        body,
        typeck,
        supply,
        table: LocationTable::new(body),
        dominators: Dominators::new(body, &predecessors),
        loans: Vec::new(),
        loan_at: HashMap::new(),
        activations: HashMap::new(),
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
    dominators: Dominators,
    loans: Vec<LoanData>,
    loan_at: HashMap<Location, Loan>,
    /// The two-phase loans each location activates.
    activations: HashMap<Location, Vec<Loan>>,
    locals: Vec<RegionTy>,
    all: AllFacts<SabikoFacts>,
    invalidations: HashMap<(Point, Loan), Invalidation>,
    uses_at: HashMap<Point, Vec<Local>>,
}

impl Generator<'_> {
    /// Numbers every borrow in the body; accesses anywhere may invalidate any of them. A
    /// two-phase borrow is activated where its temporary is used: MIR building gives it
    /// exactly one use, as a call argument (the validator checks this). Without such a
    /// use it is treated as an ordinary mutable borrow.
    fn collect_loans(&mut self) {
        let uses = operand_uses(self.body);
        for bb in self.body.block_ids() {
            for (index, statement) in self.body.block(bb).statements.iter().enumerate() {
                if let StatementKind::Assign(dest, Rvalue::Ref(kind, place)) = &statement.kind {
                    let location = Location::new(bb, index);
                    let loan = Loan(self.loans.len());
                    let activation = match uses.get(&dest.local).map(Vec::as_slice) {
                        Some(&[at]) if kind.is_two_phase() && dest.is_local() => Some(at),
                        _ => None,
                    };
                    if let Some(at) = activation {
                        self.activations.entry(at).or_default().push(loan);
                    }
                    self.loan_at.insert(location, loan);
                    self.loans.push(LoanData {
                        place: place.clone(),
                        mutability: kind.mutability(),
                        origin: self.supply.fresh(),
                        location,
                        span: statement.span,
                        activation,
                    });
                }
            }
        }
    }

    /// Declares the signature origins as universal, with their placeholder loans and
    /// implied bounds.
    fn universal_regions(&mut self, sig: &SigRegions) -> Vec<UniversalOrigin> {
        let mut universal = Vec::new();
        for (origin, param, path) in sig.input_origin_sites() {
            let loan = Loan(self.loans.len() + universal.len());
            self.all.universal_region.push(origin);
            self.all.placeholder.push((origin, loan));
            universal.push(UniversalOrigin {
                origin,
                param: Local(u32::try_from(param + 1).expect("fewer than 2^32 locals")),
                path,
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
                // Activating a two-phase borrow is a mutable borrow of its place.
                for loan in self.activations.get(&location).cloned().unwrap_or_default() {
                    let place = self.loans[loan.index()].place.clone();
                    self.access_except(&place, AccessKind::Activation, location, Some(loan));
                }
                let arg_tys: Vec<RegionTy> =
                    args.iter().map(|arg| self.operand(arg, location)).collect();
                if matches!(
                    self.typeck.items.builtin(*func),
                    Some(Builtin::Print | Builtin::PrintLine)
                ) {
                    // Overloaded builtins have no signature; their output is `()`, so
                    // nothing flows from the argument to the destination.
                    self.write_dest(destination, location);
                    return;
                }
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
                                activated: None,
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
            Rvalue::Aggregate(AggregateKind::Adt(_), fields) => {
                for field in fields {
                    self.operand(field, location);
                }
                RegionTy::Other
            }
            // A tuple carries its elements' origins: `(&a, 1)` holds the loan of `a`.
            Rvalue::Aggregate(AggregateKind::Tuple, elems) => {
                let tys: Vec<RegionTy> = elems.iter().map(|e| self.operand(e, location)).collect();
                if tys.iter().all(|t| *t == RegionTy::Other) {
                    RegionTy::Other
                } else {
                    RegionTy::Tuple(tys)
                }
            }
            Rvalue::Ref(kind, place) => self.borrow(kind.mutability(), place, location),
        }
    }

    /// `&place` / `&mut place`: issues the loan and adds the reborrow constraints.
    fn borrow(&mut self, mutability: Mutability, place: &Place, location: Location) -> RegionTy {
        self.use_local(place.local, location);
        let loan = self.loan_at[&location];
        let kind = if self.loans[loan.index()].activation.is_some() {
            AccessKind::Reservation
        } else if mutability.is_mut() {
            AccessKind::MutBorrow
        } else {
            AccessKind::SharedBorrow
        };
        self.access(place, kind, location);
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
        match (src, dst) {
            (RegionTy::Ref(m, src_origin, src_inner), RegionTy::Ref(_, dst_origin, dst_inner)) => {
                self.all.subset_base.push((*src_origin, *dst_origin, point));
                if invariant {
                    self.all.subset_base.push((*dst_origin, *src_origin, point));
                }
                self.relate(src_inner, dst_inner, point, invariant || m.is_mut());
            }
            // Element by element; a tuple is covariant in its elements.
            (RegionTy::Tuple(src_elems), RegionTy::Tuple(dst_elems)) => {
                for (s, d) in src_elems.iter().zip(dst_elems) {
                    self.relate(s, d, point, invariant);
                }
            }
            _ => {}
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
        self.access_except(place, kind, location, None);
    }

    /// [`Self::access`], ignoring the loan `except` (the one an activation activates). A
    /// two-phase loan that is only reserved here counts as shared for a read or a shared
    /// borrow, which is all it tolerates.
    fn access_except(
        &mut self,
        place: &Place,
        kind: AccessKind,
        location: Location,
        except: Option<Loan>,
    ) {
        let start = self.table.start(location);
        for (index, loan) in self.loans.iter().enumerate() {
            if except == Some(Loan(index)) {
                continue;
            }
            let reading = matches!(kind, AccessKind::Read | AccessKind::SharedBorrow);
            let mutability = if reading && loan.is_reserved_at(location, &self.dominators) {
                Mutability::Not
            } else {
                loan.mutability
            };
            if places_conflict(place, kind, &loan.place, mutability) {
                self.all.loan_invalidated_at.push((start, Loan(index)));
                self.invalidations
                    .entry((start, Loan(index)))
                    .or_insert_with(|| Invalidation {
                        kind,
                        place: place.clone(),
                        activated: except,
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

/// The locations where each local is used as an operand or as the base of a borrowed
/// place.
fn operand_uses(body: &Body) -> HashMap<Local, Vec<Location>> {
    let mut uses: HashMap<Local, Vec<Location>> = HashMap::new();
    let mut add = |place: &Place, at: Location| uses.entry(place.local).or_default().push(at);
    for bb in body.block_ids() {
        let data = body.block(bb);
        for (index, statement) in data.statements.iter().enumerate() {
            let at = Location::new(bb, index);
            let StatementKind::Assign(_, rvalue) = &statement.kind else {
                continue;
            };
            match rvalue {
                Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => {
                    op.place().into_iter().for_each(|p| add(p, at));
                }
                Rvalue::BinaryOp(_, l, r) => {
                    l.place()
                        .into_iter()
                        .chain(r.place())
                        .for_each(|p| add(p, at));
                }
                Rvalue::Aggregate(_, ops) => ops
                    .iter()
                    .filter_map(Operand::place)
                    .for_each(|p| add(p, at)),
                Rvalue::Ref(_, place) => add(place, at),
            }
        }
        let at = Location::terminator_of(body, bb);
        match data.terminator.as_ref().map(|t| &t.kind) {
            Some(TerminatorKind::Call { args, .. }) => {
                args.iter()
                    .filter_map(Operand::place)
                    .for_each(|p| add(p, at));
            }
            Some(TerminatorKind::SwitchInt { discr, .. }) => {
                discr.place().into_iter().for_each(|p| add(p, at));
            }
            _ => {}
        }
    }
    uses
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
