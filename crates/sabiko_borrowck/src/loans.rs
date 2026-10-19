//! Loan checking. Placeholder; filled in during Phase 3.

use sabiko_diagnostics::DiagnosticSink;
use sabiko_mir::Body;
use sabiko_resolve::ResolveResults;
use sabiko_types::TypeckResults;

/// Reports conflicting borrows and references that outlive their referents.
pub fn check_loans(
    _body: &Body,
    _resolve: &ResolveResults,
    _typeck: &TypeckResults,
    _sink: &mut DiagnosticSink,
) {
}
