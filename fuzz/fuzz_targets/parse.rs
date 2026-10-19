//! The parser must never panic, and the pretty printer must be a fixpoint on anything
//! that parses without errors. Run with `cargo +nightly fuzz run parse` from `fuzz/`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use sabiko_diagnostics::DiagnosticSink;
use sabiko_span::FileId;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    let mut sink = DiagnosticSink::new();
    let file = sabiko_parser::parse_file(FileId(0), src, &mut sink);
    if !sink.has_errors() {
        let printed = sabiko_ast::pretty::pretty(&file);
        let mut sink2 = DiagnosticSink::new();
        let reparsed = sabiko_parser::parse_file(FileId(0), &printed, &mut sink2);
        assert!(!sink2.has_errors(), "pretty output failed to parse:\n{printed}");
        assert_eq!(sabiko_ast::pretty::pretty(&reparsed), printed, "pretty is not a fixpoint");
    }
});
