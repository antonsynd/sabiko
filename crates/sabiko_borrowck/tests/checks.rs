//! Mutability and initialisation checks over hand-built MIR.
//!
//! `ResolveResults`/`TypeckResults` come from a tiny real program so that struct and
//! field names exist; the bodies themselves are built by hand with [`BodyBuilder`], so
//! the tests do not depend on MIR lowering.

use sabiko_borrowck::{check_all, check_body, describe_place};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink};
use sabiko_mir::{
    AggregateKind, BasicBlock, BasicBlockData, BinOp, Body, ConstValue, Constant, Local, LocalDecl,
    LocalKind, Mutability, Operand, Place, Rvalue, Statement, StatementKind, Terminator,
    TerminatorKind, Ty,
};
use sabiko_resolve::{DefId, DefKind, LocalId, ResolveResults};
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

struct Guard {
    Id: i32,
}

impl Drop for Guard {
    fn drop(&mut self) {}
}

fn Callee(n: i32) -> i32 {
    n
}

fn Main(a: i32, flag: bool) {
    let x = 1;
    let mut p = Point { X: 1, Y: 2 };
    let r = &p;
    let l = Line { A: Point { X: 0, Y: 0 }, B: Point { X: 1, Y: 1 } };
    let y = Callee(a);
    let _ = r;
    let _ = l;
    let _ = y;
    let _ = x;
    let _ = flag;
}
";

struct Fixture {
    resolve: ResolveResults,
    typeck: TypeckResults,
    point: DefId,
    line: DefId,
    guard: DefId,
    callee: DefId,
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
            guard: def(DefKind::Struct, "Guard"),
            callee: def(DefKind::Fn, "Callee"),
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

    /// The resolver's binding of `name` in `Main`, when the fixture program has one.
    fn local_source(&self, name: &str) -> Option<LocalId> {
        self.resolve
            .locals
            .iter()
            .position(|l| l.name == name && l.owner == self.main)
            .map(|i| LocalId(u32::try_from(i).unwrap()))
    }

    fn check(&self, body: &Body) -> Vec<Diagnostic> {
        let mut sink = DiagnosticSink::new();
        check_body(body, &self.resolve, &self.typeck, &mut sink);
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
                cleanups: Vec::new(),
                span: Span::new(FileId(0), 0, 1),
            },
            next_span: 1000,
        };
        b.decl(ret, Mutability::Mut, LocalKind::Return, None, None);
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
        source: Option<LocalId>,
    ) -> Local {
        let span = self.span();
        self.body.locals.push(LocalDecl {
            ty,
            mutability,
            kind,
            name: name.map(str::to_owned),
            span,
            source,
        });
        Local(u32::try_from(self.body.locals.len() - 1).unwrap())
    }

    /// Arguments must be declared before any other local.
    fn arg(&mut self, name: &str, ty: Ty, mutability: Mutability) -> Local {
        assert_eq!(self.body.locals.len(), self.body.arg_count + 1);
        self.body.arg_count += 1;
        self.decl(ty, mutability, LocalKind::Arg, Some(name), None)
    }

    fn user(&mut self, fx: &Fixture, name: &str, ty: Ty, mutability: Mutability) -> Local {
        let source = fx.local_source(name);
        self.decl(ty, mutability, LocalKind::User, Some(name), source)
    }

    fn temp(&mut self, ty: Ty) -> Local {
        self.decl(ty, Mutability::Mut, LocalKind::Temp, None, None)
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

fn copy(local: Local) -> Operand {
    Operand::Copy(Place::local(local))
}

fn use_int(v: i128) -> Rvalue {
    Rvalue::Use(int(v))
}

fn use_copy(local: Local) -> Rvalue {
    Rvalue::Use(copy(local))
}

fn local(l: Local) -> Place {
    Place::local(l)
}

fn point_aggregate(fx: &Fixture) -> Rvalue {
    Rvalue::Aggregate(AggregateKind::Adt(fx.point), vec![int(1), int(2)])
}

// ----------------------------------------------------------------------------------------
// E0400: assign twice to immutable
// ----------------------------------------------------------------------------------------

#[test]
fn assign_twice_to_immutable_local() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, x);
    let first = b.assign(bb0, local(x), use_int(1));
    let second = b.assign(bb0, local(x), use_int(2));
    b.dead(bb0, x);
    b.ret(bb0);
    let body = b.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0400 cannot assign twice to immutable variable `x`"]
    );
    let decl_span = fx.resolve.local(fx.local_source("x").unwrap()).span;
    assert_eq!(
        labels(&diags[0]),
        [
            (second, "cannot assign twice to immutable variable"),
            (first, "first assignment to `x`"),
            (
                decl_span,
                "help: consider making this binding mutable: `mut x`"
            ),
        ]
    );
    assert!(diags[0].labels[0].primary && !diags[0].labels[1].primary);
}

#[test]
fn assign_in_loop_is_assign_twice() {
    // let x; loop { x = 1; }
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    let bb1 = b.block();
    b.live(bb0, x);
    b.goto(bb0, bb1);
    let assign = b.assign(bb1, local(x), use_int(1));
    b.goto(bb1, bb1);
    let body = b.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0400 cannot assign twice to immutable variable `x`"]
    );
    // The only assignment is its own first assignment: no duplicate label on it.
    assert_eq!(
        diags[0].labels.iter().filter(|l| l.span == assign).count(),
        1
    );
}

#[test]
fn deferred_init_is_accepted() {
    // let x; x = 1; let y = x;
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let y = b.user(&fx, "y", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, x);
    b.assign(bb0, local(x), use_int(1));
    b.live(bb0, y);
    b.assign(bb0, local(y), use_copy(x));
    b.dead(bb0, y);
    b.dead(bb0, x);
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn place_mention_of_an_uninitialised_local_is_accepted() {
    // let x; let _ = x;
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, x);
    b.stmt(bb0, StatementKind::PlaceMention(local(x)));
    b.dead(bb0, x);
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn deferred_init_in_both_arms_is_accepted() {
    // let x; if flag { x = 1 } else { x = 2 } ; _0 = x
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::I32);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let (bb0, then, els, join) = (b.block(), b.block(), b.block(), b.block());
    b.live(bb0, x);
    b.branch(bb0, copy(flag), then, els);
    b.assign(then, local(x), use_int(1));
    b.goto(then, join);
    b.assign(els, local(x), use_int(2));
    b.goto(els, join);
    b.assign(join, local(Local::RETURN), use_copy(x));
    b.dead(join, x);
    b.ret(join);
    fx.assert_clean(&b.finish());
}

#[test]
fn assign_after_maybe_init_is_assign_twice() {
    // let x; if flag { x = 1 } ; x = 2
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let (bb0, then, join) = (b.block(), b.block(), b.block());
    b.live(bb0, x);
    b.branch(bb0, copy(flag), then, join);
    let first = b.assign(then, local(x), use_int(1));
    b.goto(then, join);
    let second = b.assign(join, local(x), use_int(2));
    b.ret(join);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0400 cannot assign twice to immutable variable `x`"]
    );
    assert_eq!(diags[0].primary_span(), Some(second));
    assert!(labels(&diags[0]).contains(&(first, "first assignment to `x`")));
}

#[test]
fn assign_to_immutable_argument() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let n = b.arg("n", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    let span = b.assign(bb0, local(n), use_int(3));
    b.ret(bb0);
    let body = b.finish();
    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0400 cannot assign twice to immutable variable `n`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (span, "cannot assign twice to immutable variable"),
            (
                body.local_decl(n).span,
                "help: consider making this binding mutable: `mut n`"
            ),
        ]
    );
}

#[test]
fn assign_to_mutable_argument_and_local_is_fine() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let n = b.arg("n", Ty::I32, Mutability::Mut);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Mut);
    let bb0 = b.block();
    b.assign(bb0, local(n), use_int(3));
    b.live(bb0, x);
    b.assign(bb0, local(x), use_int(1));
    b.assign(bb0, local(x), use_copy(n));
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn storage_live_in_loop_resets_initialisation() {
    // loop { let x; x = 1; }
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let (bb0, bb1) = (b.block(), b.block());
    b.goto(bb0, bb1);
    b.live(bb1, x);
    b.assign(bb1, local(x), use_int(1));
    b.dead(bb1, x);
    b.goto(bb1, bb1);
    fx.assert_clean(&b.finish());
}

// ----------------------------------------------------------------------------------------
// E0401 / E0403: assignments to projected places
// ----------------------------------------------------------------------------------------

#[test]
fn field_assign_on_immutable_local() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user(&fx, "p", fx.point_ty(), Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, p);
    b.assign(bb0, local(p), point_aggregate(&fx));
    let span = b.assign(bb0, local(p).field(0, Ty::I32), use_int(3));
    b.ret(bb0);
    let body = b.finish();
    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0401 cannot assign to `p.X`, as `p` is not declared as mutable"]
    );
    let decl_span = fx.resolve.local(fx.local_source("p").unwrap()).span;
    assert_eq!(
        labels(&diags[0]),
        [
            (span, "cannot assign"),
            (
                decl_span,
                "help: consider changing this to be mutable: `mut p`"
            ),
        ]
    );
}

#[test]
fn nested_field_assign_on_immutable_argument() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let l = b.arg("l", fx.line_ty(), Mutability::Not);
    let bb0 = b.block();
    b.assign(
        bb0,
        local(l).field(0, fx.point_ty()).field(1, Ty::I32),
        use_int(0),
    );
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0401 cannot assign to `l.A.Y`, as `l` is not declared as mutable"]
    );
}

#[test]
fn field_assign_on_mutable_local_is_fine() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.user(&fx, "p", fx.point_ty(), Mutability::Mut);
    let bb0 = b.block();
    b.live(bb0, p);
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.assign(bb0, local(p).field(0, Ty::I32), use_int(3));
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn writes_through_mut_ref_with_immutable_binding_are_fine() {
    // fn Shift(p: &mut Point) { p.X = 1; (*p).Y = 0; let q = &mut *p; q.X = 2; *n = 1 }
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.arg("p", Ty::mut_ref(fx.point_ty()), Mutability::Not);
    let n = b.arg("n", Ty::mut_ref(Ty::I32), Mutability::Not);
    let q = b.user(&fx, "q", Ty::mut_ref(fx.point_ty()), Mutability::Not);
    let bb0 = b.block();
    b.assign(bb0, local(p).deref().field(0, Ty::I32), use_int(1));
    b.assign(bb0, local(p).deref().field(1, Ty::I32), use_int(0));
    b.live(bb0, q);
    b.assign(
        bb0,
        local(q),
        Rvalue::Ref(Mutability::Mut.into(), local(p).deref()),
    );
    b.assign(bb0, local(q).deref().field(0, Ty::I32), use_int(2));
    b.assign(bb0, local(n).deref(), use_int(1));
    b.dead(bb0, q);
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn assign_through_shared_ref() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = b.block();
    let span = b.assign(bb0, local(r).deref(), use_int(1));
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0403 cannot assign to `*r`, which is behind a `&` reference"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            span,
            "`r` is a `&` reference, so the data it refers to cannot be written"
        )]
    );
}

#[test]
fn mut_binding_does_not_allow_assign_through_shared_ref() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg("r", Ty::shared_ref(Ty::I32), Mutability::Mut);
    let bb0 = b.block();
    b.assign(bb0, local(r).deref(), use_int(1));
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0403 cannot assign to `*r`, which is behind a `&` reference"]
    );
}

#[test]
fn field_assign_through_shared_ref_elides_the_deref() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.arg("p", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let bb0 = b.block();
    b.assign(bb0, local(p).deref().field(0, Ty::I32), use_int(1));
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0403 cannot assign to `p.X`, which is behind a `&` reference"]
    );
    assert_eq!(
        diags[0].labels[0].message,
        "`p` is a `&` reference, so the data it refers to cannot be written"
    );
}

#[test]
fn shared_ref_behind_mut_ref_names_the_inner_reference() {
    // r: &mut &i32; **r = 1
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg("r", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    let bb0 = b.block();
    b.assign(bb0, local(r).deref().deref(), use_int(1));
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0403 cannot assign to `**r`, which is behind a `&` reference"]
    );
    assert_eq!(
        diags[0].labels[0].message,
        "`*r` is a `&` reference, so the data it refers to cannot be written"
    );
}

// ----------------------------------------------------------------------------------------
// E0402 / E0404: mutable borrows
// ----------------------------------------------------------------------------------------

#[test]
fn borrow_mut_of_immutable_local() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let t = b.temp(Ty::mut_ref(Ty::I32));
    let bb0 = b.block();
    b.live(bb0, x);
    b.assign(bb0, local(x), use_int(1));
    let span = b.assign(bb0, local(t), Rvalue::Ref(Mutability::Mut.into(), local(x)));
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0402 cannot borrow `x` as mutable, as `x` is not declared as mutable"]
    );
    let decl_span = fx.resolve.local(fx.local_source("x").unwrap()).span;
    assert_eq!(
        labels(&diags[0]),
        [
            (span, "cannot borrow as mutable"),
            (
                decl_span,
                "help: consider changing this to be mutable: `mut x`"
            ),
        ]
    );
}

#[test]
fn borrow_mut_of_field_of_immutable_argument() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.arg("p", fx.point_ty(), Mutability::Not);
    let t = b.temp(Ty::mut_ref(Ty::I32));
    let bb0 = b.block();
    b.assign(
        bb0,
        local(t),
        Rvalue::Ref(Mutability::Mut.into(), local(p).field(0, Ty::I32)),
    );
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0402 cannot borrow `p.X` as mutable, as `p` is not declared as mutable"]
    );
}

#[test]
fn borrow_mut_of_mutable_places_is_fine() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Mut);
    let p = b.user(&fx, "p", fx.point_ty(), Mutability::Mut);
    let t = b.temp(Ty::mut_ref(Ty::I32));
    let bb0 = b.block();
    b.live(bb0, x);
    b.assign(bb0, local(x), use_int(1));
    b.assign(bb0, local(t), Rvalue::Ref(Mutability::Mut.into(), local(x)));
    b.live(bb0, p);
    b.assign(bb0, local(p), point_aggregate(&fx));
    b.assign(
        bb0,
        local(t),
        Rvalue::Ref(Mutability::Mut.into(), local(p).field(1, Ty::I32)),
    );
    // Shared borrows never care about mutability.
    let shared = b.temp(Ty::shared_ref(Ty::I32));
    let other = b.user(&fx, "y", Ty::I32, Mutability::Not);
    b.live(bb0, other);
    b.assign(bb0, local(other), use_int(1));
    b.assign(
        bb0,
        local(shared),
        Rvalue::Ref(Mutability::Not.into(), local(other)),
    );
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn reborrow_mut_through_shared_ref() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let m = b.user(&fx, "m", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, m);
    let span = b.assign(
        bb0,
        local(m),
        Rvalue::Ref(Mutability::Mut.into(), local(r).deref()),
    );
    b.assign(bb0, local(m).deref(), use_int(2));
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0404 cannot borrow `*r` as mutable, as it is behind a `&` reference"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            span,
            "`r` is a `&` reference, so the data it refers to cannot be borrowed as mutable"
        )]
    );
}

#[test]
fn reborrow_mut_field_through_shared_ref() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.arg("p", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let t = b.temp(Ty::mut_ref(Ty::I32));
    let bb0 = b.block();
    b.assign(
        bb0,
        local(t),
        Rvalue::Ref(Mutability::Mut.into(), local(p).deref().field(1, Ty::I32)),
    );
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0404 cannot borrow `p.Y` as mutable, as it is behind a `&` reference"]
    );
}

#[test]
fn reborrow_mut_through_mut_ref_is_fine() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.arg("r", Ty::mut_ref(Ty::I32), Mutability::Not);
    let p = b.arg("p", Ty::mut_ref(fx.point_ty()), Mutability::Not);
    let t = b.temp(Ty::mut_ref(Ty::I32));
    let bb0 = b.block();
    b.assign(
        bb0,
        local(t),
        Rvalue::Ref(Mutability::Mut.into(), local(r).deref()),
    );
    b.assign(
        bb0,
        local(t),
        Rvalue::Ref(Mutability::Mut.into(), local(p).deref().field(1, Ty::I32)),
    );
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

// ----------------------------------------------------------------------------------------
// E0405: uses of uninitialised locals
// ----------------------------------------------------------------------------------------

#[test]
fn use_before_init() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::I32);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, x);
    let span = b.assign(bb0, local(Local::RETURN), use_copy(x));
    b.dead(bb0, x);
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0405 used binding `x` isn't initialized"]
    );
    let decl_span = fx.resolve.local(fx.local_source("x").unwrap()).span;
    assert_eq!(
        labels(&diags[0]),
        [
            (span, "`x` used here but it isn't initialized"),
            (decl_span, "binding declared here but left uninitialized"),
        ]
    );
}

#[test]
fn partial_init_through_field_is_a_use_and_reported_once() {
    // let mut p: Point; p.X = 1; _0 = p.X
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::I32);
    let p = b.user(&fx, "p", fx.point_ty(), Mutability::Mut);
    let bb0 = b.block();
    b.live(bb0, p);
    let span = b.assign(bb0, local(p).field(0, Ty::I32), use_int(1));
    b.assign(
        bb0,
        local(Local::RETURN),
        Rvalue::Use(Operand::Copy(local(p).field(0, Ty::I32))),
    );
    b.ret(bb0);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0405 used binding `p` isn't initialized"]
    );
    assert_eq!(diags[0].primary_span(), Some(span));
}

#[test]
fn borrow_of_uninitialised_local() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let r = b.user(&fx, "r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, x);
    b.live(bb0, r);
    b.assign(bb0, local(r), Rvalue::Ref(Mutability::Not.into(), local(x)));
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0405 used binding `x` isn't initialized"]
    );
}

#[test]
fn deref_reads_the_reference_local() {
    // let r: &mut i32; *r = 1;
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let r = b.user(&fx, "r", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, r);
    b.assign(bb0, local(r).deref(), use_int(1));
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0405 used binding `r` isn't initialized"]
    );
}

#[test]
fn possibly_uninitialised_when_one_arm_assigns() {
    // let x; if flag { x = 1 } ; _0 = x
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::I32);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let (bb0, then, join) = (b.block(), b.block(), b.block());
    b.live(bb0, x);
    b.branch(bb0, copy(flag), then, join);
    b.assign(then, local(x), use_int(1));
    b.goto(then, join);
    let span = b.assign(join, local(Local::RETURN), use_copy(x));
    b.ret(join);
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0405 used binding `x` is possibly-uninitialized"]
    );
    assert_eq!(
        diags[0].labels[0],
        sabiko_diagnostics::Label {
            span,
            message: "`x` used here but it is possibly-uninitialized".to_owned(),
            primary: true,
        }
    );
}

#[test]
fn possibly_uninitialised_after_while_loop() {
    // let x; let mut i = 0; while i < n { x = i; i = i + 1 } ; _0 = x
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::I32);
    let n = b.arg("n", Ty::I32, Mutability::Not);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Mut);
    let i = b.user(&fx, "i", Ty::I32, Mutability::Mut);
    let cond = b.temp(Ty::BOOL);
    let (bb0, head, body_bb, exit) = (b.block(), b.block(), b.block(), b.block());
    b.live(bb0, x);
    b.live(bb0, i);
    b.assign(bb0, local(i), use_int(0));
    b.goto(bb0, head);
    b.assign(
        head,
        local(cond),
        Rvalue::BinaryOp(BinOp::Lt, copy(i), copy(n)),
    );
    b.branch(head, copy(cond), body_bb, exit);
    b.assign(body_bb, local(x), use_copy(i));
    b.assign(
        body_bb,
        local(i),
        Rvalue::BinaryOp(BinOp::Add, copy(i), int(1)),
    );
    b.goto(body_bb, head);
    b.assign(exit, local(Local::RETURN), use_copy(x));
    b.ret(exit);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0405 used binding `x` is possibly-uninitialized"]
    );
}

#[test]
fn switch_discriminant_and_call_arguments_are_uses() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let flag = b.user(&fx, "flag", Ty::BOOL, Mutability::Not);
    let a = b.user(&fx, "a", Ty::I32, Mutability::Not);
    let y = b.user(&fx, "y", Ty::I32, Mutability::Not);
    let (bb0, bb1, bb2) = (b.block(), b.block(), b.block());
    b.live(bb0, flag);
    b.live(bb0, a);
    b.live(bb0, y);
    b.branch(bb0, copy(flag), bb1, bb1);
    b.terminate(
        bb1,
        TerminatorKind::Call {
            func: fx.callee,
            args: vec![copy(a)],
            destination: local(y),
            target: Some(bb2),
        },
    );
    b.assign(bb2, local(Local::RETURN), use_copy(y));
    b.ret(bb2);
    assert_eq!(
        fx.messages(&b.finish()),
        [
            "E0405 used binding `flag` isn't initialized",
            "E0405 used binding `a` isn't initialized",
        ]
    );
}

#[test]
fn call_destination_initialises_and_counts_as_assignment() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let a = b.arg("a", Ty::I32, Mutability::Not);
    let y = b.user(&fx, "y", Ty::I32, Mutability::Not);
    let (bb0, bb1) = (b.block(), b.block());
    b.live(bb0, y);
    let first = b.terminate(
        bb0,
        TerminatorKind::Call {
            func: fx.callee,
            args: vec![copy(a)],
            destination: local(y),
            target: Some(bb1),
        },
    );
    let second = b.terminate(
        bb1,
        TerminatorKind::Call {
            func: fx.callee,
            args: vec![copy(y)],
            destination: local(y),
            target: None,
        },
    );
    let diags = fx.check(&b.finish());
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0400 cannot assign twice to immutable variable `y`"]
    );
    assert_eq!(diags[0].primary_span(), Some(second));
    assert!(labels(&diags[0]).contains(&(first, "first assignment to `y`")));
}

#[test]
fn call_destination_projection_is_checked() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let p = b.arg("p", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let bb0 = b.block();
    b.terminate(
        bb0,
        TerminatorKind::Call {
            func: fx.callee,
            args: vec![int(1)],
            destination: local(p).deref().field(0, Ty::I32),
            target: None,
        },
    );
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0403 cannot assign to `p.X`, which is behind a `&` reference"]
    );
}

#[test]
fn use_after_storage_dead_is_uninitialised() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::I32);
    let x = b.user(&fx, "x", Ty::I32, Mutability::Not);
    let bb0 = b.block();
    b.live(bb0, x);
    b.assign(bb0, local(x), use_int(1));
    b.dead(bb0, x);
    b.assign(bb0, local(Local::RETURN), use_copy(x));
    b.ret(bb0);
    assert_eq!(
        fx.messages(&b.finish()),
        ["E0405 used binding `x` isn't initialized"]
    );
}

// ----------------------------------------------------------------------------------------
// Temporaries and the return place
// ----------------------------------------------------------------------------------------

#[test]
fn temps_and_return_place_are_never_reported() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, fx.point_ty());
    let t = b.temp(Ty::I32);
    let u = b.temp(fx.point_ty());
    let bb0 = b.block();
    // Temp read before any write, written twice; `_0` written twice and partially.
    b.live(bb0, t);
    b.assign(bb0, local(Local::RETURN).field(0, Ty::I32), use_copy(t));
    b.assign(bb0, local(t), use_int(1));
    b.assign(bb0, local(t), use_int(2));
    b.assign(bb0, local(u).field(1, Ty::I32), use_copy(t));
    b.assign(bb0, local(Local::RETURN), point_aggregate(&fx));
    b.assign(
        bb0,
        local(Local::RETURN),
        Rvalue::Use(Operand::Move(local(u))),
    );
    b.dead(bb0, t);
    b.ret(bb0);
    fx.assert_clean(&b.finish());
}

#[test]
fn temps_render_as_underscore_n() {
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let t = b.temp(Ty::shared_ref(fx.point_ty()));
    let bb0 = b.block();
    b.assign(bb0, local(t).deref().field(1, Ty::I32), use_int(1));
    b.ret(bb0);
    let body = b.finish();
    assert_eq!(
        describe_place(&local(t).deref().field(1, Ty::I32), &body, &fx.typeck),
        "_1.Y"
    );
    assert_eq!(describe_place(&local(t).deref(), &body, &fx.typeck), "*_1");
    assert_eq!(
        fx.messages(&body),
        ["E0403 cannot assign to `_1.Y`, which is behind a `&` reference"]
    );
}

#[test]
fn check_all_walks_every_body() {
    let fx = Fixture::new();
    let mut ok = BodyBuilder::new(&fx, Ty::Unit);
    let bb = ok.block();
    ok.ret(bb);

    let mut bad = BodyBuilder::new(&fx, Ty::Unit);
    let n = bad.arg("n", Ty::I32, Mutability::Not);
    let bb = bad.block();
    bad.assign(bb, local(n), use_int(1));
    bad.ret(bb);

    let mut sink = DiagnosticSink::new();
    check_all(
        &[ok.finish(), bad.finish()],
        &fx.resolve,
        &fx.typeck,
        &mut sink,
    );
    assert_eq!(
        sink.diagnostics().iter().map(render).collect::<Vec<_>>(),
        ["E0400 cannot assign twice to immutable variable `n`"]
    );
}

#[test]
fn drop_of_a_maybe_uninitialised_immutable_local_is_accepted() {
    // let g: Guard; if flag { g = Guard {..} } drop(g);
    // The drop runs `drop(&mut g)` only where `g` holds a value (elaboration's flag), so it
    // is neither a use of a possibly uninitialised `g` (E0381) nor a mutation of an
    // immutable binding.
    let fx = Fixture::new();
    let mut b = BodyBuilder::new(&fx, Ty::Unit);
    let flag = b.arg("flag", Ty::BOOL, Mutability::Not);
    let g = b.user(&fx, "g", Ty::Adt(fx.guard), Mutability::Not);
    let (bb0, then, join, end) = (b.block(), b.block(), b.block(), b.block());
    b.live(bb0, g);
    b.branch(bb0, copy(flag), then, join);
    b.assign(
        then,
        local(g),
        Rvalue::Aggregate(AggregateKind::Adt(fx.guard), vec![int(1)]),
    );
    b.goto(then, join);
    b.terminate(
        join,
        TerminatorKind::Drop {
            place: local(g),
            target: end,
        },
    );
    b.dead(end, g);
    b.ret(end);
    fx.assert_clean(&b.finish());
}
