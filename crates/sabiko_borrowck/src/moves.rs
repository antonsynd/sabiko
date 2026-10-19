//! Move checking. Placeholder; filled in during Phase 3.

use sabiko_diagnostics::DiagnosticSink;
use sabiko_mir::Body;
use sabiko_resolve::ResolveResults;
use sabiko_types::TypeckResults;

/// Reports uses of moved values and moves out of references.
pub fn check_moves(
    _body: &Body,
    _resolve: &ResolveResults,
    _typeck: &TypeckResults,
    _sink: &mut DiagnosticSink,
) {
}
