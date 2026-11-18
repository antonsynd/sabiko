//! Move checking: uses of moved values (`E0406`) and moves out of references (`E0407`).
//!
//! The design follows rustc's `MoveData` / `MaybeUninitializedPlaces` on the Sabiko subset:
//!
//! - [`MovePaths`] is a forest of *move paths*: every local and every field projection
//!   below it that the body writes, moves out of or indexes as an array (`x`, `x.F`,
//!   `x.F.G`; rustc's set, decision 107), with parent/children links. A use of a place
//!   without a path is checked against its closest ancestor with one, which an error
//!   names. Projections through a `Deref` do not form move paths: a move out of `*r` is an error
//!   on its own, and nothing below a reference is ever tracked. Neither do projections
//!   through an `Index`: a move out of an array element is an error (`E0420`), so an
//!   array is moved and re-initialised whole, and writing an element initialises nothing.
//! - [`MaybeMovedAnalysis`] is a forward "maybe moved" dataflow (union at joins) over the
//!   paths. `Operand::Move(place)` marks the path and all its descendants moved; writing a
//!   place (an assignment or a call destination) re-initialises its path and descendants;
//!   storage markers reset a local's whole subtree.
//! - The checker walks the body with a [`ResultsCursor`] and, for every read, asks whether
//!   the closest path or, when the place has its own, any of its descendants is maybe
//!   moved. A mention is no read, but a
//!   mention through an index reads each indexed base for the bounds check (rustc's
//!   `FakeRead(ForIndex, base)`, decision 96). To point at the move site it
//!   searches backwards from the use through predecessors, stopping at moves and
//!   re-initialisations of the path (or of one of its ancestors); an edge taken against
//!   the dominance order is a loop back edge and changes the label to "in previous
//!   iteration of loop". One diagnostic is reported per set of move sites, so one move
//!   does not cascade into an error at every later use; as in rustc, a later use replaces
//!   the earlier one's report unless it uses a prefix of the place reported (decision 96).

use std::collections::BTreeMap;

use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_mir::{
    BasicBlock, Body, FakeReadCause, Local, LocalKind, Mutability, Operand, Place, PlaceElem,
    Rvalue, Statement, StatementKind, Terminator, TerminatorKind, Ty,
};
use sabiko_resolve::ResolveResults;
use sabiko_span::fx::{HashMap, HashSet};
use sabiko_span::Span;
use sabiko_types::TypeckResults;

use crate::check::{describe_any_place, user_place};
use crate::dataflow::{self, Analysis, BitSet, Dominators, Location, ResultsCursor};
use crate::index::{index_base_read, BaseRead};

/// Index into [`MovePaths`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MovePathIndex(pub usize);

/// One node of the move-path forest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePath {
    /// The place, without any `Deref` projection.
    pub place: Place,
    pub parent: Option<MovePathIndex>,
    pub children: Vec<MovePathIndex>,
}

/// The move paths of one body; see the module docs.
#[derive(Clone, Debug, Default)]
pub struct MovePaths {
    paths: Vec<MovePath>,
    lookup: HashMap<Place, MovePathIndex>,
}

impl MovePaths {
    /// Collects the paths rustc's `MoveDataBuilder` creates: every local, and the
    /// deref-free prefix of every place written (an assignment's or a call's
    /// destination), moved out of, or read for an array's bounds check (rustc's
    /// `FakeRead(ForIndex, a)`), with their ancestors. A place that is only read, borrowed
    /// or matched on has no path of its own: its use is checked against, and an error
    /// names, the closest one (`t` for `&t.0.X` after `let m = t;`, as in rustc).
    #[must_use]
    pub fn build(body: &Body) -> Self {
        let mut paths = Self::default();
        for local in 0..body.locals.len() {
            paths.add(&Place::local(Local(
                u32::try_from(local).expect("fewer than 2^32 locals"),
            )));
        }
        for_each_move_path_place(body, |place| {
            paths.add(&deref_free_prefix(place));
        });
        body.for_each_place(|place| {
            for base in place.index_bases() {
                if matches!(base.ty(body), Ty::Array(..)) {
                    paths.add(&deref_free_prefix(&base));
                }
            }
        });
        paths
    }

    /// Number of paths (the dataflow domain size).
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    #[must_use]
    pub fn path(&self, index: MovePathIndex) -> &MovePath {
        &self.paths[index.0]
    }

    /// Adds `place` (which must be deref-free) and its ancestors; returns its index.
    fn add(&mut self, place: &Place) -> MovePathIndex {
        if let Some(&index) = self.lookup.get(place) {
            return index;
        }
        let parent = (!place.projection.is_empty()).then(|| {
            let parent_place = Place {
                local: place.local,
                projection: place.projection[..place.projection.len() - 1].to_vec(),
            };
            self.add(&parent_place)
        });
        let index = MovePathIndex(self.paths.len());
        self.paths.push(MovePath {
            place: place.clone(),
            parent,
            children: Vec::new(),
        });
        if let Some(parent) = parent {
            self.paths[parent.0].children.push(index);
        }
        self.lookup.insert(place.clone(), index);
        index
    }

    /// The path of `place` itself; `None` when the place goes through a reference (nothing
    /// behind a reference is a move path) or was never seen.
    #[must_use]
    pub fn exact(&self, place: &Place) -> Option<MovePathIndex> {
        if place
            .projection
            .iter()
            .any(|e| matches!(e, PlaceElem::Deref | PlaceElem::Index(_)))
        {
            return None;
        }
        self.lookup.get(place).copied()
    }

    /// The path of the longest prefix of `place` that has one, never past a `Deref` or an
    /// `Index` (rustc's `move_path_closest_to`): the place itself when it has a path,
    /// otherwise its closest ancestor with one, which for a place through a reference is
    /// at most the reference.
    #[must_use]
    pub fn closest(&self, place: &Place) -> Option<MovePathIndex> {
        let mut prefix = deref_free_prefix(place);
        loop {
            if let Some(&index) = self.lookup.get(&prefix) {
                return Some(index);
            }
            prefix.projection.pop()?;
        }
    }

    /// Strict descendants of `index`, in pre-order.
    #[must_use]
    pub fn descendants(&self, index: MovePathIndex) -> Vec<MovePathIndex> {
        let mut out = Vec::new();
        let mut stack: Vec<MovePathIndex> = self.paths[index.0].children.clone();
        stack.reverse();
        while let Some(child) = stack.pop() {
            out.push(child);
            stack.extend(self.paths[child.0].children.iter().rev().copied());
        }
        out
    }

    /// Strict ancestors of `index`, nearest first.
    #[must_use]
    pub fn ancestors(&self, index: MovePathIndex) -> Vec<MovePathIndex> {
        let mut out = Vec::new();
        let mut current = self.paths[index.0].parent;
        while let Some(parent) = current {
            out.push(parent);
            current = self.paths[parent.0].parent;
        }
        out
    }
}

/// `place` cut before its first `Deref` or `Index`.
fn deref_free_prefix(place: &Place) -> Place {
    let len = place
        .projection
        .iter()
        .position(|elem| matches!(elem, PlaceElem::Deref | PlaceElem::Index(_)))
        .unwrap_or(place.projection.len());
    Place {
        local: place.local,
        projection: place.projection[..len].to_vec(),
    }
}

/// Calls `f` on every place the body writes (an assignment's or a call's destination),
/// moves out of (a `move` operand) or drops: the places rustc's `MoveDataBuilder` gathers
/// moves and initialisations of.
fn for_each_move_path_place(body: &Body, mut f: impl FnMut(&Place)) {
    for data in &body.blocks {
        for statement in &data.statements {
            if let StatementKind::Assign(dest, rvalue) = &statement.kind {
                f(dest);
                for_each_rvalue_operand(rvalue, |op| {
                    if let Operand::Move(place) = op {
                        f(place);
                    }
                });
            }
        }
        if let Some(terminator) = &data.terminator {
            for_each_terminator_operand(terminator, |op| {
                if let Operand::Move(place) = op {
                    f(place);
                }
            });
            match &terminator.kind {
                TerminatorKind::Call { destination, .. } => f(destination),
                TerminatorKind::Drop { place, .. } => f(place),
                _ => {}
            }
        }
    }
}

fn for_each_rvalue_operand<'a>(rvalue: &'a Rvalue, mut f: impl FnMut(&'a Operand)) {
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp(_, op)
        | Rvalue::Cast(op, _)
        | Rvalue::Repeat(op, _)
        | Rvalue::PointerCoercion(op, _) => f(op),
        Rvalue::BinaryOp(_, lhs, rhs) => {
            f(lhs);
            f(rhs);
        }
        Rvalue::Aggregate(_, fields) => fields.iter().for_each(f),
        Rvalue::Subslice(base, bounds) => {
            f(base);
            bounds.operands().for_each(f);
        }
        Rvalue::Ref(..) | Rvalue::Discriminant(_) | Rvalue::Len(_) => {}
    }
}

fn for_each_terminator_operand<'a>(terminator: &'a Terminator, mut f: impl FnMut(&'a Operand)) {
    match &terminator.kind {
        TerminatorKind::SwitchInt { discr, .. } => f(discr),
        TerminatorKind::Call { args, .. } => args.iter().for_each(f),
        // A drop is not a use: dropping a moved-out value is drop elaboration's business.
        TerminatorKind::Drop { .. }
        | TerminatorKind::Goto { .. }
        | TerminatorKind::FalseEdge { .. }
        | TerminatorKind::Return
        | TerminatorKind::Unreachable => {}
    }
}

/// The places moved out of at a statement or terminator.
fn moved_places<'a>(body: &'a Body, location: Location) -> Vec<&'a Place> {
    let mut out = Vec::new();
    let collect = |op: &'a Operand| {
        if let Operand::Move(place) = op {
            out.push(place);
        }
    };
    let data = body.block(location.block);
    match data.statements.get(location.statement_index) {
        Some(Statement {
            kind: StatementKind::Assign(_, rvalue),
            ..
        }) => for_each_rvalue_operand(rvalue, collect),
        Some(_) => {}
        None => for_each_terminator_operand(data.terminator(), collect),
    }
    out
}

/// The place written at a statement or terminator, or the local whose storage changes.
fn written_place(body: &Body, location: Location) -> Option<Place> {
    let data = body.block(location.block);
    match data.statements.get(location.statement_index) {
        Some(statement) => match &statement.kind {
            StatementKind::Assign(dest, _) => Some(dest.clone()),
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                Some(Place::local(*local))
            }
            StatementKind::PlaceMention(_) | StatementKind::FakeRead(..) | StatementKind::Nop => {
                None
            }
        },
        None => match &data.terminator().kind {
            TerminatorKind::Call { destination, .. } => Some(destination.clone()),
            _ => None,
        },
    }
}

/// Forward "maybe moved" analysis over move paths, one bit per path; see the module docs.
#[derive(Debug)]
pub struct MaybeMovedAnalysis<'a> {
    paths: &'a MovePaths,
}

impl<'a> MaybeMovedAnalysis<'a> {
    #[must_use]
    pub fn new(paths: &'a MovePaths) -> Self {
        Self { paths }
    }

    /// Sets or clears the bits of `index` and all its descendants.
    fn set_subtree(&self, state: &mut BitSet, index: MovePathIndex, moved: bool) {
        for path in std::iter::once(index).chain(self.paths.descendants(index)) {
            if moved {
                state.insert(path.0);
            } else {
                state.remove(path.0);
            }
        }
    }

    fn gen_moves<'b>(&self, state: &mut BitSet, operands: impl Iterator<Item = &'b Operand>) {
        for operand in operands {
            if let Operand::Move(place) = operand {
                // Moves out of references have no path; they are reported as errors.
                if let Some(index) = self.paths.exact(place) {
                    self.set_subtree(state, index, true);
                }
            }
        }
    }

    fn kill_write(&self, state: &mut BitSet, place: &Place) {
        // Writing through a reference re-initialises nothing the analysis tracks.
        if let Some(index) = self.paths.exact(place) {
            self.set_subtree(state, index, false);
        }
    }
}

impl Analysis for MaybeMovedAnalysis<'_> {
    fn bottom_value(&self, _: &Body) -> BitSet {
        BitSet::new_empty(self.paths.len())
    }

    fn initialize_start_block(&self, _: &Body, _: &mut BitSet) {}

    fn join(&self, into: &mut BitSet, from: &BitSet) -> bool {
        into.union_with(from)
    }

    fn apply_statement(&self, state: &mut BitSet, statement: &Statement, _: Location) {
        match &statement.kind {
            StatementKind::Assign(dest, rvalue) => {
                let mut operands = Vec::new();
                for_each_rvalue_operand(rvalue, |op| operands.push(op));
                self.gen_moves(state, operands.into_iter());
                self.kill_write(state, dest);
            }
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                self.kill_write(state, &Place::local(*local));
            }
            StatementKind::PlaceMention(_) | StatementKind::FakeRead(..) | StatementKind::Nop => {}
        }
    }

    fn apply_terminator(&self, state: &mut BitSet, terminator: &Terminator, _: Location) {
        let mut operands = Vec::new();
        for_each_terminator_operand(terminator, |op| operands.push(op));
        self.gen_moves(state, operands.into_iter());
        if let TerminatorKind::Call { destination, .. } = &terminator.kind {
            self.kill_write(state, destination);
        }
    }
}

/// Reports uses of moved values and moves out of references. Returns the locations of
/// the uses of a maybe-moved place that has move sites, with the place's root local: at
/// those, rustc reports E0382 (here E0406) and never E0381, even when the local is also
/// maybe uninitialised there (`check::check_body` leaves those uses alone).
pub fn check_moves(
    body: &Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) -> HashSet<(Location, Local)> {
    let paths = MovePaths::build(body);
    let analysis = MaybeMovedAnalysis::new(&paths);
    let results = dataflow::iterate_to_fixpoint(body, &analysis);
    let predecessors = dataflow::predecessors(body);
    let dominators = Dominators::new(body, &predecessors);
    let mut checker = MoveChecker {
        body,
        resolve,
        typeck,
        sink,
        paths: &paths,
        cursor: ResultsCursor::new(body, &analysis, &results),
        predecessors,
        dominators,
        move_errors: BTreeMap::new(),
        binding: None,
        drop_moves: Vec::new(),
        moved_uses: HashSet::default(),
    };
    // In rustc's order (`visit_results` walks reverse postorder): a later use replaces an
    // earlier one's error, so the order decides which use is reported.
    for bb in body.reverse_postorder() {
        let data = body.block(bb);
        for (index, statement) in data.statements.iter().enumerate() {
            let location = Location::new(bb, index);
            checker.cursor.seek_before(location);
            checker.check_statement(statement, location);
        }
        if let Some(terminator) = &data.terminator {
            let location = Location::terminator_of(body, bb);
            checker.cursor.seek_before(location);
            checker.check_terminator(terminator, location);
        }
    }
    checker.report_pattern_drop_moves();
    for error in std::mem::take(&mut checker.move_errors).into_values() {
        checker.sink.emit(error.diagnostic);
    }
    checker.moved_uses
}

/// How a place is being used; selects the wording of `E0406`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UseKind {
    /// An operand read.
    Use,
    /// `&place` / `&mut place`.
    Borrow,
    /// `place.F = ..`: the struct in `place` is partially assigned.
    PartialAssign,
    /// `place.F = ..` where `place`'s type implements `Drop`, which needs all of `place`.
    Assign,
}

impl UseKind {
    fn noun(self) -> &'static str {
        match self {
            UseKind::Use => "use of",
            UseKind::Borrow => "borrow of",
            UseKind::PartialAssign => "assign to part of",
            UseKind::Assign => "assign of",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            UseKind::Use => "used",
            UseKind::Borrow => "borrowed",
            UseKind::PartialAssign => "partially assigned",
            UseKind::Assign => "assigned",
        }
    }
}

/// A move found by the backward search from a use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MoveSite {
    location: Location,
    path: MovePathIndex,
    /// Whether the search went against the dominance order to reach it: the move is in an
    /// earlier iteration of a loop that contains the use.
    traversed_back_edge: bool,
}

struct MoveChecker<'a> {
    body: &'a Body,
    resolve: &'a ResolveResults,
    typeck: &'a TypeckResults,
    sink: &'a mut DiagnosticSink,
    paths: &'a MovePaths,
    cursor: ResultsCursor<'a, MaybeMovedAnalysis<'a>>,
    predecessors: Vec<Vec<BasicBlock>>,
    dominators: Dominators,
    /// The `E0406` errors, one per set of move sites (as sorted `(location, path)`
    /// pairs), emitted after the walk; see [`MoveChecker::report_use_of_moved`].
    move_errors: BTreeMap<Vec<(Location, MovePathIndex)>, BufferedMoveError>,
    /// While the operand of a by-value pattern binding is checked: the binding.
    binding: Option<Local>,
    /// Pattern bindings that move out of a type implementing `Drop`, grouped by the
    /// scrutinee they are bound out of: E0418 is reported once per scrutinee, as rustc
    /// reports E0509, after the walk.
    drop_moves: Vec<PatternMoves>,
    /// The uses found to have move sites, with their place's root local; see
    /// [`check_moves`].
    moved_uses: HashSet<(Location, Local)>,
}

/// A use-of-moved-value error waiting for the end of the walk, with the place whose use
/// it reports (rustc's `buffered_move_errors`).
struct BufferedMoveError {
    used: Place,
    diagnostic: Diagnostic,
}

/// The bindings of one scrutinee that move out of one place whose type implements `Drop`
/// (rustc's `GroupedMoveError::MovesFromValue`: `(h as One)` and `(h as Two)` are two
/// places, so two arms moving out of one enum are two errors).
struct PatternMoves {
    scrutinee: Span,
    base: Place,
    /// The type implementing `Drop`, named in the message.
    adt: sabiko_resolve::DefId,
    /// Each binding once: its span, name and type.
    bindings: Vec<(Span, String, Ty)>,
}

impl MoveChecker<'_> {
    fn check_statement(&mut self, statement: &Statement, location: Location) {
        match &statement.kind {
            StatementKind::Assign(dest, rvalue) => {
                if let Rvalue::Ref(_, place) = rvalue {
                    self.check_use(place, UseKind::Borrow, location, statement.span);
                } else if let Rvalue::Discriminant(place) | Rvalue::Len(place) = rvalue {
                    // Matching reads the discriminant: a use of the whole value.
                    self.check_use(place, UseKind::Use, location, statement.span);
                } else {
                    if let Rvalue::Use(Operand::Move(_)) = rvalue {
                        self.binding = Some(dest.local).filter(|&local| {
                            dest.is_local() && self.body.local_decl(local).match_span.is_some()
                        });
                    }
                    let mut operands = Vec::new();
                    for_each_rvalue_operand(rvalue, |op| operands.push(op));
                    for operand in operands {
                        self.check_operand(operand, location, statement.span);
                    }
                    self.binding = None;
                }
                self.check_write(dest, location, statement.span);
            }
            // An index's bounds check reads the indexed place (decision 96): a slice's
            // length through a fake borrow ("borrow of moved value").
            StatementKind::FakeRead(FakeReadCause::ForIndex, base) => {
                let read = index_base_read(base, self.body);
                let kind = match read.kind {
                    BaseRead::Array | BaseRead::SharedSliceRef => UseKind::Use,
                    BaseRead::SliceFakeBorrow => UseKind::Borrow,
                };
                self.check_use(&read.place, kind, location, statement.span);
            }
            // A mention is not a use: `let _ = p;` after a move of `p` is fine. A match
            // test reusing an earlier test's read reads nothing a guard could have moved.
            StatementKind::PlaceMention(_)
            | StatementKind::FakeRead(FakeReadCause::ForMatchTest, _)
            | StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop => {}
        }
    }

    fn check_terminator(&mut self, terminator: &Terminator, location: Location) {
        let mut operands = Vec::new();
        for_each_terminator_operand(terminator, |op| operands.push(op));
        for operand in operands {
            self.check_operand(operand, location, terminator.span);
        }
        if let TerminatorKind::Call { destination, .. } = &terminator.kind {
            self.check_write(destination, location, terminator.span);
        }
    }

    fn check_operand(&mut self, operand: &Operand, location: Location, span: Span) {
        match operand {
            Operand::Copy(place) => self.check_use(place, UseKind::Use, location, span),
            Operand::Move(place) => {
                self.check_use(place, UseKind::Use, location, span);
                if !self.check_move_out_of_index(place, span)
                    && !self.check_move_out_of_borrow(place, span)
                {
                    self.check_move_out_of_drop_type(place, span);
                }
            }
            Operand::Constant(_) => {}
        }
    }

    /// A read or borrow of `place` (rustc's `check_if_path_or_child_is_moved`): neither
    /// its closest path nor, when the place has a path of its own, any descendant of that
    /// may be maybe-moved.
    fn check_use(&mut self, place: &Place, kind: UseKind, location: Location, span: Span) {
        let Some(closest) = self.paths.closest(place) else {
            return;
        };
        if self.cursor.get().contains(closest.0) {
            // Like rustc, name the move path (`r` for `r.X` read through a moved `r`, `t`
            // for `t.0.X` after `t` moved).
            let described = self.paths.path(closest).place.clone();
            self.report_use_of_moved(&described, place, closest, kind, location, span);
            return;
        }
        let Some(exact) = self.paths.exact(place) else {
            return;
        };
        let state = self.cursor.get();
        let moved_child = self
            .paths
            .descendants(exact)
            .into_iter()
            .find(|child| state.contains(child.0));
        if let Some(child) = moved_child {
            self.report_use_of_moved(place, place, child, kind, location, span);
        }
    }

    /// `place = ..`, as rustc's `check_if_assigned_path_is_moved`. Writing a bare local
    /// re-initialises it; writing a field is a partial assignment of the enclosing value,
    /// which must not be moved, and of a value whose type implements `Drop` an assignment
    /// that needs all of it; writing through a reference uses the reference.
    fn check_write(&mut self, place: &Place, location: Location, span: Span) {
        for cut in (0..place.projection.len()).rev() {
            let base = Place {
                local: place.local,
                projection: place.projection[..cut].to_vec(),
            };
            match place.projection[cut] {
                // A field behind a reference is the referent's: `(*r).F = ..` uses `r`,
                // which the `Deref` cut checks (rustc's `check_parent_of_field` finds no
                // path below a `Deref`).
                PlaceElem::Field(..) if base.projection.contains(&PlaceElem::Deref) => {}
                // `d.F = ..` with `d: D` and `D: Drop`: `d` is dropped whole later, so it
                // must be whole now (rustc: "assign of moved value").
                PlaceElem::Field(..) if self.implements_drop(&base) => {
                    self.check_use(&base, UseKind::Assign, location, span);
                    return;
                }
                PlaceElem::Field(..) => {
                    if self.check_parent_of_field(&base, location, span) {
                        return;
                    }
                }
                // Assigning to a variant's field is fine when assigning to the field is.
                PlaceElem::Downcast(_) => {}
                // Writing an element uses the array, which its bounds check's `FakeRead`
                // already checked (rustc: "use of moved value"); rustc stops there too.
                PlaceElem::Index(_) => return,
                // Writing through a reference uses the reference: `r` for `(*r).F = ..`.
                PlaceElem::Deref => {
                    if let Some(index) = self.paths.closest(&base) {
                        if self.cursor.get().contains(index.0) {
                            let described = self.paths.path(index).place.clone();
                            self.report_use_of_moved(
                                &described,
                                &base,
                                index,
                                UseKind::Use,
                                location,
                                span,
                            );
                        }
                    }
                    // Everything behind the reference is the referent's business.
                    return;
                }
            }
        }
    }

    /// The partial assignment of `base` (a write of a field of it) after a move of it or
    /// of an enclosing value, as rustc's `check_parent_of_field`: the error names the
    /// shortest maybe-moved prefix with a move path (`p` for `p.A.B = 1` after `p`
    /// moved), not the closest one. Returns whether it found one.
    fn check_parent_of_field(&mut self, base: &Place, location: Location, span: Span) -> bool {
        let state = self.cursor.get();
        let shortest = (0..=base.projection.len()).find_map(|len| {
            let prefix = Place {
                local: base.local,
                projection: base.projection[..len].to_vec(),
            };
            self.paths
                .exact(&prefix)
                .filter(|index| state.contains(index.0))
                .map(|index| (prefix, index))
        });
        let Some((prefix, index)) = shortest else {
            return false;
        };
        self.report_use_of_moved(&prefix, base, index, UseKind::PartialAssign, location, span);
        true
    }

    /// Whether `place`'s type is an ADT implementing `Drop`.
    fn implements_drop(&self, place: &Place) -> bool {
        matches!(place.ty(self.body), Ty::Adt(adt, _) if self.typeck.items.drop_impl(adt).is_some())
    }

    /// `E0418` (rustc's E0509): a move of a field out of a value whose type implements
    /// `Drop`, at any depth (`o.Inner`, `o.Inner.X`): the value's `drop` will still see the
    /// whole of it. Copy fields are copied, never moved, so they are fine.
    fn check_move_out_of_drop_type(&mut self, place: &Place, span: Span) {
        let mut ty = self.body.local_decl(place.local).ty.clone();
        for (index, elem) in place.projection.iter().enumerate() {
            let field_ty = match elem {
                PlaceElem::Field(_, field_ty) => field_ty,
                // `(e as V)` is still `e`: its fields are checked at the next `Field`.
                PlaceElem::Downcast(_) => continue,
                PlaceElem::Deref | PlaceElem::Index(_) => return,
            };
            if let Ty::Adt(adt, _) = ty {
                if self.typeck.items.drop_impl(adt).is_some() {
                    let base = Place {
                        local: place.local,
                        projection: place.projection[..index].to_vec(),
                    };
                    match self.binding {
                        Some(binding) => self.record_pattern_drop_move(binding, base, adt, span),
                        None => self.report_move_out_of_drop_type(place, &base, adt, span),
                    }
                    return;
                }
            }
            ty = field_ty.clone();
        }
    }

    fn report_move_out_of_drop_type(
        &mut self,
        place: &Place,
        base: &Place,
        adt: sabiko_resolve::DefId,
        span: Span,
    ) {
        let name = self.typeck.items.adt(adt).name.clone();
        let desc = self.describe(place);
        let ty = place.ty(self.body);
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::MOVE_OUT_OF_DROP,
                format!("cannot move out of type `{name}`, which implements the `Drop` trait"),
            )
            .with_label(span, "cannot move out of here")
            .with_note(format!(
                "move occurs because {desc} has type `{}`, which does not implement the `Copy` trait",
                ty.display(self.resolve)
            ))
            .with_note(format!(
                "{} is dropped as a whole by its `drop`; borrow the field instead",
                self.describe(base)
            )),
        );
    }

    /// Remembers a by-value pattern binding `binding` (at `span`) that moves out of `base`,
    /// of type `adt`, which implements `Drop`, for [`Self::report_pattern_drop_moves`].
    fn record_pattern_drop_move(
        &mut self,
        binding: Local,
        base: Place,
        adt: sabiko_resolve::DefId,
        span: Span,
    ) {
        let decl = self.body.local_decl(binding);
        let scrutinee = decl.match_span.expect("a pattern binding");
        let entry = (span, decl.name.clone().unwrap_or_default(), decl.ty.clone());
        match self
            .drop_moves
            .iter_mut()
            .find(|g| g.scrutinee == scrutinee && g.base == base)
        {
            Some(group) => {
                if !group.bindings.iter().any(|(s, ..)| *s == span) {
                    group.bindings.push(entry);
                }
            }
            None => self.drop_moves.push(PatternMoves {
                scrutinee,
                base,
                adt,
                bindings: vec![entry],
            }),
        }
    }

    /// `E0418` (rustc's E0509) for pattern bindings, once per scrutinee and moved-from
    /// place, at the scrutinee, with a
    /// label at each binding that moves (rustc's "data moved here", "...and here").
    fn report_pattern_drop_moves(&mut self) {
        for group in std::mem::take(&mut self.drop_moves) {
            let name = self.typeck.items.adt(group.adt).name.clone();
            let mut diag = Diagnostic::error(
                ErrorCode::MOVE_OUT_OF_DROP,
                format!("cannot move out of type `{name}`, which implements the `Drop` trait"),
            )
            .with_label(group.scrutinee, "cannot move out of here");
            let mut bindings = group.bindings;
            bindings.sort_by_key(|(span, ..)| span.start);
            for (index, (span, ..)) in bindings.iter().enumerate() {
                let text = if index == 0 {
                    "data moved here"
                } else {
                    "...and here"
                };
                diag = diag.with_label(*span, text);
            }
            diag = match &bindings[..] {
                [(_, binding, ty)] => diag.with_note(format!(
                    "move occurs because `{binding}` has type `{}`, which does not implement the `Copy` trait",
                    ty.display(self.resolve)
                )),
                _ => diag.with_note(
                    "move occurs because these variables have types that don't implement the `Copy` trait",
                ),
            };
            let names: Vec<String> = bindings
                .iter()
                .map(|(_, b, _)| format!("`ref {b}`"))
                .collect();
            self.sink.emit(diag.with_note(format!(
                "consider borrowing the pattern binding instead: {}",
                names.join(", ")
            )));
        }
    }

    /// `E0420` (rustc's E0508): a move out of an array or slice element, at any depth
    /// (`a[i]`, `a[i].Inner`, `s[0]` through a reference). Returns whether it reported.
    fn check_move_out_of_index(&mut self, place: &Place, span: Span) -> bool {
        let Some(base) = place.index_bases().next() else {
            return false;
        };
        let base_ty = base.ty(self.body);
        let what = match base_ty {
            Ty::Slice(_) => "a non-copy slice".to_owned(),
            _ => "a non-copy array".to_owned(),
        };
        let desc = self.describe(place);
        let ty = place.ty(self.body);
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::MOVE_OUT_OF_INDEX,
                format!(
                    "cannot move out of type `{}`, {what}",
                    base_ty.display(self.resolve)
                ),
            )
            .with_label(span, "cannot move out of here")
            .with_note(format!(
                "move occurs because {desc} has type `{}`, which does not implement the `Copy` trait",
                ty.display(self.resolve)
            ))
            .with_note("borrow the element instead (`&a[i]`), or replace it"),
        );
        true
    }

    /// `E0407`: a move whose place reads through a reference. Returns whether it reported.
    fn check_move_out_of_borrow(&mut self, place: &Place, span: Span) -> bool {
        let Some(deref) = place
            .projection
            .iter()
            .rposition(|elem| *elem == PlaceElem::Deref)
        else {
            return false;
        };
        let reference = Place {
            local: place.local,
            projection: place.projection[..deref].to_vec(),
        };
        let ty = place.ty(self.body);
        debug_assert!(
            !self
                .typeck
                .items
                .is_copy_in(&ty, self.typeck.items.param_env(self.body.def)),
            "the MIR builder moves out of a Copy place {place:?}"
        );
        let kind = match reference.ty(self.body) {
            Ty::Ref(Mutability::Mut, _) => "mutable",
            _ => "shared",
        };
        // A place rustc cannot name (rooted in a parameter's pattern, `&(t, n): &(T,
        // i32)`, #49): "cannot move out of a shared reference".
        let (what, subject) = match user_place(place, self.body, self.typeck) {
            Some(desc) => (
                format!("`{desc}` which is behind a {kind} reference"),
                format!("`{desc}`"),
            ),
            None => (format!("a {kind} reference"), "the value".to_owned()),
        };
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::MOVE_OUT_OF_BORROW,
                format!("cannot move out of {what}"),
            )
            .with_label(
                span,
                format!(
                    "move occurs because {subject} has type `{}`, which does not implement the `Copy` trait",
                    ty.display(self.resolve)
                ),
            ),
        );
        true
    }

    /// `E0406` for a use of `used` (a deref-free place: the move path read, or the place
    /// whose descendant `moved` is maybe-moved) at `location`. `used_place` is the place
    /// the statement uses, as rustc's `used_place`: the whole place read or borrowed, the
    /// base of a place written, the indexed place of a bounds check.
    ///
    /// One error is reported per set of move sites, as rustc's `buffer_move_error` does:
    /// a later use with the same moves is dropped when its place is a prefix of the place
    /// already reported (`p.X` then `p`; `a` read for a bounds check after `a[i].X`), and
    /// otherwise replaces the earlier error (`a` read for the bounds check of `&a[i]`, then
    /// the borrow of `a[i]`: "borrow of moved value", at the borrow).
    fn report_use_of_moved(
        &mut self,
        used: &Place,
        used_place: &Place,
        moved: MovePathIndex,
        kind: UseKind,
        location: Location,
        span: Span,
    ) {
        // Temporaries are moved exactly once by construction and the return place is never
        // moved from; a user never wrote those uses.
        if !matches!(
            self.body.local_decl(used.local).kind,
            LocalKind::User | LocalKind::Arg
        ) {
            return;
        }
        let sites = self.find_move_sites(location, moved);
        if sites.is_empty() {
            return;
        }
        self.moved_uses.insert((location, used.local));
        let mut key: Vec<_> = sites.iter().map(|s| (s.location, s.path)).collect();
        key.sort_unstable();
        key.dedup();
        if let Some(earlier) = self.move_errors.get(&key) {
            if used_place.is_prefix_of(&earlier.used) {
                return;
            }
        }

        let is_partial = sites
            .iter()
            .any(|site| self.is_strict_descendant(site.path, used));
        let partially = if is_partial { "partially " } else { "" };
        let partial = if is_partial { "partial " } else { "" };
        let used_desc = self.describe(used);
        let mut diag = Diagnostic::error(
            ErrorCode::USE_OF_MOVED_VALUE,
            format!("{} {partially}moved value: {used_desc}", kind.noun()),
        )
        .with_label(
            span,
            format!("value {} here after {partial}move", kind.verb()),
        );
        let mut labelled = HashSet::default();
        for site in &sites {
            let site_span = self.span_of(site.location);
            if !labelled.insert(site_span) {
                continue;
            }
            let site_partially = if self.is_strict_descendant(site.path, used) {
                "partially "
            } else {
                ""
            };
            let in_loop = if site.traversed_back_edge || site.location == location {
                ", in previous iteration of loop"
            } else {
                ""
            };
            diag = diag.with_label(
                site_span,
                format!("value {site_partially}moved here{in_loop}"),
            );
        }

        let moved_place = &self.paths.path(sites[0].path).place;
        let moved_desc = self.describe(moved_place);
        let moved_ty = moved_place.ty(self.body);
        diag = diag.with_note(format!(
            "{partial}move occurs because {moved_desc} has type `{}`, which does not implement the `Copy` trait",
            moved_ty.display(self.resolve)
        ));
        if let Some(moved) = user_place(moved_place, self.body, self.typeck) {
            if self.is_plain_let_of_mut_ref(sites[0], &moved_ty) {
                diag = diag.with_note(format!("consider reborrowing: `&mut *{moved}`"));
            } else if self.is_mut_ref_moved_into_call(sites[0], &moved_ty) {
                // rustc's help for a `&mut` passed where a type parameter is expected
                // (`Id(r)`), which moves it (#132).
                diag = diag.with_note(format!(
                    "consider creating a fresh reborrow of `{moved}` here: `&mut *{moved}`"
                ));
            }
        }
        self.move_errors.insert(
            key,
            BufferedMoveError {
                used: used_place.clone(),
                diagnostic: diag,
            },
        );
    }

    /// Whether `path` is a strict descendant of `used`: a part of it was moved.
    fn is_strict_descendant(&self, path: MovePathIndex, used: &Place) -> bool {
        let place = &self.paths.path(path).place;
        place.projection.len() > used.projection.len() && used.is_prefix_of(place)
    }

    /// Whether the move at `site` is `let s = r;` of a `&mut T` local: a reborrow would
    /// have kept `r` usable.
    fn is_plain_let_of_mut_ref(&self, site: MoveSite, moved_ty: &Ty) -> bool {
        if !matches!(moved_ty, Ty::Ref(Mutability::Mut, _))
            || !self.paths.path(site.path).place.is_local()
        {
            return false;
        }
        let data = self.body.block(site.location.block);
        match data.statements.get(site.location.statement_index) {
            Some(Statement {
                kind: StatementKind::Assign(dest, Rvalue::Use(Operand::Move(_))),
                ..
            }) => dest.is_local() && self.body.local_decl(dest.local).kind == LocalKind::User,
            _ => false,
        }
    }

    /// Whether the move at `site` is of a `&mut T` local into a call's argument: the
    /// temporary it is moved into is moved into a call. A parameter whose type is not
    /// known to be a reference at the call (a type parameter's) moves the argument.
    ///
    /// Call arguments are always temporaries, so the destination need not be checked for
    /// one: a call moves no user binding, parameter or return place (`let s = r;` into a
    /// binding is [`Self::is_plain_let_of_mut_ref`]'s, asked first).
    fn is_mut_ref_moved_into_call(&self, site: MoveSite, moved_ty: &Ty) -> bool {
        if !matches!(moved_ty, Ty::Ref(Mutability::Mut, _))
            || !self.paths.path(site.path).place.is_local()
        {
            return false;
        }
        let data = self.body.block(site.location.block);
        let Some(Statement {
            kind: StatementKind::Assign(dest, Rvalue::Use(Operand::Move(_))),
            ..
        }) = data.statements.get(site.location.statement_index)
        else {
            return false;
        };
        self.body.blocks.iter().any(|b| {
            matches!(
                &b.terminator,
                Some(Terminator { kind: TerminatorKind::Call { args, .. }, .. })
                    if args.iter().any(|a| matches!(a, Operand::Move(p) if p == dest))
            )
        })
    }

    /// The moves of `moved` or of one of its ancestors that reach `use_location`, by a
    /// backward search through predecessors that stops at moves and re-initialisations of
    /// those paths. Forward edges are explored before back edges, as in rustc, so a move
    /// reachable both ways is attributed to the current iteration.
    fn find_move_sites(&self, use_location: Location, moved: MovePathIndex) -> Vec<MoveSite> {
        let mut targets = vec![moved];
        targets.extend(self.paths.ancestors(moved));

        let mut sites = Vec::new();
        let mut visited = HashSet::default();
        let mut stack = Vec::new();
        let mut back_edge_stack = Vec::new();
        self.push_predecessors(use_location, &mut stack, &mut back_edge_stack);
        let mut is_back_edge = false;
        while let Some(location) = stack.pop().or_else(|| {
            is_back_edge = true;
            back_edge_stack.pop()
        }) {
            if !visited.insert(location) {
                continue;
            }
            let moves_here: Vec<MovePathIndex> = moved_places(self.body, location)
                .into_iter()
                .filter_map(|place| self.paths.exact(place))
                .filter(|path| targets.contains(path))
                .collect();
            if !moves_here.is_empty() {
                sites.extend(moves_here.into_iter().map(|path| MoveSite {
                    location,
                    path,
                    traversed_back_edge: is_back_edge,
                }));
                continue;
            }
            let reinitialised = written_place(self.body, location)
                .and_then(|place| self.paths.exact(&place))
                .is_some_and(|path| targets.contains(&path));
            if reinitialised {
                continue;
            }
            self.push_predecessors(location, &mut stack, &mut back_edge_stack);
        }
        sites.sort_unstable();
        sites
    }

    /// Pushes the locations just before `location`, split by whether the edge from them
    /// goes against the dominance order (a loop back edge).
    fn push_predecessors(
        &self,
        location: Location,
        stack: &mut Vec<Location>,
        back_edge_stack: &mut Vec<Location>,
    ) {
        let preds: Vec<Location> = if location.statement_index > 0 {
            vec![Location::new(location.block, location.statement_index - 1)]
        } else {
            self.predecessors[location.block.index()]
                .iter()
                .map(|&bb| Location::terminator_of(self.body, bb))
                .collect()
        };
        for pred in preds {
            if self.dominates(location, pred) {
                back_edge_stack.push(pred);
            } else {
                stack.push(pred);
            }
        }
    }

    /// Whether every path from the start to `b` passes through `a`.
    fn dominates(&self, a: Location, b: Location) -> bool {
        self.dominators.dominates(a, b)
    }

    fn span_of(&self, location: Location) -> Span {
        let data = self.body.block(location.block);
        data.statements
            .get(location.statement_index)
            .map_or_else(|| data.terminator().span, |s| s.span)
    }

    fn describe(&self, place: &Place) -> String {
        describe_any_place(place, self.body, self.typeck)
    }
}
