//! IL golden tests: every `fixtures/*.sabi` is compiled through the whole pipeline and the
//! emitted IL is compared against `snapshots/`. Review changes with `cargo insta review`
//! (or set `INSTA_UPDATE=always` once to accept them). The run-pass tests in `sabikoc`
//! assemble, verify and execute programs; these snapshots keep the instruction selection
//! reviewable per construct.

use sabiko_codegen::{Options, OutputKind};
use sabiko_diagnostics::{DiagnosticSink, Format, SourceMap};

fn lower(path: &std::path::Path, overflow_checks: bool) -> String {
    let src = std::fs::read_to_string(path).unwrap();
    let mut sm = SourceMap::new();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let id = sm.add(name, src.clone());
    let mut sink = DiagnosticSink::new();
    let ast = sabiko_parser::parse_file(id, &src, &mut sink);
    let resolve = sabiko_resolve::resolve(&ast, &mut sink);
    let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
    let mut bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
    sabiko_mir::simplify::simplify_all(&mut bodies);
    sabiko_borrowck::check_all(&bodies, &resolve, &typeck, &mut sink);
    assert!(
        !sink.has_errors(),
        "fixture must be valid:\n{}",
        sabiko_diagnostics::render(sink.diagnostics(), &sm, Format::HumanPlain)
    );
    let has_entry = bodies
        .iter()
        .any(|b| b.name == "Main" && b.arg_count == 0 && b.return_ty().is_unit());
    let options = Options {
        assembly_name: stem,
        kind: if has_entry {
            OutputKind::Executable
        } else {
            OutputKind::Library
        },
        overflow_checks,
    };
    let il = sabiko_codegen::emit_il(&bodies, &resolve, &typeck, &options, &mut sink);
    assert!(
        !sink.has_errors(),
        "code generation reported errors:\n{}",
        sabiko_diagnostics::render(sink.diagnostics(), &sm, Format::HumanPlain)
    );
    il.expect("IL emitted")
}

#[test]
fn fixtures_debug() {
    insta::glob!("fixtures/*.sabi", |path| {
        insta::assert_snapshot!(lower(path, true));
    });
}

/// Release mode differs only in arithmetic; one fixture covers it.
#[test]
fn arith_release() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/arith.sabi");
    insta::assert_snapshot!("arith_release", lower(&path, false));
}
