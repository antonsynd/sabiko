//! The per-body walk that applies the mutability and initialisation rules.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_mir::{
    Body, Local, LocalKind, Mutability, Operand, Place, PlaceElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, Ty,
};
use sabiko_resolve::ResolveResults;
use sabiko_span::Span;
use sabiko_types::TypeckResults;

use crate::dataflow::{self, Location, ResultsCursor};
use crate::init::{InitAnalysis, InitKind};
use crate::mutability::{place_mutability, PlaceMutability};

/// Runs every check over `body`, emitting diagnostics to `sink`.
pub fn check_body(
    body: &Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    let maybe_analysis = InitAnalysis::new(InitKind::Maybe, body);
    let definite_analysis = InitAnalysis::new(InitKind::Definite, body);
    let maybe_results = dataflow::iterate_to_fixpoint(body, &maybe_analysis);
    let definite_results = dataflow::iterate_to_fixpoint(body, &definite_analysis);
    let mut checker = Checker {
        body,
        resolve,
        typeck,
        sink,
        maybe_init: ResultsCursor::new(body, &maybe_analysis, &maybe_results),
        definitely_init: ResultsCursor::new(body, &definite_analysis, &definite_results),
        first_assignment: first_assignments(body),
        reported_uninit: HashSet::new(),
    };
    for bb in body.block_ids() {
        let data = body.block(bb);
        for (index, statement) in data.statements.iter().enumerate() {
            checker.seek(Location::new(bb, index));
            checker.check_statement(statement);
        }
        if let Some(terminator) = &data.terminator {
            checker.seek(Location::terminator_of(body, bb));
            checker.check_terminator(terminator);
        }
    }
}

/// The span of the earliest whole-local write to each local, in block order. Used for
/// the "first assignment" label of the assign-twice error.
fn first_assignments(body: &Body) -> HashMap<Local, Span> {
    let mut first = HashMap::new();
    for data in &body.blocks {
        for statement in &data.statements {
            if let StatementKind::Assign(place, _) = &statement.kind {
                if place.is_local() {
                    first.entry(place.local).or_insert(statement.span);
                }
            }
        }
        if let Some(Terminator {
            kind: TerminatorKind::Call { destination, .. },
            span,
        }) = &data.terminator
        {
            if destination.is_local() {
                first.entry(destination.local).or_insert(*span);
            }
        }
    }
    first
}

struct Checker<'a> {
    body: &'a Body,
    resolve: &'a ResolveResults,
    typeck: &'a TypeckResults,
    sink: &'a mut DiagnosticSink,
    maybe_init: ResultsCursor<'a, InitAnalysis>,
    definitely_init: ResultsCursor<'a, InitAnalysis>,
    first_assignment: HashMap<Local, Span>,
    /// Locals already reported as used while uninitialised; reported once per body so a
    /// single missing `let x = ..` does not cascade into an error at every use.
    reported_uninit: HashSet<Local>,
}

impl Checker<'_> {
    fn seek(&mut self, location: Location) {
        self.maybe_init.seek_before(location);
        self.definitely_init.seek_before(location);
    }

    fn check_statement(&mut self, statement: &Statement) {
        match &statement.kind {
            StatementKind::Assign(place, rvalue) => {
                self.check_rvalue(rvalue, statement.span);
                self.check_write(place, statement.span);
            }
            // `let _ = place;` needs no initialised or mutable place: it touches nothing.
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::PlaceMention(_)
            | StatementKind::Nop => {}
        }
    }

    fn check_terminator(&mut self, terminator: &Terminator) {
        match &terminator.kind {
            TerminatorKind::SwitchInt { discr, .. } => self.check_operand(discr, terminator.span),
            TerminatorKind::Call {
                args, destination, ..
            } => {
                for arg in args {
                    self.check_operand(arg, terminator.span);
                }
                self.check_write(destination, terminator.span);
            }
            // A drop needs neither an initialised place (it drops what is there) nor a
            // `mut` binding, although `drop` takes `&mut self`.
            TerminatorKind::Drop { .. }
            | TerminatorKind::Goto { .. }
            | TerminatorKind::Return
            | TerminatorKind::Unreachable => {}
        }
    }

    fn check_rvalue(&mut self, rvalue: &Rvalue, span: Span) {
        match rvalue {
            Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => {
                self.check_operand(op, span);
            }
            Rvalue::BinaryOp(_, lhs, rhs) => {
                self.check_operand(lhs, span);
                self.check_operand(rhs, span);
            }
            Rvalue::Aggregate(_, fields) => {
                for field in fields {
                    self.check_operand(field, span);
                }
            }
            Rvalue::Ref(kind, place) => {
                self.check_read(place, span);
                if kind.mutability() == Mutability::Mut {
                    self.check_borrow_mut(place, span);
                }
            }
        }
    }

    fn check_operand(&mut self, operand: &Operand, span: Span) {
        if let Some(place) = operand.place() {
            self.check_read(place, span);
        }
    }

    /// A read of `place`, or of the base of a projected place being written: the root
    /// local must be definitely initialised.
    fn check_read(&mut self, place: &Place, span: Span) {
        let local = place.local;
        if self.body.local_decl(local).kind != LocalKind::User
            || self.reported_uninit.contains(&local)
            || self.definitely_init.get().contains(local.index())
        {
            return;
        }
        self.reported_uninit.insert(local);
        let name = self.local_name(local);
        let (message, label) = if self.maybe_init.get().contains(local.index()) {
            (
                format!("used binding `{name}` is possibly-uninitialized"),
                format!("`{name}` used here but it is possibly-uninitialized"),
            )
        } else {
            (
                format!("used binding `{name}` isn't initialized"),
                format!("`{name}` used here but it isn't initialized"),
            )
        };
        self.sink.emit(
            Diagnostic::error(ErrorCode::USE_OF_UNINITIALIZED, message)
                .with_label(span, label)
                .with_label(
                    self.decl_span(local),
                    "binding declared here but left uninitialized",
                ),
        );
    }

    /// `place = ..`, from an assignment or a call destination.
    fn check_write(&mut self, place: &Place, span: Span) {
        if place.is_local() {
            self.check_whole_local_write(place.local, span);
            return;
        }
        // Writing a field or through a reference reads the root (partial initialisation of
        // a struct binding is not initialisation).
        self.check_read(place, span);
        match place_mutability(place, self.body) {
            PlaceMutability::Mutable => {}
            PlaceMutability::ImmutableBinding(root) => {
                let root_name = self.local_name(root);
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::ASSIGN_TO_IMMUTABLE,
                        format!(
                            "cannot assign to `{}`, as `{root_name}` is not declared as mutable",
                            self.describe(place)
                        ),
                    )
                    .with_label(span, "cannot assign")
                    .with_label(self.decl_span(root), self.make_mutable_help(root)),
                );
            }
            PlaceMutability::BehindSharedRef(reference) => {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::ASSIGN_THROUGH_SHARED_REF,
                        format!(
                            "cannot assign to `{}`, which is behind a `&` reference",
                            self.describe(place)
                        ),
                    )
                    .with_label(
                        span,
                        format!(
                            "`{}` is a `&` reference, so the data it refers to cannot be written",
                            self.describe(&reference)
                        ),
                    ),
                );
            }
        }
    }

    /// `local = ..`: fine for mutable bindings, temporaries and the return place; for an
    /// immutable binding only as its (deferred) initialisation.
    fn check_whole_local_write(&mut self, local: Local, span: Span) {
        let decl = self.body.local_decl(local);
        let immutable_binding = matches!(decl.kind, LocalKind::Arg | LocalKind::User)
            && decl.mutability == Mutability::Not;
        if !immutable_binding || !self.maybe_init.get().contains(local.index()) {
            return;
        }
        let name = self.local_name(local);
        let mut diag = Diagnostic::error(
            ErrorCode::ASSIGN_TWICE_IMMUTABLE,
            format!("cannot assign twice to immutable variable `{name}`"),
        )
        .with_label(span, "cannot assign twice to immutable variable");
        // Arguments are initialised by the call, so there is no first assignment to
        // point at. A loop assigning once is its own first assignment; the primary label
        // already covers it.
        if let Some(&first) = self.first_assignment.get(&local) {
            if first != span {
                diag = diag.with_label(first, format!("first assignment to `{name}`"));
            }
        }
        let help = format!("help: consider making this binding mutable: `mut {name}`");
        self.sink.emit(diag.with_label(self.decl_span(local), help));
    }

    /// `&mut place`.
    fn check_borrow_mut(&mut self, place: &Place, span: Span) {
        match place_mutability(place, self.body) {
            PlaceMutability::Mutable => {}
            PlaceMutability::ImmutableBinding(root) => {
                let root_name = self.local_name(root);
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::BORROW_MUT_IMMUTABLE,
                        format!(
                            "cannot borrow `{}` as mutable, as `{root_name}` is not declared as mutable",
                            self.describe(place)
                        ),
                    )
                    .with_label(span, "cannot borrow as mutable")
                    .with_label(self.decl_span(root), self.make_mutable_help(root)),
                );
            }
            PlaceMutability::BehindSharedRef(reference) => {
                self.sink.emit(
                    Diagnostic::error(
                        ErrorCode::BORROW_MUT_THROUGH_SHARED_REF,
                        format!(
                            "cannot borrow `{}` as mutable, as it is behind a `&` reference",
                            self.describe(place)
                        ),
                    )
                    .with_label(
                        span,
                        format!(
                            "`{}` is a `&` reference, so the data it refers to cannot be borrowed as mutable",
                            self.describe(&reference)
                        ),
                    ),
                );
            }
        }
    }

    fn make_mutable_help(&self, local: Local) -> String {
        format!(
            "help: consider changing this to be mutable: `mut {}`",
            self.local_name(local)
        )
    }

    fn describe(&self, place: &Place) -> String {
        describe_place(place, self.body, self.typeck)
    }

    /// The user-facing name of a local: its binding's name, or `_N` for temporaries.
    fn local_name(&self, local: Local) -> String {
        let decl = self.body.local_decl(local);
        decl.name
            .clone()
            .or_else(|| decl.source.map(|id| self.resolve.local(id).name.clone()))
            .unwrap_or_else(|| format!("_{}", local.0))
    }

    /// Where the local was declared: the binding's name span when the resolver knows it,
    /// so that "`mut x`" help points at the identifier; otherwise the MIR declaration span.
    fn decl_span(&self, local: Local) -> Span {
        let decl = self.body.local_decl(local);
        decl.source
            .map_or(decl.span, |id| self.resolve.local(id).span)
    }
}

/// Renders a place the way a user wrote it: locals by name (`_N` for temporaries),
/// fields by name, and derefs as `*` except directly before a field access, where
/// auto-deref hides them (`(*p).X` prints as `p.X`, as in rustc).
#[must_use]
pub fn describe_place(place: &Place, body: &Body, typeck: &TypeckResults) -> String {
    let decl = body.local_decl(place.local);
    let mut out = match (&decl.kind, &decl.name) {
        (LocalKind::User | LocalKind::Arg, Some(name)) => name.clone(),
        _ => format!("_{}", place.local.0),
    };
    let mut ty = decl.ty.clone();
    for (index, elem) in place.projection.iter().enumerate() {
        match elem {
            PlaceElem::Deref => {
                let followed_by_field =
                    matches!(place.projection.get(index + 1), Some(PlaceElem::Field(..)));
                if !followed_by_field {
                    out.insert(0, '*');
                }
                ty = match ty {
                    Ty::Ref(_, inner) => *inner,
                    other => other,
                };
            }
            PlaceElem::Field(field, field_ty) => {
                let name = match &ty {
                    Ty::Adt(def) => typeck
                        .items
                        .adt(*def)
                        .fields
                        .get(*field)
                        .map_or_else(|| field.to_string(), |f| f.name.clone()),
                    _ => field.to_string(),
                };
                let _ = write!(out, ".{name}");
                ty = field_ty.clone();
            }
        }
    }
    out
}
