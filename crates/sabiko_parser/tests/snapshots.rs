//! Snapshot tests: every `fixtures/*.sabi` is parsed and its structural dump plus any
//! diagnostics are compared against `snapshots/`. Review changes with `cargo insta review`
//! (or set `INSTA_UPDATE=always` once to accept them).

use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};

#[test]
fn fixtures() {
    insta::glob!("fixtures/*.sabi", |path| {
        let src = std::fs::read_to_string(path).unwrap();
        let mut sm = SourceMap::new();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let id = sm.add(name, src.clone());
        let mut sink = DiagnosticSink::new();
        let file = sabiko_parser::parse_file(id, &src, &mut sink);
        let mut out = sabiko_ast::dump::dump(&file);
        let mut diags = sink.into_vec();
        sabiko_diagnostics::sort(&mut diags);
        if !diags.is_empty() {
            out.push_str("--- diagnostics ---\n");
            out.push_str(&sabiko_diagnostics::render(&diags, &sm, Format::HumanPlain));
        }
        insta::assert_snapshot!(out);
    });
}
