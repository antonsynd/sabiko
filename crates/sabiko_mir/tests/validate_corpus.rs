//! The validator accepts every program in the test corpus that type-checks, before and
//! after simplification, and lowering upholds a few structural properties.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sabiko_diagnostics::DiagnosticSink;
use sabiko_mir::{Body, StatementKind};
use sabiko_span::FileId;

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
}

fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in [
        root.join("tests/fixtures"),
        root.join("../sabiko_parser/tests/fixtures"),
        root.join("../../tests/ui"),
    ] {
        collect(&dir, &mut files);
    }
    files.sort();
    files
}

/// Lowers a file, or returns `None` when any front-end stage reported a diagnostic.
fn lower(src: &str) -> Option<(Vec<Body>, sabiko_types::TypeckResults)> {
    let mut sink = DiagnosticSink::new();
    let ast = sabiko_parser::parse_file(FileId(0), src, &mut sink);
    if sink.has_errors() {
        return None;
    }
    let resolve = sabiko_resolve::resolve(&ast, &mut sink);
    if sink.has_errors() {
        return None;
    }
    let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
    if !sink.diagnostics().is_empty() {
        return None;
    }
    let bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
    assert!(sink.diagnostics().is_empty(), "lowering never reports");
    Some((bodies, typeck))
}

#[test]
fn validator_accepts_corpus() {
    let mut lowered = 0;
    for path in corpus() {
        let src = std::fs::read_to_string(&path).unwrap();
        let Some((mut bodies, typeck)) = lower(&src) else {
            continue;
        };
        lowered += bodies.len();
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "{}: invalid MIR after lowering: {errors:#?}",
            path.display()
        );
        sabiko_mir::simplify::simplify_all(&mut bodies);
        let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
        assert!(
            errors.is_empty(),
            "{}: invalid MIR after simplification: {errors:#?}",
            path.display()
        );
    }
    assert!(
        lowered > 10,
        "only {lowered} bodies lowered; corpus missing?"
    );
}

#[test]
fn structural_properties() {
    let mut checked = 0;
    for path in corpus() {
        let src = std::fs::read_to_string(&path).unwrap();
        let Some((bodies, _)) = lower(&src) else {
            continue;
        };
        for body in &bodies {
            checked += 1;
            let what = format!("{}::{}", path.display(), body.name);
            assert!(!body.blocks.is_empty(), "{what}: no bb0");
            let mut live = HashSet::new();
            let mut dead = HashSet::new();
            for (i, block) in body.blocks.iter().enumerate() {
                assert!(
                    block.terminator.is_some(),
                    "{what}: bb{i} has no terminator"
                );
                for stmt in &block.statements {
                    match stmt.kind {
                        StatementKind::StorageLive(l) => {
                            live.insert(l);
                        }
                        StatementKind::StorageDead(l) => {
                            dead.insert(l);
                        }
                        _ => {}
                    }
                }
            }
            for local in &live {
                assert!(
                    dead.contains(local),
                    "{what}: StorageLive(_{}) without StorageDead",
                    local.0
                );
            }
            // Arguments and the return place never get storage markers.
            for local in live.iter().chain(&dead) {
                assert!(
                    local.index() > body.arg_count,
                    "{what}: storage marker on _{}",
                    local.0
                );
            }
        }
    }
    assert!(
        checked > 10,
        "only {checked} bodies checked; corpus missing?"
    );
}
