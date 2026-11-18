//! The dataflow framework on its own: `BitSet` semantics the analyses rely on, the
//! cursor replaying a block from its entry state, and the initialisation analysis'
//! untracked locals.

use std::cell::RefCell;

use sabiko_borrowck::dataflow::{iterate_to_fixpoint, Analysis, BitSet, Location, ResultsCursor};
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
        match_span: None,
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
        instance: Vec::new(),
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

/// An analysis that records the location of every statement it is applied to.
#[derive(Default)]
struct Recording {
    applied: RefCell<Vec<Location>>,
}

impl Analysis for Recording {
    fn bottom_value(&self, body: &Body) -> BitSet {
        BitSet::new_empty(body.locals.len())
    }

    fn initialize_start_block(&self, _: &Body, _: &mut BitSet) {}

    fn join(&self, _: &mut BitSet, _: &BitSet) -> bool {
        false
    }

    fn apply_statement(&self, _: &mut BitSet, _: &Statement, location: Location) {
        self.applied.borrow_mut().push(location);
    }

    fn apply_terminator(&self, _: &mut BitSet, _: &Terminator, _: Location) {}
}

#[test]
fn cursor_applies_each_statement_once_seeking_forwards() {
    let (body, _) = one_block_body();
    let analysis = Recording::default();
    let results = iterate_to_fixpoint(&body, &analysis);
    analysis.applied.borrow_mut().clear();
    let mut cursor = ResultsCursor::new(&body, &analysis, &results);
    let at = |i| Location::new(BasicBlock::START, i);

    cursor.seek_before(at(2));
    assert_eq!(*analysis.applied.borrow(), [at(0), at(1)]);
    // Forwards in the same block: only the statements in between, each at its own
    // location, not the block again from its entry.
    cursor.seek_before(at(3));
    assert_eq!(*analysis.applied.borrow(), [at(0), at(1), at(2)]);
    cursor.seek_before(at(3));
    assert_eq!(
        analysis.applied.borrow().len(),
        3,
        "seeking where the cursor is applies nothing"
    );
}

#[test]
fn init_treats_temporaries_as_initialised_throughout() {
    // `F`'s body with a temporary `_2` given storage, a value and its end after `x`'s:
    // only user bindings are tracked, so the temporary is initialised before its
    // `StorageLive` and after its `StorageDead` too (the lowering guarantees its uses).
    let (mut body, x) = one_block_body();
    let mut decl = body.locals[1].clone();
    decl.kind = LocalKind::Temp;
    decl.name = None;
    body.locals.push(decl);
    let temp = Local(2);
    body.blocks[0].statements.extend([
        Statement {
            kind: StatementKind::StorageLive(temp),
            span: span(5),
        },
        Statement {
            kind: StatementKind::Assign(
                Place::local(temp),
                Rvalue::Use(Operand::Copy(Place::local(x))),
            ),
            span: span(6),
        },
        Statement {
            kind: StatementKind::StorageDead(temp),
            span: span(7),
        },
    ]);
    for kind in [InitKind::Maybe, InitKind::Definite] {
        let analysis = InitAnalysis::new(kind, &body);
        assert!(analysis.is_tracked(x) && !analysis.is_tracked(temp));
        let results = iterate_to_fixpoint(&body, &analysis);
        let mut cursor = ResultsCursor::new(&body, &analysis, &results);
        for i in 0..=body.blocks[0].statements.len() {
            cursor.seek_before(Location::new(BasicBlock::START, i));
            assert!(
                cursor.get().contains(temp.index()),
                "{kind:?}: the temporary before statement {i}"
            );
        }
    }
}
