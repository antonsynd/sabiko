//! Move checks (`E0406`, `E0407`, `E0418`) over hand-built MIR.
//!
//! As in `checks.rs`, `ResolveResults`/`TypeckResults` come from a tiny real program so
//! that struct and field names exist, while the bodies are built by hand so the tests do
//! not depend on MIR lowering. Only `moves::check_moves` runs, so the diagnostics of the
//! other checks never interfere.

use sabiko_borrowck::moves::{check_moves, MovePaths};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink};
use sabiko_mir::{
    AggregateKind, BasicBlock, BasicBlockData, Body, ConstValue, Constant, FakeReadCause, Local,
    LocalDecl, LocalKind, Mutability, Operand, Place, Rvalue, Statement, StatementKind, Terminator,
    TerminatorKind, Ty,
};
use sabiko_resolve::{DefId, DefKind, PrimTy, ResolveResults};
use sabiko_span::{FileId, Span};
use sabiko_types::TypeckResults;

const SRC: &str = r"
struct Point {
    X: i32,
    Y: i32,
}

struct Line {
    A: Point,
    B: Point,
}

#[derive(Copy, Clone)]
struct Vec2 {
    X: i32,
    Y: i32,
}

struct Guard {
    Id: i32,
    Inner: Point,
}

impl Drop for Guard {
    fn drop(&mut self) {}
}

struct Holder {
    G: Guard,
}

fn Take(p: Point) {}

fn TakeLine(l: Line) {}

fn TakeVec(v: Vec2) {}

fn MakeLine() -> Line {
    Line { A: Point { X: 0, Y: 0 }, B: Point { X: 1, Y: 1 } }
}

fn Main() {
    let p = Point { X: 1, Y: 2 };
    let l = MakeLine();
    Take(p);
    TakeLine(l);
    TakeVec(Vec2 { X: 1, Y: 2 });
}
";

struct Fixture {
    resolve: ResolveResults,
    typeck: TypeckResults,
    point: DefId,
    line: DefId,
    vec2: DefId,
    guard: DefId,
    holder: DefId,
    take: DefId,
    take_line: DefId,
    take_vec: DefId,
    make_line: DefId,
    main: DefId,
}

impl Fixture {
    fn new() -> Self {
        let mut sink = DiagnosticSink::new();
        let mut ast = sabiko_parser::parse_file(FileId(0), SRC, &mut sink);
        let resolve = sabiko_resolve::resolve(&mut ast, &mut sink);
        let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
        assert!(
            !sink.has_errors(),
            "fixture program must be clean: {:?}",
            sink.diagnostics()
        );
        let def = |kind: DefKind, name: &str| {
            resolve
                .defs_of_kind(kind)
                .find(|&d| resolve.def(d).name == name)
                .unwrap_or_else(|| panic!("no {kind:?} named {name}"))
        };
        Self {
            point: def(DefKind::Struct, "Point"),
            line: def(DefKind::Struct, "Line"),
            vec2: def(DefKind::Struct, "Vec2"),
            guard: def(DefKind::Struct, "Guard"),
            holder: def(DefKind::Struct, "Holder"),
            take: def(DefKind::Fn, "Take"),
            take_line: def(DefKind::Fn, "TakeLine"),
            take_vec: def(DefKind::Fn, "TakeVec"),
            make_line: def(DefKind::Fn, "MakeLine"),
            main: def(DefKind::Fn, "Main"),
            resolve,
            typeck,
        }
    }

    fn point_ty(&self) -> Ty {
        Ty::adt(self.point)
    }

    fn line_ty(&self) -> Ty {
        Ty::adt(self.line)
    }

    fn vec2_ty(&self) -> Ty {
        Ty::adt(self.vec2)
    }

    fn guard_ty(&self) -> Ty {
        Ty::adt(self.guard)
    }

    fn holder_ty(&self) -> Ty {
        Ty::adt(self.holder)
    }

    fn check(&self, body: &Body) -> Vec<Diagnostic> {
        let mut sink = DiagnosticSink::new();
        check_moves(body, &self.resolve, &self.typeck, &mut sink);
        sink.into_vec()
    }

    fn messages(&self, body: &Body) -> Vec<String> {
        self.check(body).iter().map(render).collect()
    }

    /// Asserts the checker accepts `body`, showing any diagnostics on failure.
    fn assert_clean(&self, body: &Body) {
        let messages = self.messages(body);
        assert!(messages.is_empty(), "unexpected diagnostics: {messages:#?}");
    }
}

fn render(d: &Diagnostic) -> String {
    format!("{} {}", d.code, d.message)
}

fn labels(d: &Diagnostic) -> Vec<(Span, &str)> {
    d.labels
        .iter()
        .map(|l| (l.span, l.message.as_str()))
        .collect()
}

/// Builds a [`Body`] statement by statement. Every statement, terminator and local gets a
/// distinct span so tests can tell labels apart.
struct BodyBuilder {
    body: Body,
    next_span: u32,
}

impl BodyBuilder {
    fn new(fx: &Fixture, ret: Ty) -> Self {
        let mut b = Self {
            body: Body {
                def: fx.main,
                name: "Main".to_owned(),
                instance: Vec::new(),
                locals: Vec::new(),
                arg_count: 0,
                blocks: Vec::new(),
                cleanups: Vec::new(),
                span: Span::new(FileId(0), 0, 1),
            },
            next_span: 1000,
        };
        b.decl(ret, Mutability::Mut, LocalKind::Return, None);
        b
    }

    fn span(&mut self) -> Span {
        let s = Span::new(FileId(0), self.next_span, self.next_span + 1);
        self.next_span += 1;
        s
    }

    fn decl(
        &mut self,
        ty: Ty,
        mutability: Mutability,
        kind: LocalKind,
        name: Option<&str>,
    ) -> Local {
        let span = self.span();
        self.body.locals.push(LocalDecl {
            ty,
            mutability,
            kind,
            name: name.map(str::to_owned),
            span,
            source: None,
            match_span: None,
        });
        Local(u32::try_from(self.body.locals.len() - 1).unwrap())
    }

    /// Arguments must be declared before any other local.
    fn arg(&mut self, name: &str, ty: Ty, mutability: Mutability) -> Local {
        assert_eq!(self.body.locals.len(), self.body.arg_count + 1);
        self.body.arg_count += 1;
        self.decl(ty, mutability, LocalKind::Arg, Some(name))
    }

    fn user(&mut self, name: &str, ty: Ty, mutability: Mutability) -> Local {
        self.decl(ty, mutability, LocalKind::User, Some(name))
    }

    fn temp(&mut self, ty: Ty) -> Local {
        self.decl(ty, Mutability::Mut, LocalKind::Temp, None)
    }

    fn block(&mut self) -> BasicBlock {
        self.body.blocks.push(BasicBlockData::default());
        BasicBlock(u32::try_from(self.body.blocks.len() - 1).unwrap())
    }

    fn stmt(&mut self, bb: BasicBlock, kind: StatementKind) -> Span {
        let span = self.span();
        self.body.blocks[bb.index()]
            .statements
            .push(Statement { kind, span });
        span
    }

    fn assign(&mut self, bb: BasicBlock, place: Place, rvalue: Rvalue) -> Span {
        self.stmt(bb, StatementKind::Assign(place, rvalue))
    }

    fn live(&mut self, bb: BasicBlock, local: Local) -> Span {
        self.stmt(bb, StatementKind::StorageLive(local))
    }

    fn dead(&mut self, bb: BasicBlock, local: Local) -> Span {
        self.stmt(bb, StatementKind::StorageDead(local))
    }

    fn terminate(&mut self, bb: BasicBlock, kind: TerminatorKind) -> Span {
        let span = self.span();
        self.body.blocks[bb.index()].terminator = Some(Terminator { kind, span });
        span
    }

    fn goto(&mut self, bb: BasicBlock, target: BasicBlock) -> Span {
        self.terminate(bb, TerminatorKind::Goto { target })
    }

    fn ret(&mut self, bb: BasicBlock) -> Span {
        self.terminate(bb, TerminatorKind::Return)
    }

    /// `switchInt(discr) -> [1: then, otherwise: else]`.
    fn branch(
        &mut self,
        bb: BasicBlock,
        discr: Operand,
        then: BasicBlock,
        els: BasicBlock,
    ) -> Span {
        self.terminate(
            bb,
            TerminatorKind::SwitchInt {
                discr,
                targets: vec![(1, then)],
                otherwise: els,
            },
        )
    }

    /// `destination = func(args) -> target`.
    fn call(
        &mut self,
        bb: BasicBlock,
        func: DefId,
        args: Vec<Operand>,
        destination: Place,
        target: BasicBlock,
    ) -> Span {
        self.terminate(
            bb,
            TerminatorKind::Call {
                func,
                generic_args: Vec::new(),
                args,
                destination,
                target: Some(target),
                fn_span: Span::dummy(),
            },
        )
    }

    fn finish(self) -> Body {
        self.body
    }
}

fn int(v: i128) -> Operand {
    Operand::Constant(Constant {
        ty: Ty::I32,
        value: ConstValue::Int(v),
        span: Span::new(FileId(0), 0, 0),
    })
}

fn unit() -> Rvalue {
    Rvalue::Use(Operand::Constant(Constant {
        ty: Ty::Unit,
        value: ConstValue::Unit,
        span: Span::new(FileId(0), 0, 0),
    }))
}

fn local(l: Local) -> Place {
    Place::local(l)
}

fn mv(place: Place) -> Operand {
    Operand::Move(place)
}

fn copy(place: Place) -> Operand {
    Operand::Copy(place)
}

fn use_move(place: Place) -> Rvalue {
    Rvalue::Use(mv(place))
}

fn point_aggregate(fx: &Fixture) -> Rvalue {
    Rvalue::Aggregate(
        AggregateKind::Adt(fx.point, Box::new([])),
        vec![int(1), int(2)],
    )
}

/// `l.A` / `l.B` of a `Line`.
fn line_field(fx: &Fixture, l: Local, index: usize) -> Place {
    local(l).field(index, fx.point_ty())
}

/// `p.X` of a `Point`.
fn point_x(p: Place) -> Place {
    p.field(0, Ty::I32)
}

const NOT_COPY_POINT: &str =
    "move occurs because `p` has type `Point`, which does not implement the `Copy` trait";

// ----------------------------------------------------------------------------------------
// E0406: use after move
// ----------------------------------------------------------------------------------------

/// A body has a move path for every local and every place it writes, moves out of or
/// indexes as an array (rustc's set): here `_0`, `p`, `l` and `x`; `l.A.X` is only read.
#[test]
fn move_paths_cover_the_mentioned_places() {
    assert!(MovePaths::default().is_empty());
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let x = mir.temp(Ty::I32);
    let bb0 = mir.block();
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.assign(
        bb0,
        local(x),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 0)))),
    );
    mir.ret(bb0);
    let paths = MovePaths::build(&mir.finish());
    assert!(!paths.is_empty());
    assert_eq!(paths.len(), 4);
}

#[test]
fn straight_line_use_after_move() {
    // let p = Point {..}; let q = p; let r = p;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let q = mir.user("q", fx.point_ty(), Mutability::Not);
    let r = mir.user("r", fx.point_ty(), Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, p);
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.live(bb0, q);
    let moved = mir.assign(bb0, local(q), use_move(local(p)));
    mir.live(bb0, r);
    let used = mir.assign(bb0, local(r), use_move(local(p)));
    mir.dead(bb0, r);
    mir.dead(bb0, q);
    mir.dead(bb0, p);
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
    assert!(diags[0].labels[0].primary && !diags[0].labels[1].primary);
    assert_eq!(diags[0].notes, [NOT_COPY_POINT]);
}

#[test]
fn second_use_of_same_move_is_not_reported_again() {
    // Take(p); let a = p.X; let b = p.X;  -- one error, not one per use, naming `p`, the
    // closest place with a move path, as rustc does
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let a = mir.user("a", Ty::I32, Mutability::Not);
    let c = mir.user("c", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let (bb0, bb1) = (mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    let moved = mir.call(bb0, fx.take, vec![mv(local(p))], local(t), bb1);
    let used = mir.assign(bb1, local(a), Rvalue::Use(copy(point_x(local(p)))));
    mir.assign(bb1, local(c), Rvalue::Use(copy(point_x(local(p)))));
    mir.ret(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
    assert_eq!(diags[0].notes, [NOT_COPY_POINT]);
}

#[test]
fn move_on_one_branch_is_maybe_moved() {
    // let p; if flag { Take(p) } ; Take(p)
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let t1 = mir.temp(Ty::Unit);
    let t2 = mir.temp(Ty::Unit);
    let (bb0, then, join, end) = (mir.block(), mir.block(), mir.block(), mir.block());
    mir.live(bb0, p);
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.branch(bb0, copy(local(flag)), then, join);
    let moved = mir.call(then, fx.take, vec![mv(local(p))], local(t1), join);
    let used = mir.call(join, fx.take, vec![mv(local(p))], local(t2), end);
    mir.dead(end, p);
    mir.ret(end);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn move_on_both_branches_labels_both() {
    // if flag { let a = p } else { let c = p }; Take(p)
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let c = mir.user("c", fx.point_ty(), Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let (bb0, then, els, join, end) = (
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
    );
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.branch(bb0, copy(local(flag)), then, els);
    let first = mir.assign(then, local(a), use_move(local(p)));
    mir.goto(then, join);
    let second = mir.assign(els, local(c), use_move(local(p)));
    mir.goto(els, join);
    let used = mir.call(join, fx.take, vec![mv(local(p))], local(t), end);
    mir.ret(end);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (first, "value moved here"),
            (second, "value moved here"),
        ]
    );
}

#[test]
fn move_in_loop_is_reported_for_the_next_iteration() {
    // let p; loop { Take(p) }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let (bb0, head, body_bb, back) = (mir.block(), mir.block(), mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.goto(bb0, head);
    mir.goto(head, body_bb);
    mir.live(body_bb, t);
    let site = mir.call(body_bb, fx.take, vec![mv(local(p))], local(t), back);
    mir.dead(back, t);
    mir.goto(back, head);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (site, "value used here after move"),
            (site, "value moved here, in previous iteration of loop"),
        ]
    );
}

#[test]
fn move_before_loop_then_use_inside_is_not_a_loop_move() {
    // Take(p); while flag { let a = p.X }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let a = mir.user("a", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let (bb0, head, body_bb, exit) = (mir.block(), mir.block(), mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    let moved = mir.call(bb0, fx.take, vec![mv(local(p))], local(t), head);
    mir.branch(head, copy(local(flag)), body_bb, exit);
    let used = mir.assign(body_bb, local(a), Rvalue::Use(copy(point_x(local(p)))));
    mir.goto(body_bb, head);
    mir.ret(exit);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(diags.len(), 1, "{diags:#?}");
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn borrow_after_move() {
    // Take(p); let r = &p;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let r = mir.user(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.point_ty())),
        Mutability::Not,
    );
    let t = mir.temp(Ty::Unit);
    let (bb0, bb1) = (mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    let moved = mir.call(bb0, fx.take, vec![mv(local(p))], local(t), bb1);
    let used = mir.assign(bb1, local(r), Rvalue::Ref(Mutability::Not.into(), local(p)));
    mir.ret(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 borrow of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value borrowed here after move"),
            (moved, "value moved here"),
        ]
    );
    assert_eq!(diags[0].notes, [NOT_COPY_POINT]);
}

#[test]
fn assign_to_part_of_moved_value() {
    // Take(p); p.X = 1;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let t = mir.temp(Ty::Unit);
    let (bb0, bb1) = (mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    let moved = mir.call(bb0, fx.take, vec![mv(local(p))], local(t), bb1);
    let used = mir.assign(bb1, point_x(local(p)), Rvalue::Use(int(1)));
    mir.ret(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 assign to part of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value partially assigned here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn use_through_moved_reference_names_the_reference() {
    // fn f(r: &mut Point) { let s = r; let x = r.X; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let ref_ty = Ty::Ref(Mutability::Mut, Box::new(fx.point_ty()));
    let r = mir.arg("r", ref_ty.clone(), Mutability::Not);
    let s = mir.user("s", ref_ty, Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    let moved = mir.assign(bb0, local(s), use_move(local(r)));
    let used = mir.assign(bb0, local(x), Rvalue::Use(copy(point_x(local(r).deref()))));
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `r`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
    assert_eq!(
        diags[0].notes,
        [
            "move occurs because `r` has type `&mut Point`, which does not implement the `Copy` trait",
            "consider reborrowing: `&mut *r`",
        ]
    );
}

// ----------------------------------------------------------------------------------------
// Partial moves and re-initialisation
// ----------------------------------------------------------------------------------------

/// `let l = MakeLine();` in `bb0`, continuing in the returned block.
fn make_line(mir: &mut BodyBuilder, fx: &Fixture, l: Local) -> BasicBlock {
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.live(bb0, l);
    mir.call(bb0, fx.make_line, vec![], local(l), bb1);
    bb1
}

#[test]
fn partial_move_then_use_of_parent() {
    // let a = l.A; TakeLine(l);
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let end = mir.block();
    let moved = mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let used = mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), end);
    mir.ret(end);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of partially moved value: `l`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after partial move"),
            (moved, "value partially moved here"),
        ]
    );
    assert_eq!(
        diags[0].notes,
        ["partial move occurs because `l.A` has type `Point`, which does not implement the `Copy` trait"]
    );
}

#[test]
fn partial_move_then_borrow_of_parent() {
    // let a = l.A; let r = &l;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let r = mir.user(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.line_ty())),
        Mutability::Not,
    );
    let bb1 = make_line(&mut mir, &fx, l);
    mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let used = mir.assign(bb1, local(r), Rvalue::Ref(Mutability::Not.into(), local(l)));
    mir.ret(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 borrow of partially moved value: `l`"]
    );
    assert_eq!(
        labels(&diags[0])[0],
        (used, "value borrowed here after partial move")
    );
}

#[test]
fn partial_move_then_sibling_is_accepted() {
    // let a = l.A; let x = l.B.X; let b = l.B;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let bp = mir.user("b", fx.point_ty(), Mutability::Not);
    let bb1 = make_line(&mut mir, &fx, l);
    mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    mir.assign(
        bb1,
        local(x),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 1)))),
    );
    mir.assign(bb1, local(bp), use_move(line_field(&fx, l, 1)));
    mir.ret(bb1);
    fx.assert_clean(&mir.finish());
}

#[test]
fn moving_the_same_field_twice() {
    // let a = l.A; let b = l.A;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let c = mir.user("c", fx.point_ty(), Mutability::Not);
    let bb1 = make_line(&mut mir, &fx, l);
    let moved = mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let used = mir.assign(bb1, local(c), use_move(line_field(&fx, l, 0)));
    mir.ret(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `l.A`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
    assert_eq!(
        diags[0].notes,
        ["move occurs because `l.A` has type `Point`, which does not implement the `Copy` trait"]
    );
}

#[test]
fn reinitialisation_by_assignment_then_use_is_accepted() {
    // Take(p); p = Point {..}; Take(p);
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let t1 = mir.temp(Ty::Unit);
    let t2 = mir.temp(Ty::Unit);
    let (bb0, bb1, bb2) = (mir.block(), mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.call(bb0, fx.take, vec![mv(local(p))], local(t1), bb1);
    mir.assign(bb1, local(p), point_aggregate(&fx));
    mir.call(bb1, fx.take, vec![mv(local(p))], local(t2), bb2);
    mir.ret(bb2);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reinitialisation_by_call_destination_is_accepted() {
    // TakeLine(l); l = MakeLine(); TakeLine(l);
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let t1 = mir.temp(Ty::Unit);
    let t2 = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let (bb2, bb3, bb4) = (mir.block(), mir.block(), mir.block());
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t1), bb2);
    mir.call(bb2, fx.make_line, vec![], local(l), bb3);
    mir.call(bb3, fx.take_line, vec![mv(local(l))], local(t2), bb4);
    mir.ret(bb4);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reinitialising_the_field_then_using_the_parent_is_accepted() {
    // let a = l.A; l.A = Point {..}; TakeLine(l);
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let end = mir.block();
    mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    mir.assign(bb1, line_field(&fx, l, 0), point_aggregate(&fx));
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), end);
    mir.ret(end);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reinitialising_the_parent_clears_the_children() {
    // let a = l.A; l = MakeLine(); let b = l.A;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let c = mir.user("c", fx.point_ty(), Mutability::Not);
    let bb1 = make_line(&mut mir, &fx, l);
    let bb2 = mir.block();
    mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    mir.call(bb1, fx.make_line, vec![], local(l), bb2);
    mir.assign(bb2, local(c), use_move(line_field(&fx, l, 0)));
    mir.ret(bb2);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reinitialising_one_field_leaves_the_other_moved() {
    // let a = l.A; let b = l.B; l.A = Point {..}; TakeLine(l);
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let a = mir.user("a", fx.point_ty(), Mutability::Not);
    let c = mir.user("c", fx.point_ty(), Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let end = mir.block();
    mir.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let moved_b = mir.assign(bb1, local(c), use_move(line_field(&fx, l, 1)));
    mir.assign(bb1, line_field(&fx, l, 0), point_aggregate(&fx));
    let used = mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), end);
    mir.ret(end);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of partially moved value: `l`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after partial move"),
            (moved_b, "value partially moved here"),
        ]
    );
}

#[test]
fn storage_dead_resets_the_local() {
    // loop { StorageLive(p); p = ..; Take(p); StorageDead(p) }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let (head, body_bb, back) = (mir.block(), mir.block(), mir.block());
    mir.goto(head, body_bb);
    mir.live(body_bb, p);
    mir.assign(body_bb, local(p), point_aggregate(&fx));
    mir.call(body_bb, fx.take, vec![mv(local(p))], local(t), back);
    mir.dead(back, p);
    mir.goto(back, head);
    fx.assert_clean(&mir.finish());
}

// ----------------------------------------------------------------------------------------
// E0407: move out of a borrow
// ----------------------------------------------------------------------------------------

#[test]
fn move_out_of_shared_reference() {
    // fn f(r: &Point) { let p = *r; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let r = mir.arg(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.point_ty())),
        Mutability::Not,
    );
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let bb0 = mir.block();
    let site = mir.assign(bb0, local(p), use_move(local(r).deref()));
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0407 cannot move out of `*r` which is behind a shared reference"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            site,
            "move occurs because `*r` has type `Point`, which does not implement the `Copy` trait"
        )]
    );
}

#[test]
fn move_out_of_mutable_reference_field() {
    // fn f(r: &mut Line) { Take(r.A); }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let r = mir.arg(
        "r",
        Ty::Ref(Mutability::Mut, Box::new(fx.line_ty())),
        Mutability::Not,
    );
    let t = mir.temp(Ty::Unit);
    let (bb0, bb1) = (mir.block(), mir.block());
    let site = mir.call(
        bb0,
        fx.take,
        vec![mv(local(r).deref().field(0, fx.point_ty()))],
        local(t),
        bb1,
    );
    mir.ret(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0407 cannot move out of `r.A` which is behind a mutable reference"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            site,
            "move occurs because `r.A` has type `Point`, which does not implement the `Copy` trait"
        )]
    );
}

#[test]
fn copy_out_of_reference_is_accepted() {
    // fn f(r: &Point) { let x = r.X; let y = r.X; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let r = mir.arg(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.point_ty())),
        Mutability::Not,
    );
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let y = mir.user("y", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), Rvalue::Use(copy(point_x(local(r).deref()))));
    mir.assign(bb0, local(y), Rvalue::Use(copy(point_x(local(r).deref()))));
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

// ----------------------------------------------------------------------------------------
// Copy types and temporaries
// ----------------------------------------------------------------------------------------

#[test]
fn copy_struct_is_never_reported() {
    // let v = Vec2 {..}; TakeVec(v); TakeVec(v); let w = v;
    let fx = Fixture::new();
    assert!(fx.typeck.items.is_copy(&fx.vec2_ty()));
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let v = mir.user("v", fx.vec2_ty(), Mutability::Not);
    let w = mir.user("w", fx.vec2_ty(), Mutability::Not);
    let t1 = mir.temp(Ty::Unit);
    let t2 = mir.temp(Ty::Unit);
    let (bb0, bb1, bb2) = (mir.block(), mir.block(), mir.block());
    mir.assign(
        bb0,
        local(v),
        Rvalue::Aggregate(
            AggregateKind::Adt(fx.vec2, Box::new([])),
            vec![int(1), int(2)],
        ),
    );
    mir.call(bb0, fx.take_vec, vec![copy(local(v))], local(t1), bb1);
    mir.call(bb1, fx.take_vec, vec![copy(local(v))], local(t2), bb2);
    mir.assign(bb2, local(w), Rvalue::Use(copy(local(v))));
    mir.ret(bb2);
    fx.assert_clean(&mir.finish());
}

#[test]
fn temporaries_are_tracked_but_never_reported() {
    // _t = Point {..}; l.A = move _t; TakeLine(l);  and a (malformed) double move of _t.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let t = mir.temp(fx.point_ty());
    let u = mir.temp(fx.point_ty());
    let unit_t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let end = mir.block();
    mir.live(bb1, t);
    mir.assign(bb1, local(t), point_aggregate(&fx));
    mir.assign(bb1, line_field(&fx, l, 0), use_move(local(t)));
    mir.assign(bb1, local(u), use_move(local(t)));
    mir.dead(bb1, t);
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(unit_t), end);
    mir.assign(end, local(Local::RETURN), unit());
    mir.ret(end);
    fx.assert_clean(&mir.finish());
}

// ----------------------------------------------------------------------------------------
// Move sites: the backward search stops at re-initialisations
// ----------------------------------------------------------------------------------------

#[test]
fn only_the_move_after_a_reinitialisation_is_reported() {
    // Take(p); p = Point {..}; if flag { Take(p) } Take(p)
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let t1 = mir.temp(Ty::Unit);
    let t2 = mir.temp(Ty::Unit);
    let t3 = mir.temp(Ty::Unit);
    let (bb0, bb1, then, join, end) = (
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
    );
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.call(bb0, fx.take, vec![mv(local(p))], local(t1), bb1);
    mir.assign(bb1, local(p), point_aggregate(&fx));
    mir.branch(bb1, copy(local(flag)), then, join);
    let second = mir.call(then, fx.take, vec![mv(local(p))], local(t2), join);
    let used = mir.call(join, fx.take, vec![mv(local(p))], local(t3), end);
    mir.ret(end);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    // The first move is behind the re-initialisation on every path: it is not a site.
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (second, "value moved here"),
        ]
    );
}

#[test]
fn a_call_destination_reinitialisation_also_stops_the_search() {
    // TakeLine(l); l = MakeLine(); if flag { TakeLine(l) } TakeLine(l)
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let t1 = mir.temp(Ty::Unit);
    let t2 = mir.temp(Ty::Unit);
    let t3 = mir.temp(Ty::Unit);
    let (bb0, bb1, bb2, bb3, then, join, end) = (
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
        mir.block(),
    );
    mir.call(bb0, fx.make_line, vec![], local(l), bb1);
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t1), bb2);
    // The re-initialisation is a call destination, so it terminates its block.
    mir.call(bb2, fx.make_line, vec![], local(l), bb3);
    mir.branch(bb3, copy(local(flag)), then, join);
    let second = mir.call(then, fx.take_line, vec![mv(local(l))], local(t2), join);
    let used = mir.call(join, fx.take_line, vec![mv(local(l))], local(t3), end);
    mir.ret(end);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `l`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (second, "value moved here"),
        ]
    );
}

#[test]
fn partial_assign_to_a_nested_field_of_a_moved_value() {
    // TakeLine(l); l.A.X = 1;  -- every enclosing struct of the written field is checked
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Mut);
    let t = mir.temp(Ty::Unit);
    let (bb0, bb1, bb2) = (mir.block(), mir.block(), mir.block());
    mir.call(bb0, fx.make_line, vec![], local(l), bb1);
    let moved = mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), bb2);
    let written = mir.assign(bb2, point_x(line_field(&fx, l, 0)), Rvalue::Use(int(1)));
    mir.ret(bb2);
    let body = mir.finish();

    let diags = fx.check(&body);
    // As rustc's `check_parent_of_field`, the shortest moved prefix is named (#169).
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 assign to part of moved value: `l`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (written, "value partially assigned here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn use_before_the_move_in_a_loop_names_the_previous_iteration() {
    // loop { let x = p.X; Take(p); }  -- the use comes first in the body, so the move is
    // only reached through the back edge.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let (bb0, head, body_bb) = (mir.block(), mir.block(), mir.block());
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.goto(bb0, head);
    let used = mir.assign(head, local(x), Rvalue::Use(copy(point_x(local(p)))));
    let moved = mir.call(head, fx.take, vec![mv(local(p))], local(t), body_bb);
    mir.goto(body_bb, head);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here, in previous iteration of loop"),
        ]
    );
}

#[test]
fn mut_ref_moved_into_a_deref_gets_no_reborrow_hint() {
    // fn F(r: &mut i32, slot: &mut &mut i32) { let q = slot; *q = r; *r = 1; }
    // Only `let s = r;` earns the "consider reborrowing" note, not a write through a
    // user local.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let r = mir.arg("r", Ty::mut_ref(Ty::I32), Mutability::Not);
    let slot = mir.arg("slot", Ty::mut_ref(Ty::mut_ref(Ty::I32)), Mutability::Not);
    let q = mir.user("q", Ty::mut_ref(Ty::mut_ref(Ty::I32)), Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, q);
    mir.assign(bb0, local(q), use_move(local(slot)));
    let moved = mir.assign(bb0, local(q).deref(), use_move(local(r)));
    let used = mir.assign(bb0, local(r).deref(), Rvalue::Use(int(1)));
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `r`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
    assert_eq!(
        diags[0].notes,
        ["move occurs because `r` has type `&mut i32`, which does not implement the `Copy` trait"]
    );
}

#[test]
fn place_mention_is_not_a_use_or_a_move() {
    // let p = Point {..}; let _ = p; let q = p; let _ = p; let _ = p.X;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let q = mir.user("q", fx.point_ty(), Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, p);
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.stmt(bb0, StatementKind::PlaceMention(local(p)));
    mir.live(bb0, q);
    mir.assign(bb0, local(q), use_move(local(p)));
    mir.stmt(bb0, StatementKind::PlaceMention(local(p)));
    mir.stmt(bb0, StatementKind::PlaceMention(point_x(local(p))));
    mir.dead(bb0, q);
    mir.dead(bb0, p);
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

fn usize_ty() -> Ty {
    Ty::Prim(PrimTy::Usize)
}

/// `a = [Point { .. }]` and `i = 0`, with both live.
fn init_point_array(fx: &Fixture, mir: &mut BodyBuilder, bb: BasicBlock, a: Local, i: Local) {
    let elem = mir.temp(fx.point_ty());
    mir.assign(bb, local(elem), point_aggregate(fx));
    mir.live(bb, a);
    mir.assign(
        bb,
        local(a),
        Rvalue::Aggregate(AggregateKind::Array(fx.point_ty()), vec![mv(local(elem))]),
    );
    mir.assign(
        bb,
        local(i),
        Rvalue::Use(Operand::Constant(Constant {
            ty: usize_ty(),
            value: ConstValue::Int(0),
            span: Span::new(FileId(0), 0, 0),
        })),
    );
}

#[test]
fn index_bounds_check_uses_the_moved_base() {
    // let a = [Point {..}]; let b = a; let _ = a[i];  -- rustc's `FakeRead(ForIndex, a)`
    // before the bounds check is a use of `a` (#164).
    let fx = Fixture::new();
    let array = Ty::Array(Box::new(fx.point_ty()), 1);
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let a = mir.user("a", array.clone(), Mutability::Not);
    let b = mir.user("b", array, Mutability::Not);
    let i = mir.temp(usize_ty());
    let bb0 = mir.block();
    init_point_array(&fx, &mut mir, bb0, a, i);
    mir.live(bb0, b);
    let moved = mir.assign(bb0, local(b), use_move(local(a)));
    let used = mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(a)),
    );
    // A second bounds check repeats the same move: not reported again.
    mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(a)),
    );
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `a`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn a_borrow_after_the_bounds_check_replaces_its_error() {
    // let b = a; FakeRead(ForIndex, a); r = &a[i];  -- the bounds check of `&a[i]` reads
    // `a`, then the element is borrowed; as in rustc, the borrow's error replaces the
    // bounds check's for the same move: "borrow of moved value", at the borrow.
    let fx = Fixture::new();
    let array = Ty::Array(Box::new(fx.point_ty()), 1);
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let a = mir.user("a", array.clone(), Mutability::Not);
    let b = mir.user("b", array, Mutability::Not);
    let r = mir.user("r", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let i = mir.temp(usize_ty());
    let bb0 = mir.block();
    init_point_array(&fx, &mut mir, bb0, a, i);
    mir.live(bb0, b);
    let moved = mir.assign(bb0, local(b), use_move(local(a)));
    mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(a)),
    );
    let borrowed = mir.assign(
        bb0,
        local(r),
        Rvalue::Ref(Mutability::Not.into(), local(a).index(i)),
    );
    // A later bounds check reads `a`, a prefix of the borrowed place: no new report.
    mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(a)),
    );
    mir.ret(bb0);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 borrow of moved value: `a`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (borrowed, "value borrowed here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn a_later_use_of_another_part_replaces_the_error_and_a_prefix_does_not() {
    // let l = MakeLine(); TakeLine(l); x = l.A.X; y = l.B.X; Take(l.B);
    // One error per set of moves, as rustc's `buffer_move_error`: `l.B.X` is no prefix of
    // `l.A.X`, so its error replaces the first; `l.B` is a prefix of `l.B.X`, so its use
    // is not reported again. The error names `l.B`, which has a move path (it is moved), not
    // `l.B.X`.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let y = mir.user("y", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let (bb2, bb3) = (mir.block(), mir.block());
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), bb2);
    mir.assign(
        bb2,
        local(x),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 0)))),
    );
    let second = mir.assign(
        bb2,
        local(y),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 1)))),
    );
    mir.call(bb2, fx.take, vec![mv(line_field(&fx, l, 1))], local(t), bb3);
    mir.ret(bb3);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `l.B`"]
    );
    assert_eq!(diags[0].primary_span(), Some(second));
}

/// `switchInt(discr) -> [0: els, otherwise: then]`, the shape MIR building gives an `if`
/// or a `while` condition (unlike [`BodyBuilder::branch`]).
fn branch_as_lowered(
    mir: &mut BodyBuilder,
    bb: BasicBlock,
    discr: Operand,
    then: BasicBlock,
    els: BasicBlock,
) {
    mir.terminate(
        bb,
        TerminatorKind::SwitchInt {
            discr,
            targets: vec![(0, els)],
            otherwise: then,
        },
    );
}

#[test]
fn the_walk_visits_the_else_branch_before_the_then_branch() {
    // let l = MakeLine(); TakeLine(l); if flag { x = l.A.X } else { y = l.B.X }
    // rustc walks the blocks in reverse postorder, where the `else` block (the first
    // target) comes before the `then` block (`otherwise`): the `then` use is the later
    // one and its error replaces the other's, although its block is numbered first.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let y = mir.user("y", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let (bb2, then, els, end) = (mir.block(), mir.block(), mir.block(), mir.block());
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), bb2);
    branch_as_lowered(&mut mir, bb2, copy(local(flag)), then, els);
    let in_then = mir.assign(
        then,
        local(x),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 0)))),
    );
    mir.goto(then, end);
    mir.assign(
        els,
        local(y),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 1)))),
    );
    mir.goto(els, end);
    mir.ret(end);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `l`"]
    );
    assert_eq!(diags[0].primary_span(), Some(in_then));
}

#[test]
fn the_walk_visits_the_code_after_a_loop_before_its_body() {
    // let l = MakeLine(); TakeLine(l); while flag { x = l.A.X } y = l.B.X;
    // In reverse postorder the loop's exit (the first target) comes before its body, so
    // the use in the body is the later one and its error is reported.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let flag = mir.arg("flag", Ty::BOOL, Mutability::Not);
    let l = mir.user("l", fx.line_ty(), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let y = mir.user("y", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::Unit);
    let bb1 = make_line(&mut mir, &fx, l);
    let (header, body, exit) = (mir.block(), mir.block(), mir.block());
    mir.call(bb1, fx.take_line, vec![mv(local(l))], local(t), header);
    branch_as_lowered(&mut mir, header, copy(local(flag)), body, exit);
    let in_body = mir.assign(
        body,
        local(x),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 0)))),
    );
    mir.goto(body, header);
    mir.assign(
        exit,
        local(y),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 1)))),
    );
    mir.ret(exit);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `l`"]
    );
    assert_eq!(diags[0].primary_span(), Some(in_body));
}

#[test]
fn a_write_to_a_field_behind_a_moved_reference_uses_the_reference() {
    // fn f(r: &mut Point) { let s = r; (*r).X = 1; }  -- rustc checks the reference `r`
    // the write goes through ("use of moved value"), not the field ("assign to part of").
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let ref_ty = Ty::Ref(Mutability::Mut, Box::new(fx.point_ty()));
    let r = mir.arg("r", ref_ty.clone(), Mutability::Not);
    let s = mir.user("s", ref_ty, Mutability::Not);
    let bb0 = mir.block();
    let moved = mir.assign(bb0, local(s), use_move(local(r)));
    let used = mir.assign(bb0, point_x(local(r).deref()), Rvalue::Use(int(1)));
    mir.ret(bb0);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `r`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value used here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn index_bounds_check_of_a_moved_field_names_the_field() {
    // let t = ([Point {..}], 1); let u = t.0; let _ = t.0[i];
    let fx = Fixture::new();
    let array = Ty::Array(Box::new(fx.point_ty()), 1);
    let tuple = Ty::Tuple(Box::new(vec![array.clone(), Ty::I32]));
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let a = mir.temp(array.clone());
    let t = mir.user("t", tuple, Mutability::Not);
    let u = mir.user("u", array.clone(), Mutability::Not);
    let i = mir.temp(usize_ty());
    let bb0 = mir.block();
    init_point_array(&fx, &mut mir, bb0, a, i);
    mir.live(bb0, t);
    mir.assign(
        bb0,
        local(t),
        Rvalue::Aggregate(AggregateKind::Tuple, vec![mv(local(a)), int(1)]),
    );
    mir.live(bb0, u);
    mir.assign(bb0, local(u), use_move(local(t).field(0, array.clone())));
    mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(t).field(0, array)),
    );
    mir.ret(bb0);
    assert_eq!(
        fx.messages(&mir.finish()),
        ["E0406 use of moved value: `t.0`"]
    );
}

#[test]
fn slice_bounds_check_borrows_the_moved_reference() {
    // fn F(s: &mut [Point]) { let t = s; let _ = (*s)[i]; }  -- a slice's length is read
    // through a fake borrow of `*s`: "borrow of moved value".
    let fx = Fixture::new();
    let slice_ref = Ty::mut_ref(Ty::Slice(Box::new(fx.point_ty())));
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let s = mir.arg("s", slice_ref.clone(), Mutability::Not);
    let t = mir.user("t", slice_ref, Mutability::Not);
    let i = mir.temp(usize_ty());
    let bb0 = mir.block();
    mir.assign(bb0, local(i), Rvalue::Use(int(0)));
    mir.live(bb0, t);
    let moved = mir.assign(bb0, local(t), use_move(local(s)));
    let used = mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(s).deref()),
    );
    mir.ret(bb0);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 borrow of moved value: `s`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (used, "value borrowed here after move"),
            (moved, "value moved here"),
        ]
    );
}

#[test]
fn index_bounds_check_after_reinitialisation_is_accepted() {
    // let mut a = [Point {..}]; let b = a; a = [Point {..}]; let _ = a[i];
    let fx = Fixture::new();
    let array = Ty::Array(Box::new(fx.point_ty()), 1);
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let a = mir.user("a", array.clone(), Mutability::Mut);
    let b = mir.user("b", array.clone(), Mutability::Not);
    let c = mir.temp(array);
    let i = mir.temp(usize_ty());
    let bb0 = mir.block();
    init_point_array(&fx, &mut mir, bb0, a, i);
    mir.live(bb0, b);
    mir.assign(bb0, local(b), use_move(local(a)));
    init_point_array(&fx, &mut mir, bb0, c, i);
    mir.assign(bb0, local(a), use_move(local(c)));
    mir.stmt(
        bb0,
        StatementKind::FakeRead(FakeReadCause::ForIndex, local(a)),
    );
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

/// `g = Guard { Id: 1, Inner: Point { .. } }`, with `g` live.
fn init_guard(fx: &Fixture, mir: &mut BodyBuilder, bb: BasicBlock, g: Local) {
    let inner = mir.temp(fx.point_ty());
    mir.assign(bb, local(inner), point_aggregate(fx));
    mir.live(bb, g);
    mir.assign(
        bb,
        local(g),
        Rvalue::Aggregate(
            AggregateKind::Adt(fx.guard, Box::new([])),
            vec![int(1), mv(local(inner))],
        ),
    );
}

#[test]
fn move_out_of_a_drop_type_is_rejected() {
    // let g = Guard {..}; let q = g.Inner;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let g = mir.user("g", fx.guard_ty(), Mutability::Not);
    let q = mir.user("q", fx.point_ty(), Mutability::Not);
    let bb0 = mir.block();
    init_guard(&fx, &mut mir, bb0, g);
    mir.live(bb0, q);
    let span = mir.assign(bb0, local(q), use_move(local(g).field(1, fx.point_ty())));
    mir.ret(bb0);
    let diags = fx.check(&mir.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0418 cannot move out of type `Guard`, which implements the `Drop` trait"]
    );
    assert_eq!(labels(&diags[0]), [(span, "cannot move out of here")]);
    assert_eq!(
        diags[0].notes,
        [
            "move occurs because `g.Inner` has type `Point`, which does not implement the `Copy` trait",
            "`g` is dropped as a whole by its `drop`; borrow the field instead",
        ]
    );
}

#[test]
fn move_out_of_a_drop_type_inside_a_plain_struct_is_rejected() {
    // let h = Holder { G: Guard {..} }; let q = h.G.Inner;  (and `h.G` itself is fine)
    let fx = Fixture::new();
    for whole_guard in [false, true] {
        let mut mir = BodyBuilder::new(&fx, Ty::Unit);
        let h = mir.user("h", fx.holder_ty(), Mutability::Not);
        let g = mir.temp(fx.guard_ty());
        let bb0 = mir.block();
        init_guard(&fx, &mut mir, bb0, g);
        mir.live(bb0, h);
        mir.assign(
            bb0,
            local(h),
            Rvalue::Aggregate(
                AggregateKind::Adt(fx.holder, Box::new([])),
                vec![mv(local(g))],
            ),
        );
        let guard = local(h).field(0, fx.guard_ty());
        let (dest_ty, moved) = if whole_guard {
            (fx.guard_ty(), guard)
        } else {
            (fx.point_ty(), guard.field(1, fx.point_ty()))
        };
        let q = mir.user("q", dest_ty, Mutability::Not);
        mir.live(bb0, q);
        mir.assign(bb0, local(q), use_move(moved));
        mir.ret(bb0);
        let body = mir.finish();
        if whole_guard {
            fx.assert_clean(&body);
        } else {
            let diags = fx.check(&body);
            assert_eq!(
                diags.iter().map(render).collect::<Vec<_>>(),
                ["E0418 cannot move out of type `Guard`, which implements the `Drop` trait"]
            );
            assert_eq!(
                diags[0].notes[1],
                "`h.G` is dropped as a whole by its `drop`; borrow the field instead"
            );
        }
    }
}

#[test]
fn copying_a_field_out_of_a_drop_type_is_fine() {
    // let g = Guard {..}; let n = g.Id; let x = g.Inner.X;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let g = mir.user("g", fx.guard_ty(), Mutability::Not);
    let n = mir.user("n", Ty::I32, Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    init_guard(&fx, &mut mir, bb0, g);
    mir.assign(bb0, local(n), Rvalue::Use(copy(local(g).field(0, Ty::I32))));
    mir.assign(
        bb0,
        local(x),
        Rvalue::Use(copy(point_x(local(g).field(1, fx.point_ty())))),
    );
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn drop_of_a_moved_value_is_not_a_use() {
    // let g = Guard {..}; let q = g; drop(q); drop(g);
    // MIR building drops every local at its scope end, moved or not; elaboration removes
    // the drops of moved values later, so the move checker must not see a use there.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let g = mir.user("g", fx.guard_ty(), Mutability::Not);
    let q = mir.user("q", fx.guard_ty(), Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    let bb2 = mir.block();
    init_guard(&fx, &mut mir, bb0, g);
    mir.live(bb0, q);
    mir.assign(bb0, local(q), use_move(local(g)));
    mir.terminate(
        bb0,
        TerminatorKind::Drop {
            place: local(q),
            target: bb1,
        },
    );
    mir.dead(bb1, q);
    mir.terminate(
        bb1,
        TerminatorKind::Drop {
            place: local(g),
            target: bb2,
        },
    );
    mir.dead(bb2, g);
    mir.ret(bb2);
    fx.assert_clean(&mir.finish());
}

#[test]
fn move_out_from_behind_a_reference_inside_a_drop_type_is_only_e0407() {
    // let q = *g.R;  with `g: Guard` and `R: &Point`. No struct can hold a reference before
    // lifetimes (Phase 9), but the checks read the projection's types, so hand-built MIR
    // can: the move is out of a borrow (rustc's E0507 alone), not out of the `Drop` type.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx, Ty::Unit);
    let g = mir.user("g", fx.guard_ty(), Mutability::Not);
    let q = mir.user("q", fx.point_ty(), Mutability::Not);
    let bb0 = mir.block();
    init_guard(&fx, &mut mir, bb0, g);
    mir.live(bb0, q);
    let through_ref = local(g).field(1, Ty::shared_ref(fx.point_ty())).deref();
    mir.assign(bb0, local(q), use_move(through_ref));
    mir.ret(bb0);
    let messages = fx.messages(&mir.finish());
    assert_eq!(messages.len(), 1, "{messages:#?}");
    assert!(messages[0].starts_with("E0407 "), "{messages:#?}");
}
