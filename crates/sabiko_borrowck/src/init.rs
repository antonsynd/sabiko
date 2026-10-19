//! Initialisation analysis: which locals hold a value at each point of a body.
//!
//! Two instances of one transfer function, differing only in how paths merge:
//! "maybe initialised" (union at joins) and "definitely initialised" (intersection).
//! Reading a local that is not definitely initialised is an error; whether it is maybe
//! initialised only changes the wording. Assigning to an immutable local that is maybe
//! initialised is the "assign twice" error.
//!
//! Only user bindings are tracked. Arguments hold a value on entry and have no storage
//! markers, and the return place and temporaries are always treated as initialised: the
//! lowering guarantees their uses are well-formed.

use sabiko_mir::{
    Body, Local, LocalKind, Place, Statement, StatementKind, Terminator, TerminatorKind,
};

use crate::dataflow::{Analysis, BitSet, Location};

/// Which join the analysis uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitKind {
    /// Initialised on some path reaching the point (union at joins).
    Maybe,
    /// Initialised on every path reaching the point (intersection at joins).
    Definite,
}

/// Forward "initialised locals" analysis; see the module docs.
#[derive(Clone, Debug)]
pub struct InitAnalysis {
    pub kind: InitKind,
    /// The user bindings; every other local is initialised throughout.
    tracked: BitSet,
}

impl InitAnalysis {
    #[must_use]
    pub fn new(kind: InitKind, body: &Body) -> Self {
        let mut tracked = BitSet::new_empty(body.locals.len());
        for (index, decl) in body.locals.iter().enumerate() {
            if decl.kind == LocalKind::User {
                tracked.insert(index);
            }
        }
        Self { kind, tracked }
    }

    /// Whether the analysis tracks `local`; untracked locals are always initialised.
    #[must_use]
    pub fn is_tracked(&self, local: Local) -> bool {
        self.tracked.contains(local.index())
    }

    /// The place written by an assignment initialises its local only when it is the bare
    /// local: writing a field or through a deref leaves the root as it was.
    fn initialised_by_write(place: &Place) -> Option<Local> {
        place.is_local().then_some(place.local)
    }
}

impl Analysis for InitAnalysis {
    fn bottom_value(&self, body: &Body) -> BitSet {
        match self.kind {
            InitKind::Maybe => BitSet::new_empty(body.locals.len()),
            InitKind::Definite => BitSet::new_filled(body.locals.len()),
        }
    }

    fn initialize_start_block(&self, _: &Body, state: &mut BitSet) {
        state.insert_all();
        state.subtract(&self.tracked);
    }

    fn join(&self, into: &mut BitSet, from: &BitSet) -> bool {
        match self.kind {
            InitKind::Maybe => into.union_with(from),
            InitKind::Definite => into.intersect_with(from),
        }
    }

    fn apply_statement(&self, state: &mut BitSet, statement: &Statement, _: Location) {
        match &statement.kind {
            StatementKind::Assign(place, _) => {
                if let Some(local) = Self::initialised_by_write(place) {
                    state.insert(local.index());
                }
            }
            // Storage markers delimit a scope; re-entering one (a loop body) must start
            // the local out uninitialised again, so both ends kill.
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                if self.is_tracked(*local) {
                    state.remove(local.index());
                }
            }
            StatementKind::Nop => {}
        }
    }

    fn apply_terminator(&self, state: &mut BitSet, terminator: &Terminator, _: Location) {
        if let TerminatorKind::Call { destination, .. } = &terminator.kind {
            if let Some(local) = Self::initialised_by_write(destination) {
                state.insert(local.index());
            }
        }
    }
}
