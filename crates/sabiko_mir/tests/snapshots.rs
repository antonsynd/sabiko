//! Snapshot tests: every `fixtures/*.sabi` is lowered to MIR, validated, simplified and
//! printed; the output is compared against `snapshots/`. Review changes with
//! `cargo insta review` (or set `INSTA_UPDATE=always` once to accept them).

use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};
use sabiko_span::FileId;

/// Parses a fixture with the `core` prelude after it and resolves both, as the driver
/// does (decision 90).
fn parse_and_resolve(
    id: FileId,
    src: &str,
    sm: &mut SourceMap,
    sink: &mut DiagnosticSink,
) -> (sabiko_ast::SourceFile, sabiko_resolve::ResolveResults) {
    let options = sabiko_parser::ParseOptions::default();
    let (mut ast, next) = sabiko_parser::parse_file_with(id, src, options, sink);
    let start = sabiko_parser::prelude::add_core_prelude(&mut ast, next, sm, sink);
    let resolve = sabiko_resolve::resolve_program(&ast, start, sink);
    (ast, resolve)
}

/// The MIR of the fixture's own functions and their instances, without the prelude's.
fn print_user_bodies(
    bodies: &[sabiko_mir::Body],
    resolve: &sabiko_resolve::ResolveResults,
    typeck: &sabiko_types::TypeckResults,
) -> String {
    let user: Vec<sabiko_mir::Body> = bodies
        .iter()
        .filter(|b| !resolve.is_prelude(b.def))
        .cloned()
        .collect();
    sabiko_mir::pretty::print_bodies(&user, resolve, typeck)
}

#[test]
fn fixtures() {
    insta::glob!("fixtures/*.sabi", |path| {
        let src = std::fs::read_to_string(path).unwrap();
        let mut sm = SourceMap::new();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let id = sm.add(name, src.clone());
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = parse_and_resolve(id, &src, &mut sm, &mut sink);
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
        insta::assert_snapshot!(print_user_bodies(&bodies, &resolve, &typeck));
    });
}

/// The same fixtures after monomorphisation (decision 82), drop elaboration (decision 55),
/// simplification and copy forwarding (decision 51), which the driver runs after borrow
/// checking and before code generation (it then also strips the storage markers).
#[test]
fn fixtures_forwarded() {
    insta::glob!("fixtures/*.sabi", |path| {
        let src = std::fs::read_to_string(path).unwrap();
        let mut sm = SourceMap::new();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let id = sm.add(name, src.clone());
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = parse_and_resolve(id, &src, &mut sm, &mut sink);
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        let bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        let mut bodies = sabiko_mir::mono::monomorphize(bodies, &resolve, &typeck, &mut sink);
        sabiko_mir::elaborate::elaborate_all(&mut bodies, &typeck);
        sabiko_mir::simplify::simplify_all(&mut bodies);
        sabiko_mir::forward::forward_all(&mut bodies);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "invalid MIR after forwarding: {errors:#?}"
        );
        insta::assert_snapshot!(print_user_bodies(&bodies, &resolve, &typeck));
    });
}

/// The drop fixtures after drop elaboration and simplification (`--emit=mir-elaborated`):
/// `Drop` terminators only where a value is certainly initialised, flags elsewhere, and
/// the cleanup scopes' drops guarded by the same flags (decision 55).
#[test]
fn fixtures_elaborated() {
    insta::glob!("fixtures/drop*.sabi", |path| {
        let src = std::fs::read_to_string(path).unwrap();
        let mut sm = SourceMap::new();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let id = sm.add(name, src.clone());
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = parse_and_resolve(id, &src, &mut sm, &mut sink);
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        assert!(sink.diagnostics().is_empty(), "fixture must be valid");
        let bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        let mut bodies = sabiko_mir::mono::monomorphize(bodies, &resolve, &typeck, &mut sink);
        sabiko_mir::elaborate::elaborate_all(&mut bodies, &typeck);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "invalid MIR after elaboration: {errors:#?}"
        );
        sabiko_mir::simplify::simplify_all(&mut bodies);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "invalid MIR after simplification: {errors:#?}"
        );
        insta::assert_snapshot!(print_user_bodies(&bodies, &resolve, &typeck));
    });
}
