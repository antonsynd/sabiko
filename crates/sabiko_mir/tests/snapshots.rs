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
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = sabiko_driver::parse_and_resolve(&name, &src, &mut sm, &mut sink)
            .program
            .expect("fixture parses");
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
        insta::assert_snapshot!(sabiko_mir::pretty::print_user_bodies(
            &bodies, &resolve, &typeck
        ));
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
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = sabiko_driver::parse_and_resolve(&name, &src, &mut sm, &mut sink)
            .program
            .expect("fixture parses");
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        let bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        let mut bodies = sabiko_mir::mono::monomorphize(bodies, &ast, &resolve, &typeck, &mut sink);
        sabiko_mir::elaborate::elaborate_all(&mut bodies, &typeck);
        sabiko_mir::simplify::simplify_all(&mut bodies);
        sabiko_mir::forward::forward_all(&mut bodies);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "invalid MIR after forwarding: {errors:#?}"
        );
        insta::assert_snapshot!(sabiko_mir::pretty::print_user_bodies(
            &bodies, &resolve, &typeck
        ));
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
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = sabiko_driver::parse_and_resolve(&name, &src, &mut sm, &mut sink)
            .program
            .expect("fixture parses");
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        assert!(sink.diagnostics().is_empty(), "fixture must be valid");
        let bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        let mut bodies = sabiko_mir::mono::monomorphize(bodies, &ast, &resolve, &typeck, &mut sink);
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
        insta::assert_snapshot!(sabiko_mir::pretty::print_user_bodies(
            &bodies, &resolve, &typeck
        ));
    });
}

/// The fixtures on reads of uninhabited values after every pass before code generation
/// (`lower_for_codegen`): each block that reads a value of type `!` ends there, its
/// indexed reads up to it mentioned, and a two-phase borrow whose call is gone becomes a
/// plain one (decision 103).
#[test]
fn fixtures_for_codegen() {
    insta::glob!("fixtures/uninhabited*.sabi", |path| {
        let src = std::fs::read_to_string(path).unwrap();
        let mut sm = SourceMap::new();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut sink = DiagnosticSink::new();
        let (ast, resolve) = sabiko_driver::parse_and_resolve(&name, &src, &mut sm, &mut sink)
            .program
            .expect("fixture parses");
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        assert!(sink.diagnostics().is_empty(), "fixture must be valid");
        let bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
        let mut bodies = sabiko_mir::mono::monomorphize(bodies, &ast, &resolve, &typeck, &mut sink);
        // Without the pass, the check that no uninhabited operand is left fails.
        let mut uncut = bodies.clone();
        sabiko_mir::elaborate::elaborate_all(&mut uncut, &typeck);
        sabiko_mir::simplify::simplify_all(&mut uncut);
        sabiko_mir::forward::forward_all(&mut uncut);
        assert!(!sabiko_mir::validate::uninhabited_operands(&uncut).is_empty());
        let errors = sabiko_mir::lower_for_codegen(&mut bodies, &typeck);
        assert!(
            errors.is_empty(),
            "invalid MIR for code generation: {errors:#?}"
        );
        insta::assert_snapshot!(sabiko_mir::pretty::print_user_bodies(
            &bodies, &resolve, &typeck
        ));
    });
}
