//! Borrow checking for Sabiko MIR.
//!
//! Four checks run on the MIR of one function at a time and report through the shared
//! diagnostics sink:
//! - mutability: writes and `&mut` borrows must go to mutable places;
//! - initialisation: reads follow a write on every path, an immutable binding is written
//!   at most once;
//! - moves: a value is used at most once after it is moved, moves out of references are
//!   rejected;
//! - loans: shared XOR mutable, no mutation or move of a borrowed place while a loan is
//!   live, no reference outlives what it points to. Liveness and conflicts are solved by
//!   the Polonius engine from facts generated here.
//!
//! Module map:
//! - [`dataflow`]: a generic forward dataflow framework over bitsets, with a worklist
//!   fixpoint and a cursor to replay states inside a block.
//! - [`init`]: the "maybe" and "definitely" initialised analyses built on it.
//! - [`mutability`]: place mutability classification.
//! - `check`: the walk that turns the above into diagnostics (`E0400`..`E0405`).
//! - [`moves`]: move paths and the maybe-moved analysis (`E0406`, `E0407`).
//! - [`loans`]: regions, Polonius facts and loan diagnostics (`E0408`..`E0416`).

pub mod dataflow;
pub mod init;
pub mod loans;
pub mod moves;
pub mod mutability;

mod check;

pub use check::describe_place;
use sabiko_diagnostics::DiagnosticSink;
use sabiko_mir::Body;
use sabiko_resolve::ResolveResults;
use sabiko_types::TypeckResults;

/// Checks one body.
pub fn check_body(
    body: &Body,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    check::check_body(body, resolve, typeck, sink);
    moves::check_moves(body, resolve, typeck, sink);
    loans::check_loans(body, resolve, typeck, sink);
}

/// Checks every body, in order.
pub fn check_all(
    bodies: &[Body],
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) {
    for body in bodies {
        check_body(body, resolve, typeck, sink);
    }
}
