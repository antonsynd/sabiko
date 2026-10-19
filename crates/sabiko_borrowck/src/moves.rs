//! Move checking: uses of moved values (`E0406`) and moves out of references (`E0407`).
//!
//! The design follows rustc's `MoveData` / `MaybeUninitializedPlaces` on the Sabiko subset:
//!
//! - [`MovePaths`] is a forest of *move paths*: every local and every field projection
//!   below it that occurs in the body (`x`, `x.F`, `x.F.G`), with parent/children links.
//!   Projections through a `Deref` do not form move paths: a move out of `*r` is an error
//!   on its own, and nothing below a reference is ever tracked.
//! - [`MaybeMovedAnalysis`] is a forward "maybe moved" dataflow (union at joins) over the
//!   paths. `Operand::Move(place)` marks the path and all its descendants moved; writing a
//!   place (an assignment or a call destination) re-initialises its path and descendants;
//!   storage markers reset a local's whole subtree.
//! - The checker walks the body with a [`ResultsCursor`] and, for every read, asks whether
//!   the read path or any of its descendants is maybe moved. To point at the move site it
//!   searches backwards from the use through predecessors, stopping at moves and
//!   re-initialisations of the path (or of one of its ancestors); an edge taken against
//!   the dominance order is a loop back edge and changes the label to "in previous
//!   iteration of loop". A diagnostic is suppressed when the same set of move sites was
//!   already reported, so one move does not cascade into an error at every later use.

use std::collections::{HashMap, HashSet};

use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_mir::{
    BasicBlock, Body, Local, LocalKind, Mutability, Operand, Place, PlaceElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, Ty,
};
use sabiko_resolve::ResolveResults;
use sabiko_span::Span;
use sabiko_types::TypeckResults;

use crate::check::describe_place;
use crate::dataflow::{self, Analysis, BitSet, Location, ResultsCursor};

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
    /// Collects a path for every deref-free prefix of every place in `body`.
    #[must_use]
    pub fn build(body: &Body) -> Self {
        let mut paths = Self::default();
        for_each_place(body, |place| {
            paths.add(&deref_free_prefix(place));
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
        if place.projection.contains(&PlaceElem::Deref) {
            return None;
        }
        self.lookup.get(place).copied()
    }

    /// The path of the longest deref-free prefix of `place`: the place itself when it has
    /// no `Deref`, otherwise the reference it reads through.
    #[must_use]
    pub fn closest(&self, place: &Place) -> Option<MovePathIndex> {
        self.lookup.get(&deref_free_prefix(place)).copied()
    }

    /// The path of a bare local, if the body mentions it.
    #[must_use]
    pub fn of_local(&self, local: Local) -> Option<MovePathIndex> {
        self.lookup.get(&Place::local(local)).copied()
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

/// `place` cut before its first `Deref`.
fn deref_free_prefix(place: &Place) -> Place {
    let len = place
        .projection
        .iter()
        .position(|elem| *elem == PlaceElem::Deref)
        .unwrap_or(place.projection.len());
    Place {
        local: place.local,
        projection: place.projection[..len].to_vec(),
    }
}

/// Calls `f` on every place the body mentions: read, written or borrowed.
fn for_each_place(body: &Body, mut f: impl FnMut(&Place)) {
    for data in &body.blocks {
        for statement in &data.statements {
            match &statement.kind {
                StatementKind::Assign(dest, rvalue) => {
                    f(dest);
                    if let Rvalue::Ref(_, place) = rvalue {
                        f(place);
                    }
                    for_each_rvalue_operand(rvalue, |op| {
                        if let Some(place) = op.place() {
                            f(place);
                        }
                    });
                }
                StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                    f(&Place::local(*local));
                }
                StatementKind::Nop => {}
            }
        }
        if let Some(terminator) = &data.terminator {
            for_each_terminator_operand(terminator, |op| {
                if let Some(place) = op.place() {
                    f(place);
                }
            });
            if let TerminatorKind::Call { destination, .. } = &terminator.kind {
                f(destination);
            }
        }
    }
}

fn for_each_rvalue_operand<'a>(rvalue: &'a Rvalue, mut f: impl FnMut(&'a Operand)) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => f(op),
        Rvalue::BinaryOp(_, lhs, rhs) => {
            f(lhs);
            f(rhs);
        }
        Rvalue::Aggregate(_, fields) => fields.iter().for_each(f),
        Rvalue::Ref(..) => {}
    }
}

fn for_each_terminator_operand<'a>(terminator: &'a Terminator, mut f: impl FnMut(&'a Operand)) {
    match &terminator.kind {
        TerminatorKind::SwitchInt { discr, .. } => f(discr),
        TerminatorKind::Call { args, .. } => args.iter().for_each(f),
        TerminatorKind::Goto { .. } | TerminatorKind::Return | TerminatorKind::Unreachable => {}
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
            StatementKind::Nop => None,
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

    #[must_use]
    pub fn paths(&self) -> &'a MovePaths {
        self.paths
    }

    /// Whether `index` is maybe-moved in `state`.
    #[must_use]
    pub fn is_moved(state: &BitSet, index: MovePathIndex) -> bool {
        state.contains(index.0)
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
    fn domain_size(&self, _: &Body) -> usize {
        self.paths.len()
    }

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
            StatementKind::Nop => {}
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

/// Reports uses of moved values and moves out of references.
pub fn check_moves(
    body: &Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    let paths = MovePaths::build(body);
    let analysis = MaybeMovedAnalysis::new(&paths);
    let results = dataflow::iterate_to_fixpoint(body, &analysis);
    let predecessors = predecessors(body);
    let dominators = dominators(body, &predecessors);
    let mut checker = MoveChecker {
        body,
        resolve,
        typeck,
        sink,
        paths: &paths,
        cursor: ResultsCursor::new(body, &analysis, &results),
        predecessors,
        dominators,
        reported: HashSet::new(),
    };
    for bb in body.block_ids() {
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
}

/// The predecessor blocks of every block.
fn predecessors(body: &Body) -> Vec<Vec<BasicBlock>> {
    let mut preds = vec![Vec::new(); body.blocks.len()];
    for bb in body.block_ids() {
        if let Some(terminator) = &body.block(bb).terminator {
            for succ in terminator.kind.successors() {
                preds[succ.index()].push(bb);
            }
        }
    }
    preds
}

/// `dominators[b]` is the set of blocks that dominate `b` (including `b`), by the classic
/// iterative data-flow formulation. Unreachable blocks keep the full set.
fn dominators(body: &Body, predecessors: &[Vec<BasicBlock>]) -> Vec<BitSet> {
    let n = body.blocks.len();
    let mut doms = vec![BitSet::new_filled(n); n];
    if n == 0 {
        return doms;
    }
    let start = BasicBlock::START.index();
    doms[start] = BitSet::new_empty(n);
    doms[start].insert(start);
    let mut changed = true;
    while changed {
        changed = false;
        for bb in 1..n {
            let mut new = BitSet::new_filled(n);
            for pred in &predecessors[bb] {
                new.intersect_with(&doms[pred.index()]);
            }
            new.insert(bb);
            if new != doms[bb] {
                doms[bb] = new;
                changed = true;
            }
        }
    }
    doms
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
}

impl UseKind {
    fn noun(self) -> &'static str {
        match self {
            UseKind::Use => "use of",
            UseKind::Borrow => "borrow of",
            UseKind::PartialAssign => "assign to part of",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            UseKind::Use => "used",
            UseKind::Borrow => "borrowed",
            UseKind::PartialAssign => "partially assigned",
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
    dominators: Vec<BitSet>,
    /// The sets of move sites already reported, as sorted `(location, path)` pairs.
    reported: HashSet<Vec<(Location, MovePathIndex)>>,
}

impl MoveChecker<'_> {
    fn check_statement(&mut self, statement: &Statement, location: Location) {
        match &statement.kind {
            StatementKind::Assign(dest, rvalue) => {
                if let Rvalue::Ref(_, place) = rvalue {
                    self.check_use(place, UseKind::Borrow, location, statement.span);
                } else {
                    let mut operands = Vec::new();
                    for_each_rvalue_operand(rvalue, |op| operands.push(op));
                    for operand in operands {
                        self.check_operand(operand, location, statement.span);
                    }
                }
                self.check_write(dest, location, statement.span);
            }
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
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
                self.check_move_out_of_borrow(place, span);
            }
            Operand::Constant(_) => {}
        }
    }

    /// A read or borrow of `place`: neither its path nor, when it does not go through a
    /// reference, any descendant may be maybe-moved.
    fn check_use(&mut self, place: &Place, kind: UseKind, location: Location, span: Span) {
        let Some(closest) = self.paths.closest(place) else {
            return;
        };
        if self.cursor.get().contains(closest.0) {
            // Like rustc, name the move path (for `r.X` read through a moved `r`: `r`).
            let described = self.paths.path(closest).place.clone();
            self.report_use_of_moved(&described, closest, kind, location, span);
            return;
        }
        if place.projection.contains(&PlaceElem::Deref) {
            return;
        }
        let state = self.cursor.get();
        let moved_child = self
            .paths
            .descendants(closest)
            .into_iter()
            .find(|child| state.contains(child.0));
        if let Some(child) = moved_child {
            self.report_use_of_moved(place, child, kind, location, span);
        }
    }

    /// `place = ..`. Writing a bare local re-initialises it; writing a field is a partial
    /// assignment of each enclosing struct, which must not be moved; writing through a
    /// reference uses the reference.
    fn check_write(&mut self, place: &Place, location: Location, span: Span) {
        for cut in (0..place.projection.len()).rev() {
            let base = Place {
                local: place.local,
                projection: place.projection[..cut].to_vec(),
            };
            let kind = match place.projection[cut] {
                PlaceElem::Field(..) => UseKind::PartialAssign,
                PlaceElem::Deref => UseKind::Use,
            };
            if let Some(index) = self.paths.closest(&base) {
                if self.cursor.get().contains(index.0) {
                    let described = self.paths.path(index).place.clone();
                    self.report_use_of_moved(&described, index, kind, location, span);
                    return;
                }
            }
            if kind == UseKind::Use {
                // Everything behind the reference is the referent's business.
                return;
            }
        }
    }

    /// `E0407`: a move whose place reads through a reference.
    fn check_move_out_of_borrow(&mut self, place: &Place, span: Span) {
        let Some(deref) = place
            .projection
            .iter()
            .rposition(|elem| *elem == PlaceElem::Deref)
        else {
            return;
        };
        let reference = Place {
            local: place.local,
            projection: place.projection[..deref].to_vec(),
        };
        let ty = place.ty(self.body);
        debug_assert!(
            !self.typeck.items.is_copy(&ty),
            "the MIR builder moves out of a Copy place {place:?}"
        );
        let kind = match reference.ty(self.body) {
            Ty::Ref(Mutability::Mut, _) => "mutable",
            _ => "shared",
        };
        let desc = self.describe(place);
        self.sink.emit(
            Diagnostic::error(
                ErrorCode::MOVE_OUT_OF_BORROW,
                format!("cannot move out of `{desc}` which is behind a {kind} reference"),
            )
            .with_label(
                span,
                format!(
                    "move occurs because `{desc}` has type `{}`, which does not implement the `Copy` trait",
                    ty.display(self.resolve)
                ),
            ),
        );
    }

    /// `E0406` for a use of `used` (a deref-free place: the move path read, or the place
    /// whose descendant `moved` is maybe-moved) at `location`.
    fn report_use_of_moved(
        &mut self,
        used: &Place,
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
        let mut key: Vec<_> = sites.iter().map(|s| (s.location, s.path)).collect();
        key.sort_unstable();
        key.dedup();
        if !self.reported.insert(key) {
            return;
        }

        let is_partial = sites
            .iter()
            .any(|site| self.is_strict_descendant(site.path, used));
        let partially = if is_partial { "partially " } else { "" };
        let partial = if is_partial { "partial " } else { "" };
        let used_desc = self.describe(used);
        let mut diag = Diagnostic::error(
            ErrorCode::USE_OF_MOVED_VALUE,
            format!("{} {partially}moved value: `{used_desc}`", kind.noun()),
        )
        .with_label(
            span,
            format!("value {} here after {partial}move", kind.verb()),
        );
        let mut labelled = HashSet::new();
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
            "{partial}move occurs because `{moved_desc}` has type `{}`, which does not implement the `Copy` trait",
            moved_ty.display(self.resolve)
        ));
        if self.is_plain_let_of_mut_ref(sites[0], &moved_ty) {
            diag = diag.with_note(format!("consider reborrowing: `&mut *{moved_desc}`"));
        }
        self.sink.emit(diag);
    }

    /// Whether `path` is a strict descendant of `used`: a part of it was moved.
    fn is_strict_descendant(&self, path: MovePathIndex, used: &Place) -> bool {
        let place = &self.paths.path(path).place;
        place.local == used.local
            && place.projection.len() > used.projection.len()
            && place.projection.starts_with(&used.projection)
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

    /// The moves of `moved` or of one of its ancestors that reach `use_location`, by a
    /// backward search through predecessors that stops at moves and re-initialisations of
    /// those paths. Forward edges are explored before back edges, as in rustc, so a move
    /// reachable both ways is attributed to the current iteration.
    fn find_move_sites(&self, use_location: Location, moved: MovePathIndex) -> Vec<MoveSite> {
        let mut targets = vec![moved];
        targets.extend(self.paths.ancestors(moved));

        let mut sites = Vec::new();
        let mut visited = HashSet::new();
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
        if a.block == b.block {
            a.statement_index <= b.statement_index
        } else {
            self.dominators[b.block.index()].contains(a.block.index())
        }
    }

    fn span_of(&self, location: Location) -> Span {
        let data = self.body.block(location.block);
        data.statements
            .get(location.statement_index)
            .map_or_else(|| data.terminator().span, |s| s.span)
    }

    fn describe(&self, place: &Place) -> String {
        describe_place(place, self.body, self.typeck)
    }
}
