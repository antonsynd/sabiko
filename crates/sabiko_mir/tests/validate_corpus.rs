//! The validator accepts every program in the test corpus that type-checks, before and
//! after simplification, and lowering upholds a few structural properties.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sabiko_diagnostics::DiagnosticSink;
use sabiko_mir::{
    AggregateKind, Body, Constant, LocalKind, Operand, Place, PlaceElem, Rvalue, Statement,
    StatementKind, TerminatorKind,
};
use sabiko_span::FileId;
use sabiko_types::{ConstValue, Ty};

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
        // Drop elaboration, copy forwarding and the rest, as before code generation.
        let errors = sabiko_mir::lower_for_codegen(&mut bodies, &typeck);
        assert!(
            errors.is_empty(),
            "{}: invalid MIR before code generation: {errors:#?}",
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

/// The validator rejects a `switchInt` on a non-integer discriminant: retyping the
/// discriminant of `if b { .. }` from `bool` to `f64` must be reported.
#[test]
fn validator_rejects_switch_on_float() {
    let (mut bodies, typeck) =
        lower("fn F(b: bool) -> i32 { if b { 1 } else { 2 } }").expect("valid program");
    assert!(sabiko_mir::validate::validate_all(&bodies, &typeck).is_empty());
    let body = &mut bodies[0];
    let discr = body
        .blocks
        .iter()
        .find_map(|b| match &b.terminator.as_ref()?.kind {
            TerminatorKind::SwitchInt {
                discr: Operand::Copy(p) | Operand::Move(p),
                ..
            } => Some(p.local),
            _ => None,
        })
        .expect("an `if` lowers to a switchInt on a place");
    body.locals[discr.index()].ty = sabiko_types::Ty::F64;
    let errors = sabiko_mir::validate::validate_all(&bodies, &typeck);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("switchInt on non-integer type")),
        "{errors:#?}"
    );
}

/// A two-phase borrow must go into a temporary that exactly one call reads: the borrow
/// checker activates it at that use. Another use, or a user local as the destination, is
/// reported.
#[test]
fn validator_rejects_misused_two_phase_borrow() {
    let src = "struct P { X: i32 }\nimpl P { fn Inc(&mut self) { self.X += 1; } }\nfn F(mut p: P) { p.Inc(); }";
    let (bodies, typeck) = lower(src).expect("valid program");
    assert!(sabiko_mir::validate::validate_all(&bodies, &typeck).is_empty());
    let f = bodies.iter().position(|b| b.name == "F").expect("F");
    let (bb, index, temp) = bodies[f]
        .blocks
        .iter()
        .enumerate()
        .find_map(|(bb, data)| {
            data.statements
                .iter()
                .enumerate()
                .find_map(|(i, s)| match &s.kind {
                    StatementKind::Assign(dest, Rvalue::Ref(kind, _)) if kind.is_two_phase() => {
                        Some((bb, i, dest.local))
                    }
                    _ => None,
                })
        })
        .expect("the autoref'd `&mut self` receiver is two-phase");

    // `(*_t).X = 1;` after the borrow: a second use.
    let mut used_twice = bodies.clone();
    let span = used_twice[f].blocks[bb].statements[index].span;
    used_twice[f].blocks[bb].statements.insert(
        index + 1,
        Statement {
            kind: StatementKind::Assign(
                Place::local(temp).deref().field(0, Ty::I32),
                Rvalue::Use(Operand::Constant(Constant {
                    ty: Ty::I32,
                    value: ConstValue::Int(1),
                    span,
                })),
            ),
            span,
        },
    );
    let errors = sabiko_mir::validate::validate_all(&used_twice, &typeck);
    assert!(
        errors.iter().any(|e| e.contains("used 2 times")),
        "{errors:#?}"
    );

    // `_u = copy _t;` after the borrow: a second use, as an operand.
    let mut read_twice = bodies.clone();
    let reader = read_twice[f].locals.len() - 1;
    read_twice[f].blocks[bb].statements.insert(
        index + 1,
        Statement {
            kind: StatementKind::Assign(
                Place::local(sabiko_mir::Local(u32::try_from(reader).unwrap())),
                Rvalue::Use(Operand::Copy(Place::local(temp))),
            ),
            span,
        },
    );
    let errors = sabiko_mir::validate::validate_all(&read_twice, &typeck);
    assert!(
        errors.iter().any(|e| e.contains("used 2 times")),
        "{errors:#?}"
    );

    let mut into_user = bodies.clone();
    into_user[f].locals[temp.index()].kind = LocalKind::User;
    let errors = sabiko_mir::validate::validate_all(&into_user, &typeck);
    assert!(
        errors.iter().any(|e| e.contains("not a temporary")),
        "{errors:#?}"
    );
}

/// Enum places and variant aggregates must be well-formed: a field of an enum only after a
/// downcast, a downcast only of an enum and to one of its variants, and a variant
/// aggregate with one operand of the right type per field of an existing variant.
#[test]
fn validator_rejects_malformed_enum_mir() {
    let src = "enum E { A, B(i32) }\n\
        fn F(e: E) -> i32 { match e { E::B(n) => n, E::A => 0 } }\n\
        fn G(n: i32) -> E { E::B(n) }";
    let (bodies, typeck) = lower(src).expect("valid program");
    assert!(sabiko_mir::validate::validate_all(&bodies, &typeck).is_empty());
    let rejects = |bodies: &[Body], message: &str| {
        let errors = sabiko_mir::validate::validate_all(bodies, &typeck);
        assert!(
            errors.iter().any(|e| e.contains(message)),
            "expected {message:?}: {errors:#?}"
        );
    };

    // The binding `n = copy (_1 as B).0` in `F`.
    let f = bodies.iter().position(|b| b.name == "F").expect("F");
    let (bb, index) = bodies[f]
        .blocks
        .iter()
        .enumerate()
        .find_map(|(bb, data)| {
            data.statements
                .iter()
                .position(|s| {
                    matches!(&s.kind, StatementKind::Assign(_, Rvalue::Use(Operand::Copy(p)))
                        if p.projection.len() == 2)
                })
                .map(|i| (bb, i))
        })
        .expect("the binding reads a field of a variant");
    let with_source = |edit: &dyn Fn(&mut Place)| {
        let mut bodies = bodies.clone();
        let StatementKind::Assign(_, Rvalue::Use(Operand::Copy(p))) =
            &mut bodies[f].blocks[bb].statements[index].kind
        else {
            unreachable!("found above")
        };
        edit(p);
        bodies
    };
    rejects(
        &with_source(&|p| p.projection[0] = PlaceElem::Downcast(7)),
        "downcast to variant 7 of `E`, which has 2",
    );
    rejects(
        &with_source(&|p| {
            p.projection.remove(0);
        }),
        "field of an enum without a downcast to a variant",
    );
    let mut not_enum = bodies.clone();
    not_enum[f].locals[1].ty = Ty::I32;
    rejects(&not_enum, "downcast of non-enum type");

    // The aggregate `E::B(n)` in `G`.
    let g = bodies.iter().position(|b| b.name == "G").expect("G");
    let (bb, index) = bodies[g]
        .blocks
        .iter()
        .enumerate()
        .find_map(|(bb, data)| {
            data.statements
                .iter()
                .position(|s| {
                    matches!(
                        &s.kind,
                        StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Variant(..), _))
                    )
                })
                .map(|i| (bb, i))
        })
        .expect("`E::B(n)` is a variant aggregate");
    let with_aggregate = |edit: &dyn Fn(&mut AggregateKind, &mut Vec<Operand>)| {
        let mut bodies = bodies.clone();
        let StatementKind::Assign(_, Rvalue::Aggregate(kind, ops)) =
            &mut bodies[g].blocks[bb].statements[index].kind
        else {
            unreachable!("found above")
        };
        edit(kind, ops);
        bodies
    };
    let constant = |ty: Ty, value: ConstValue| {
        Operand::Constant(Constant {
            ty,
            value,
            span: sabiko_span::Span::dummy(),
        })
    };
    rejects(
        &with_aggregate(&|_, ops| ops.push(constant(Ty::I32, ConstValue::Int(1)))),
        "aggregate of `E::B` has 2 operands for 1 fields",
    );
    rejects(
        &with_aggregate(&|_, ops| ops[0] = constant(Ty::BOOL, ConstValue::Bool(true))),
        "aggregate field of type",
    );
    rejects(
        &with_aggregate(&|kind, _| {
            if let AggregateKind::Variant(_, v) = kind {
                *v = 9;
            }
        }),
        "aggregate of variant 9 of `E`",
    );
}
