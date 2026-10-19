//! Loan checking: shared XOR mutable, no mutation, move or drop of a borrowed place while
//! a loan is live, no reference outliving its referent.
//!
//! The check is the NLL formulation solved by the Polonius engine:
//! - [`regions`] assigns an origin to every reference type in the body and to the
//!   function signature (universal origins, with return-type elision);
//! - [`facts`] walks the MIR once and produces the engine's input facts: CFG, loans,
//!   subset constraints, variable definitions and uses, invalidating accesses and kills;
//! - this module runs the engine and turns `errors` (a live loan invalidated by an
//!   access) and `subset_errors` (a flow between two signature origins the signature
//!   does not declare) into diagnostics `E0408`..`E0416`.
//!
//! A loan is live at a point when some variable whose type holds an origin containing the
//! loan may still be used, so a borrow ends at its last use, not at the end of its scope.
//! `E0416` (missing lifetime specifier) is reported here too, since it is the signature's
//! origins that cannot be built; only functions that reached MIR are checked, which is
//! fine for Phase 3 because the type checker rejects the rest of the body first anyway.

pub mod facts;
pub mod regions;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use polonius_engine::{Algorithm, Output};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_mir::{Body, Local, LocalKind, Place, PlaceElem, TerminatorKind};
use sabiko_resolve::ResolveResults;
use sabiko_span::Span;
use sabiko_types::TypeckResults;

use crate::dataflow::Location;
use crate::describe_place;
use facts::{AccessKind, Facts, FactsError, LocationTable, SabikoFacts};
use regions::{Loan, Origin, Point};

/// Reports conflicting borrows and references that outlive their referents.
pub fn check_loans(
    body: &Body,
    _resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    let facts = match facts::generate(body, typeck) {
        Ok(facts) => facts,
        Err(FactsError::MissingLifetime { input_origins }) => {
            report_missing_lifetime(body, typeck, input_origins, sink);
            return;
        }
    };
    let output = Output::compute(&facts.all, Algorithm::DatafrogOpt, true);
    let mut successors: HashMap<Point, Vec<Point>> = HashMap::new();
    let mut predecessors: HashMap<Point, Vec<Point>> = HashMap::new();
    for &(from, to) in &facts.all.cfg_edge {
        successors.entry(from).or_default().push(to);
        predecessors.entry(to).or_default().push(from);
    }
    let reporter = Reporter {
        body,
        typeck,
        facts: &facts,
        output: &output,
        successors,
        predecessors,
    };
    reporter.report_errors(sink);
    reporter.report_subset_errors(sink);
}

/// E0416: the return type borrows but the signature cannot say from which parameter.
fn report_missing_lifetime(
    body: &Body,
    typeck: &TypeckResults,
    input_origins: usize,
    sink: &mut DiagnosticSink,
) {
    let sig = typeck.items.fn_sig(body.def);
    let why = if input_origins == 0 {
        "but there is no value for it to be borrowed from"
    } else {
        "but the signature does not say which one of the parameters it is borrowed from"
    };
    sink.emit(
        Diagnostic::error(
            ErrorCode::MISSING_LIFETIME_SPECIFIER,
            "missing lifetime specifier",
        )
        .with_label(sig.name_span, "expected named lifetime parameter")
        .with_note(format!(
            "this function's return type contains a borrowed value, {why}"
        ))
        .with_note("explicit lifetimes are planned for Phase 9; see docs/PLAN.md"),
    );
}

struct Reporter<'a> {
    body: &'a Body,
    typeck: &'a TypeckResults,
    facts: &'a Facts,
    output: &'a Output<SabikoFacts>,
    successors: HashMap<Point, Vec<Point>>,
    predecessors: HashMap<Point, Vec<Point>>,
}

impl Reporter<'_> {
    /// One diagnostic per (location, loan) pair in `errors`, in body order.
    fn report_errors(&self, sink: &mut DiagnosticSink) {
        let mut errors: Vec<(Location, Point, Loan)> = Vec::new();
        for (&point, loans) in &self.output.errors {
            debug_assert!(!LocationTable::is_mid(point), "invalidations are at Start");
            let location = self.facts.table.location(point);
            errors.extend(loans.iter().map(|&loan| (location, point, loan)));
        }
        errors.sort_unstable();
        errors.dedup();
        for (location, point, loan) in errors {
            if let Some(diag) = self.loan_error(location, point, loan) {
                sink.emit(diag);
            }
        }
    }

    fn loan_error(&self, location: Location, point: Point, loan: Loan) -> Option<Diagnostic> {
        let invalidation = self.facts.invalidations.get(&(point, loan))?;
        let data = &self.facts.loans[loan.index()];
        let span = self.span_at(location);
        let borrowed = self.describe(&data.place);
        let accessed = self.describe(&invalidation.place);
        let diag = match invalidation.kind {
            AccessKind::MutBorrow if data.mutability.is_mut() => Diagnostic::error(
                ErrorCode::MULTIPLE_MUT_BORROWS,
                format!("cannot borrow `{accessed}` as mutable more than once at a time"),
            )
            .with_label(span, "second mutable borrow occurs here")
            .with_label(data.span, "first mutable borrow occurs here"),
            AccessKind::MutBorrow => Diagnostic::error(
                ErrorCode::BORROW_CONFLICT,
                format!(
                    "cannot borrow `{accessed}` as mutable because it is also borrowed as immutable"
                ),
            )
            .with_label(span, "mutable borrow occurs here")
            .with_label(data.span, "immutable borrow occurs here"),
            AccessKind::SharedBorrow => Diagnostic::error(
                ErrorCode::BORROW_CONFLICT,
                format!(
                    "cannot borrow `{accessed}` as immutable because it is also borrowed as mutable"
                ),
            )
            .with_label(span, "immutable borrow occurs here")
            .with_label(data.span, "mutable borrow occurs here"),
            AccessKind::Write => Diagnostic::error(
                ErrorCode::ASSIGN_TO_BORROWED,
                format!("cannot assign to `{accessed}` because it is borrowed"),
            )
            .with_label(
                span,
                format!("assignment to borrowed `{accessed}` occurs here"),
            )
            .with_label(data.span, format!("`{borrowed}` is borrowed here")),
            AccessKind::Read => Diagnostic::error(
                ErrorCode::USE_WHILE_MUTABLY_BORROWED,
                format!("cannot use `{accessed}` because it was mutably borrowed"),
            )
            .with_label(span, format!("use of borrowed `{accessed}`"))
            .with_label(data.span, format!("`{borrowed}` is borrowed here")),
            AccessKind::Move => Diagnostic::error(
                ErrorCode::MOVE_WHILE_BORROWED,
                format!("cannot move out of `{accessed}` because it is borrowed"),
            )
            .with_label(span, format!("move out of `{accessed}` occurs here"))
            .with_label(data.span, format!("borrow of `{borrowed}` occurs here")),
            AccessKind::StorageDead | AccessKind::Return => {
                return Some(self.lifetime_error(location, point, loan, invalidation.kind));
            }
        };
        Some(self.with_later_use(diag, point, loan))
    }

    /// The borrowed local's storage ends while the loan is live: E0414 when the loan
    /// escapes through the return value, E0413 otherwise. As in rustc, the primary label
    /// is the borrow; the drop and the later use are context.
    fn lifetime_error(
        &self,
        location: Location,
        point: Point,
        loan: Loan,
        kind: AccessKind,
    ) -> Diagnostic {
        let data = &self.facts.loans[loan.index()];
        let borrowed = self.describe(&data.place);
        let root = data.place.local;
        if self.loan_in_any(point, loan, &self.facts.return_origins()) {
            let what = match (self.body.local_decl(root).kind, data.place.is_local()) {
                (LocalKind::Arg, true) => "function parameter",
                (_, true) => "local variable",
                (_, false) => "local data",
            };
            return Diagnostic::error(
                ErrorCode::RETURN_REF_TO_LOCAL,
                format!("cannot return reference to {what} `{borrowed}`"),
            )
            .with_label(
                data.span,
                "returns a reference to data owned by the current function",
            );
        }
        let local = self.describe(&Place::local(root));
        let diag = if self.body.local_decl(root).kind == LocalKind::Temp
            && kind == AccessKind::StorageDead
        {
            // The MIR builder ends a statement's temporaries with the statement.
            Diagnostic::error(
                ErrorCode::DOES_NOT_LIVE_LONG_ENOUGH,
                "temporary value dropped while borrowed",
            )
            .with_label(
                data.span,
                "creates a temporary value which is freed while still in use",
            )
            .with_label(
                drop_span(self.span_at(location)),
                "temporary value is freed at the end of this statement",
            )
        } else {
            let (span, label) = if kind == AccessKind::Return {
                (
                    self.span_at(location),
                    format!(
                        "`{local}` dropped here, when the function returns, while still borrowed"
                    ),
                )
            } else {
                (
                    drop_span(self.span_at(location)),
                    format!("`{local}` dropped here while still borrowed"),
                )
            };
            Diagnostic::error(
                ErrorCode::DOES_NOT_LIVE_LONG_ENOUGH,
                format!("`{borrowed}` does not live long enough"),
            )
            .with_label(data.span, "borrowed value does not live long enough")
            .with_label(span, label)
        };
        self.with_later_use(diag, point, loan)
    }

    /// Adds the "borrow later used here" label: the first use, after the error, of a
    /// variable whose type holds an origin that contained the loan when the error
    /// happened. A loan that only the signature keeps alive is used when the function
    /// returns.
    fn with_later_use(&self, diag: Diagnostic, point: Point, loan: Loan) -> Diagnostic {
        let holders = self.origins_containing(point, loan);
        if let Some((location, local)) = self.later_use(point, &holders) {
            let label = if local == Local::RETURN {
                "borrow later used here, when the function returns"
            } else {
                "borrow later used here"
            };
            return diag.with_label(self.span_at(location), label);
        }
        let universal: Vec<Origin> = self.facts.universal.iter().map(|u| u.origin).collect();
        if universal.iter().any(|origin| holders.contains(origin)) {
            if let Some(span) = self.return_span() {
                return diag.with_label(span, "borrow later used here, when the function returns");
            }
        }
        diag
    }

    /// The origins containing `loan` at `point`.
    fn origins_containing(&self, point: Point, loan: Loan) -> HashSet<Origin> {
        self.output
            .origin_contains_loan_at
            .get(&point)
            .map(|by_origin| {
                by_origin
                    .iter()
                    .filter(|(_, loans)| loans.contains(&loan))
                    .map(|(&origin, _)| origin)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn loan_in_any(&self, point: Point, loan: Loan, origins: &[Origin]) -> bool {
        let containing = self.origins_containing(point, loan);
        origins.iter().any(|origin| containing.contains(origin))
    }

    /// Breadth-first over the CFG from `from`: the first point where a local whose type
    /// holds one of `origins` is used.
    fn later_use(&self, from: Point, origins: &HashSet<Origin>) -> Option<(Location, Local)> {
        let mut seen = HashSet::from([from]);
        let mut queue = VecDeque::from([from]);
        while let Some(point) = queue.pop_front() {
            if let Some(locals) = self.facts.uses_at.get(&point) {
                let hit = locals.iter().copied().find(|local| {
                    self.facts.locals[local.index()]
                        .origins()
                        .iter()
                        .any(|origin| origins.contains(origin))
                });
                if let Some(local) = hit {
                    return Some((self.facts.table.location(point), local));
                }
            }
            for &next in self.successors.get(&point).into_iter().flatten() {
                if seen.insert(next) {
                    queue.push_back(next);
                }
            }
        }
        None
    }

    /// E0415: a value flows from one signature origin into another without the signature
    /// saying so. Universal origins are live everywhere, so the engine reports the
    /// relation at every point after the constraint; only the entry points of that region
    /// (the points none of whose predecessors report it) are diagnosed.
    fn report_subset_errors(&self, sink: &mut DiagnosticSink) {
        let mut by_pair: BTreeMap<(Origin, Origin), BTreeSet<Point>> = BTreeMap::new();
        for (&point, pairs) in &self.output.subset_errors {
            for &pair in pairs {
                by_pair.entry(pair).or_default().insert(point);
            }
        }
        for ((longer, shorter), points) in by_pair {
            let mut locations: Vec<Location> = points
                .iter()
                .filter(|&point| {
                    !self
                        .predecessors
                        .get(point)
                        .into_iter()
                        .flatten()
                        .any(|pred| points.contains(pred))
                })
                .map(|&point| self.facts.table.location(point))
                .collect();
            if locations.is_empty() {
                // Every reporting point has a reporting predecessor (a loop): take the
                // first in body order.
                locations.extend(points.iter().map(|&p| self.facts.table.location(p)).min());
            }
            locations.sort_unstable();
            locations.dedup();
            let label = match (self.universal_place(longer), self.universal_place(shorter)) {
                (Some(longer), Some(shorter)) => format!(
                    "requires that the data `{longer}` points to outlives the data `{shorter}` points to"
                ),
                _ => "requires that one borrowed lifetime outlives another".to_owned(),
            };
            for location in locations {
                sink.emit(
                    Diagnostic::error(
                        ErrorCode::LIFETIME_TOO_SHORT,
                        "lifetime may not live long enough",
                    )
                    .with_label(self.span_at(location), &label)
                    .with_note(
                        "the reference assigned here must not outlive the data it points to; \
                         explicit lifetimes are planned for Phase 9",
                    ),
                );
            }
        }
    }

    /// The reference in the signature with this universal origin, as a place: `x` for
    /// `x: &T`, `*x` for the inner reference of `x: &mut &T`.
    fn universal_place(&self, origin: Origin) -> Option<String> {
        let site = self.facts.universal.iter().find(|u| u.origin == origin)?;
        let place = Place {
            local: site.param,
            projection: vec![PlaceElem::Deref; site.depth],
        };
        Some(self.describe(&place))
    }

    fn describe(&self, place: &Place) -> String {
        describe_place(place, self.body, self.typeck)
    }

    fn span_at(&self, location: Location) -> Span {
        let data = self.body.block(location.block);
        data.statements
            .get(location.statement_index)
            .map_or_else(|| data.terminator().span, |statement| statement.span)
    }

    /// The span of the function's `return`, if it has one.
    fn return_span(&self) -> Option<Span> {
        self.body.blocks.iter().find_map(|data| {
            data.terminator
                .as_ref()
                .filter(|t| t.kind == TerminatorKind::Return)
                .map(|t| t.span)
        })
    }
}

/// `StorageDead` carries the span of the scope it closes; point at its last character
/// (the closing brace of a block), as rustc does for "dropped here".
fn drop_span(span: Span) -> Span {
    Span::new(
        span.file,
        span.end.saturating_sub(1).max(span.start),
        span.end,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sabiko_mir::{Local, Mutability, Place, PlaceElem, Ty};

    fn field(place: Place, index: usize) -> Place {
        place.field(index, Ty::I32)
    }

    #[test]
    fn disjoint_fields_do_not_conflict() {
        let p = Place::local(Local(1));
        assert!(!facts::places_conflict(
            &field(p.clone(), 0),
            AccessKind::MutBorrow,
            &field(p, 1),
            Mutability::Mut
        ));
    }

    #[test]
    fn prefix_conflicts() {
        let p = Place::local(Local(1));
        assert!(facts::places_conflict(
            &p,
            AccessKind::Write,
            &field(p.clone(), 1),
            Mutability::Not
        ));
        assert!(facts::places_conflict(
            &field(p.clone(), 1),
            AccessKind::Read,
            &p,
            Mutability::Mut
        ));
        assert!(!facts::places_conflict(
            &field(p.clone(), 1),
            AccessKind::Read,
            &p,
            Mutability::Not
        ));
    }

    #[test]
    fn shallow_write_does_not_reach_through_deref() {
        let r = Place::local(Local(1));
        let deref = r.clone().deref();
        assert!(!facts::places_conflict(
            &r,
            AccessKind::Write,
            &deref,
            Mutability::Mut
        ));
        assert!(!facts::places_conflict(
            &r,
            AccessKind::StorageDead,
            &deref,
            Mutability::Mut
        ));
        assert!(facts::places_conflict(
            &r,
            AccessKind::Move,
            &deref,
            Mutability::Mut
        ));
        assert!(facts::places_conflict(
            &deref,
            AccessKind::Write,
            &r,
            Mutability::Not
        ));
        assert_eq!(deref.projection, [PlaceElem::Deref]);
    }
}
