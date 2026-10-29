//! The dataflow framework on its own: `BitSet` semantics the analyses rely on, and the
//! cursor replaying a block from its entry state.

use sabiko_borrowck::dataflow::{iterate_to_fixpoint, BitSet, Location, ResultsCursor};
use sabiko_borrowck::init::{InitAnalysis, InitKind};
use sabiko_mir::{
    BasicBlock, BasicBlockData, Body, ConstValue, Constant, Local, LocalDecl, LocalKind,
    Mutability, Operand, Place, Rvalue, Statement, StatementKind, Terminator, TerminatorKind, Ty,
};
use sabiko_resolve::DefId;
use sabiko_span::{FileId, Span};

#[test]
fn filled_set_equals_every_member_inserted() {
    // The last word holds only 6 of the 70 bits: the excess bits must be clear, or whole-word
    // comparisons would tell a filled set from one built member by member.
    let filled = BitSet::new_filled(70);
    let mut built = BitSet::new_empty(70);
    for i in 0..70 {
        assert!(built.insert(i));
    }
    assert_eq!(filled, built);
    assert_eq!(filled.iter().count(), 70);
    // A multiple of the word size has no excess bits to clear.
    assert_eq!(BitSet::new_filled(128).iter().count(), 128);
}

fn span(n: u32) -> Span {
    Span::new(FileId(0), n, n + 1)
}

/// `fn F() { let x; x = 1; }` as MIR: `StorageLive(x); x = 1; StorageDead(x); return`.
fn one_block_body() -> (Body, Local) {
    let decl = |ty: Ty, kind: LocalKind, name: Option<&str>| LocalDecl {
        ty,
        mutability: Mutability::Not,
        kind,
        name: name.map(str::to_owned),
        span: span(0),
        source: None,
    };
    let x = Local(1);
    let statements = vec![
        Statement {
            kind: StatementKind::StorageLive(x),
            span: span(1),
        },
        Statement {
            kind: StatementKind::Assign(
                Place::local(x),
                Rvalue::Use(Operand::Constant(Constant {
                    ty: Ty::I32,
                    value: ConstValue::Int(1),
                    span: span(2),
                })),
            ),
            span: span(2),
        },
        Statement {
            kind: StatementKind::StorageDead(x),
            span: span(3),
        },
    ];
    let body = Body {
        def: DefId(0),
        name: "F".to_owned(),
        locals: vec![
            decl(Ty::Unit, LocalKind::Return, None),
            decl(Ty::I32, LocalKind::User, Some("x")),
        ],
        arg_count: 0,
        blocks: vec![BasicBlockData {
            statements,
            terminator: Some(Terminator {
                kind: TerminatorKind::Return,
                span: span(4),
            }),
            cleanup: None,
        }],
        cleanups: Vec::new(),
        span: span(0),
    };
    (body, x)
}

#[test]
fn cursor_replays_the_block_when_seeking_backwards() {
    let (body, x) = one_block_body();
    let analysis = InitAnalysis::new(InitKind::Definite, &body);
    let results = iterate_to_fixpoint(&body, &analysis);
    let mut cursor = ResultsCursor::new(&body, &analysis, &results);
    let bb0 = BasicBlock::START;

    // Forward: after `x = 1`, before `StorageDead(x)`.
    cursor.seek_before(Location::new(bb0, 2));
    assert!(cursor.get().contains(x.index()));
    // Backwards to the block entry: the state is the entry set again, not a stale one.
    cursor.seek_before(Location::new(bb0, 0));
    assert!(!cursor.get().contains(x.index()));
    // Forward again, incrementally, through `StorageLive` and the assignment.
    cursor.seek_before(Location::new(bb0, 1));
    assert!(!cursor.get().contains(x.index()));
    cursor.seek_before(Location::terminator_of(&body, bb0));
    assert!(
        !cursor.get().contains(x.index()),
        "storage ended before return"
    );
    assert_eq!(
        results.entry_set(bb0).iter().count(),
        1,
        "only `_0` is initialised on entry"
    );
}
