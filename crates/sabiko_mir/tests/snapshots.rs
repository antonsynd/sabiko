//! Snapshot tests: every `fixtures/*.sabi` is lowered to MIR, validated, simplified and
//! printed; the output is compared against `snapshots/`. Review changes with
//! `cargo insta review` (or set `INSTA_UPDATE=always` once to accept them).

use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};

#[test]
fn fixtures() {
    insta::glob!("fixtures/*.sabi", |path| {
        let src = std::fs::read_to_string(path).unwrap();
        let mut sm = SourceMap::new();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let id = sm.add(name, src.clone());
        let mut sink = DiagnosticSink::new();
        let ast = sabiko_parser::parse_file(id, &src, &mut sink);
        let resolve = sabiko_resolve::resolve(&ast, &mut sink);
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        assert!(
            sink.diagnostics().is_empty(),
            "fixture must be valid:\n{}",
            sabiko_diagnostics::render(sink.diagnostics(), &sm, Format::HumanPlain)
        );
        let mut bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(errors.is_empty(), "invalid MIR after lowering: {errors:#?}");
        sabiko_mir::simplify::simplify_all(&mut bodies);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "invalid MIR after simplification: {errors:#?}"
        );
        assert!(sink.diagnostics().is_empty(), "lowering never reports");
        insta::assert_snapshot!(sabiko_mir::pretty::print_bodies(&bodies, &resolve, &typeck));
    });
}
