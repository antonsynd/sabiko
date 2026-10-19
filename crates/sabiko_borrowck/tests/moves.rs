//! Move checks (`E0406`, `E0407`) over hand-built MIR.
//!
//! As in `checks.rs`, `ResolveResults`/`TypeckResults` come from a tiny real program so
//! that struct and field names exist, while the bodies are built by hand so the tests do
//! not depend on MIR lowering. Only `moves::check_moves` runs, so the diagnostics of the
//! other checks never interfere.

use sabiko_borrowck::moves::check_moves;
use sabiko_diagnostics::{Diagnostic, DiagnosticSink};
use sabiko_mir::{
    BasicBlock, BasicBlockData, Body, ConstValue, Constant, Local, LocalDecl, LocalKind,
    Mutability, Operand, Place, Rvalue, Statement, StatementKind, Terminator, TerminatorKind, Ty,
};
use sabiko_resolve::{DefId, DefKind, ResolveResults};
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
    take: DefId,
    take_line: DefId,
    take_vec: DefId,
    make_line: DefId,
    main: DefId,
}

impl Fixture {
    fn new() -> Self {
        let mut sink = DiagnosticSink::new();
        let ast = sabiko_parser::parse_file(FileId(0), SRC, &mut sink);
        let resolve = sabiko_resolve::resolve(&ast, &mut sink);
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
        Ty::Adt(self.point)
    }

    fn line_ty(&self) -> Ty {
        Ty::Adt(self.line)
    }

    fn vec2_ty(&self) -> Ty {
        Ty::Adt(self.vec2)
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
                locals: Vec::new(),
                arg_count: 0,
                blocks: Vec::new(),
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
                args,
                destination,
                target: Some(target),
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
    Rvalue::Aggregate(fx.point, vec![int(1), int(2)])
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

#[test]
fn straight_line_use_after_move() {
    // let p = Point {..}; let q = p; let r = p;
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let q = b.user("q", fx.point_ty(), Mutability::Not);
    let r = b.user("r", fx.point_ty(), Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, p);
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.live(bb0, q);
    let moved = b.assign(bb0, local(q), use_move(local(p)));
    b.live(bb0, r);
    let used = b.assign(bb0, local(r), use_move(local(p)));
    b.dead(bb0, r);
    b.dead(bb0, q);
    b.dead(bb0, p);
    b.ret(bb0);
    let body = b.finish();

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
    // Take(p); let a = p.X; let b = p.X;  -- one error, not one per use
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let a = b.user("a", Ty::I32, Mutability::Not);
    let c = b.user("c", Ty::I32, Mutability::Not);
    let t = b.temp(Ty::Unit);
    let (bb0, bb1) = (b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    let moved = b.call(bb0, fx.take, vec![mv(local(p))], local(t), bb1);
    let used = b.assign(bb1, local(a), Rvalue::Use(copy(point_x(local(p)))));
    b.assign(bb1, local(c), Rvalue::Use(copy(point_x(local(p)))));
    b.ret(bb1);
    let body = b.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0406 use of moved value: `p.X`"]
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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let t1 = b.temp(Ty::Unit);
    let t2 = b.temp(Ty::Unit);
    let (bb0, then, join, end) = (b.block(), b.block(), b.block(), b.block());
    b.live(bb0, p);
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.branch(bb0, copy(local(flag)), then, join);
    let moved = b.call(then, fx.take, vec![mv(local(p))], local(t1), join);
    let used = b.call(join, fx.take, vec![mv(local(p))], local(t2), end);
    b.dead(end, p);
    b.ret(end);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let c = b.user("c", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::Unit);
    let (bb0, then, els, join, end) = (b.block(), b.block(), b.block(), b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.branch(bb0, copy(local(flag)), then, els);
    let first = b.assign(then, local(a), use_move(local(p)));
    b.goto(then, join);
    let second = b.assign(els, local(c), use_move(local(p)));
    b.goto(els, join);
    let used = b.call(join, fx.take, vec![mv(local(p))], local(t), end);
    b.ret(end);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::Unit);
    let (bb0, head, body_bb, back) = (b.block(), b.block(), b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.goto(bb0, head);
    b.goto(head, body_bb);
    b.live(body_bb, t);
    let site = b.call(body_bb, fx.take, vec![mv(local(p))], local(t), back);
    b.dead(back, t);
    b.goto(back, head);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let a = b.user("a", Ty::I32, Mutability::Not);
    let t = b.temp(Ty::Unit);
    let (bb0, head, body_bb, exit) = (b.block(), b.block(), b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    let moved = b.call(bb0, fx.take, vec![mv(local(p))], local(t), head);
    b.branch(head, copy(local(flag)), body_bb, exit);
    let used = b.assign(body_bb, local(a), Rvalue::Use(copy(point_x(local(p)))));
    b.goto(body_bb, head);
    b.ret(exit);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let r = b.user(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.point_ty())),
        Mutability::Not,
    );
    let t = b.temp(Ty::Unit);
    let (bb0, bb1) = (b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    let moved = b.call(bb0, fx.take, vec![mv(local(p))], local(t), bb1);
    let used = b.assign(bb1, local(r), Rvalue::Ref(Mutability::Not, local(p)));
    b.ret(bb1);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Mut);
    let t = b.temp(Ty::Unit);
    let (bb0, bb1) = (b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    let moved = b.call(bb0, fx.take, vec![mv(local(p))], local(t), bb1);
    let used = b.assign(bb1, point_x(local(p)), Rvalue::Use(int(1)));
    b.ret(bb1);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let ref_ty = Ty::Ref(Mutability::Mut, Box::new(fx.point_ty()));
    let r = b.arg("r", ref_ty.clone(), Mutability::Not);
    let s = b.user("s", ref_ty, Mutability::Not);
    let x = b.user("x", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    let moved = b.assign(bb0, local(s), use_move(local(r)));
    let used = b.assign(bb0, local(x), Rvalue::Use(copy(point_x(local(r).deref()))));
    b.ret(bb0);
    let body = b.finish();

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
fn make_line(b: &mut BodyBuilder, fx: &Fixture, l: Local) -> BasicBlock {
    let bb0 = b.block();
    let bb1 = b.block();
    b.live(bb0, l);
    b.call(bb0, fx.make_line, vec![], local(l), bb1);
    bb1
}

#[test]
fn partial_move_then_use_of_parent() {
    // let a = l.A; TakeLine(l);
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Not);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::Unit);
    let bb1 = make_line(&mut b, &fx, l);
    let end = b.block();
    let moved = b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let used = b.call(bb1, fx.take_line, vec![mv(local(l))], local(t), end);
    b.ret(end);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Not);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let r = b.user(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.line_ty())),
        Mutability::Not,
    );
    let bb1 = make_line(&mut b, &fx, l);
    b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let used = b.assign(bb1, local(r), Rvalue::Ref(Mutability::Not, local(l)));
    b.ret(bb1);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Not);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let x = b.user("x", Ty::I32, Mutability::Not);
    let bp = b.user("b", fx.point_ty(), Mutability::Not);
    let bb1 = make_line(&mut b, &fx, l);
    b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    b.assign(
        bb1,
        local(x),
        Rvalue::Use(copy(point_x(line_field(&fx, l, 1)))),
    );
    b.assign(bb1, local(bp), use_move(line_field(&fx, l, 1)));
    b.ret(bb1);
    fx.assert_clean(&b.finish());
}

#[test]
fn moving_the_same_field_twice() {
    // let a = l.A; let b = l.A;
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Not);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let c = b.user("c", fx.point_ty(), Mutability::Not);
    let bb1 = make_line(&mut b, &fx, l);
    let moved = b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let used = b.assign(bb1, local(c), use_move(line_field(&fx, l, 0)));
    b.ret(bb1);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Mut);
    let t1 = b.temp(Ty::Unit);
    let t2 = b.temp(Ty::Unit);
    let (bb0, bb1, bb2) = (b.block(), b.block(), b.block());
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.call(bb0, fx.take, vec![mv(local(p))], local(t1), bb1);
    b.assign(bb1, local(p), point_aggregate(&fx));
    b.call(bb1, fx.take, vec![mv(local(p))], local(t2), bb2);
    b.ret(bb2);
    fx.assert_clean(&b.finish());
}

#[test]
fn reinitialisation_by_call_destination_is_accepted() {
    // TakeLine(l); l = MakeLine(); TakeLine(l);
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Mut);
    let t1 = b.temp(Ty::Unit);
    let t2 = b.temp(Ty::Unit);
    let bb1 = make_line(&mut b, &fx, l);
    let (bb2, bb3, bb4) = (b.block(), b.block(), b.block());
    b.call(bb1, fx.take_line, vec![mv(local(l))], local(t1), bb2);
    b.call(bb2, fx.make_line, vec![], local(l), bb3);
    b.call(bb3, fx.take_line, vec![mv(local(l))], local(t2), bb4);
    b.ret(bb4);
    fx.assert_clean(&b.finish());
}

#[test]
fn reinitialising_the_field_then_using_the_parent_is_accepted() {
    // let a = l.A; l.A = Point {..}; TakeLine(l);
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Mut);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::Unit);
    let bb1 = make_line(&mut b, &fx, l);
    let end = b.block();
    b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    b.assign(bb1, line_field(&fx, l, 0), point_aggregate(&fx));
    b.call(bb1, fx.take_line, vec![mv(local(l))], local(t), end);
    b.ret(end);
    fx.assert_clean(&b.finish());
}

#[test]
fn reinitialising_the_parent_clears_the_children() {
    // let a = l.A; l = MakeLine(); let b = l.A;
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Mut);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let c = b.user("c", fx.point_ty(), Mutability::Not);
    let bb1 = make_line(&mut b, &fx, l);
    let bb2 = b.block();
    b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    b.call(bb1, fx.make_line, vec![], local(l), bb2);
    b.assign(bb2, local(c), use_move(line_field(&fx, l, 0)));
    b.ret(bb2);
    fx.assert_clean(&b.finish());
}

#[test]
fn reinitialising_one_field_leaves_the_other_moved() {
    // let a = l.A; let b = l.B; l.A = Point {..}; TakeLine(l);
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Mut);
    let a = b.user("a", fx.point_ty(), Mutability::Not);
    let c = b.user("c", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::Unit);
    let bb1 = make_line(&mut b, &fx, l);
    let end = b.block();
    b.assign(bb1, local(a), use_move(line_field(&fx, l, 0)));
    let moved_b = b.assign(bb1, local(c), use_move(line_field(&fx, l, 1)));
    b.assign(bb1, line_field(&fx, l, 0), point_aggregate(&fx));
    let used = b.call(bb1, fx.take_line, vec![mv(local(l))], local(t), end);
    b.ret(end);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::Unit);
    let (head, body_bb, back) = (b.block(), b.block(), b.block());
    b.goto(head, body_bb);
    b.live(body_bb, p);
    b.assign(body_bb, local(p), point_aggregate(&fx));
    b.call(body_bb, fx.take, vec![mv(local(p))], local(t), back);
    b.dead(back, p);
    b.goto(back, head);
    fx.assert_clean(&b.finish());
}

// ----------------------------------------------------------------------------------------
// E0407: move out of a borrow
// ----------------------------------------------------------------------------------------

#[test]
fn move_out_of_shared_reference() {
    // fn f(r: &Point) { let p = *r; }
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.point_ty())),
        Mutability::Not,
    );
    let p = b.user("p", fx.point_ty(), Mutability::Not);
    let bb0 = b.block();
    let site = b.assign(bb0, local(p), use_move(local(r).deref()));
    b.ret(bb0);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg(
        "r",
        Ty::Ref(Mutability::Mut, Box::new(fx.line_ty())),
        Mutability::Not,
    );
    let t = b.temp(Ty::Unit);
    let (bb0, bb1) = (b.block(), b.block());
    let site = b.call(
        bb0,
        fx.take,
        vec![mv(local(r).deref().field(0, fx.point_ty()))],
        local(t),
        bb1,
    );
    b.ret(bb1);
    let body = b.finish();

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
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg(
        "r",
        Ty::Ref(Mutability::Not, Box::new(fx.point_ty())),
        Mutability::Not,
    );
    let x = b.user("x", Ty::I32, Mutability::Not);
    let y = b.user("y", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    b.assign(bb0, local(x), Rvalue::Use(copy(point_x(local(r).deref()))));
    b.assign(bb0, local(y), Rvalue::Use(copy(point_x(local(r).deref()))));
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

// ----------------------------------------------------------------------------------------
// Copy types and temporaries
// ----------------------------------------------------------------------------------------

#[test]
fn copy_struct_is_never_reported() {
    // let v = Vec2 {..}; TakeVec(v); TakeVec(v); let w = v;
    let fx = Fixture::new();
    assert!(fx.typeck.items.is_copy(&fx.vec2_ty()));
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let v = b.user("v", fx.vec2_ty(), Mutability::Not);
    let w = b.user("w", fx.vec2_ty(), Mutability::Not);
    let t1 = b.temp(Ty::Unit);
    let t2 = b.temp(Ty::Unit);
    let (bb0, bb1, bb2) = (b.block(), b.block(), b.block());
    b.assign(
        bb0,
        local(v),
        Rvalue::Aggregate(fx.vec2, vec![int(1), int(2)]),
    );
    b.call(bb0, fx.take_vec, vec![copy(local(v))], local(t1), bb1);
    b.call(bb1, fx.take_vec, vec![copy(local(v))], local(t2), bb2);
    b.assign(bb2, local(w), Rvalue::Use(copy(local(v))));
    b.ret(bb2);
    fx.assert_clean(&b.finish());
}

#[test]
fn temporaries_are_tracked_but_never_reported() {
    // _t = Point {..}; l.A = move _t; TakeLine(l);  and a (malformed) double move of _t.
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.user("l", fx.line_ty(), Mutability::Mut);
    let t = b.temp(fx.point_ty());
    let u = b.temp(fx.point_ty());
    let unit_t = b.temp(Ty::Unit);
    let bb1 = make_line(&mut b, &fx, l);
    let end = b.block();
    b.live(bb1, t);
    b.assign(bb1, local(t), point_aggregate(&fx));
    b.assign(bb1, line_field(&fx, l, 0), use_move(local(t)));
    b.assign(bb1, local(u), use_move(local(t)));
    b.dead(bb1, t);
    b.call(bb1, fx.take_line, vec![mv(local(l))], local(unit_t), end);
    b.assign(end, local(Local::RETURN), unit());
    b.ret(end);
    fx.assert_clean(&b.finish());
}
