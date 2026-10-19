//! Borrow checking for Sabiko MIR.
//!
//! Phase 2 ships the two checks that need no lifetimes: mutability (writes and `&mut`
//! borrows must go to mutable places) and initialisation (reads must follow a write on
//! every path; an immutable binding is written at most once). Both run on the MIR of one
//! function at a time and report through the shared diagnostics sink.
//!
//! Module map:
//! - [`dataflow`]: a generic forward dataflow framework over bitsets, with a worklist
//!   fixpoint and a cursor to replay states inside a block.
//! - [`init`]: the "maybe" and "definitely" initialised analyses built on it.
//! - [`mutability`]: place mutability classification.
//! - `check`: the walk over statements and terminators that turns the above into
//!   diagnostics (`E0400`..`E0405`).

pub mod dataflow;
pub mod init;
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
