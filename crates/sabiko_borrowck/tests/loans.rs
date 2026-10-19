//! Loan checks over hand-built MIR.
//!
//! As in `checks.rs`, `ResolveResults`/`TypeckResults` come from a tiny real program so
//! that struct names and function signatures exist; the bodies are built by hand with
//! [`BodyBuilder`] and run through `loans::check_loans` alone, so these tests depend
//! neither on MIR lowering nor on the other borrow checks.

use sabiko_borrowck::loans::check_loans;
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

fn First(p: &Point) -> &i32 {
    &p.X
}

fn Two(a: &i32, b: &i32) -> &i32 {
    a
}

fn Short(x: &mut &i32, y: &i32) {
    *x = y;
}

fn Deep(x: &mut &i32, y: &&i32) {}

fn Ret(p: &i32) -> &i32 {
    p
}

fn Main() {
    let mut x = 1;
    let r = &x;
    let _ = r;
    x = 2;
}
";

struct Fixture {
    resolve: ResolveResults,
    typeck: TypeckResults,
    point: DefId,
    first: DefId,
    two: DefId,
    short: DefId,
    deep: DefId,
    ret: DefId,
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
            first: def(DefKind::Fn, "First"),
            two: def(DefKind::Fn, "Two"),
            short: def(DefKind::Fn, "Short"),
            deep: def(DefKind::Fn, "Deep"),
            ret: def(DefKind::Fn, "Ret"),
            main: def(DefKind::Fn, "Main"),
            resolve,
            typeck,
        }
    }

    fn point_ty(&self) -> Ty {
        Ty::Adt(self.point)
    }

    fn check(&self, body: &Body) -> Vec<Diagnostic> {
        let mut sink = DiagnosticSink::new();
        check_loans(body, &self.resolve, &self.typeck, &mut sink);
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
    /// A body for `Main`, returning `()`.
    fn new(fx: &Fixture) -> Self {
        Self::for_fn(fx.main, "Main", Ty::Unit)
    }

    /// A body for the function `def`, whose signature the facts are built from.
    fn for_fn(def: DefId, name: &str, ret: Ty) -> Self {
        let mut mir = Self {
            body: Body {
                def,
                name: name.to_owned(),
                locals: Vec::new(),
                arg_count: 0,
                blocks: Vec::new(),
                span: Span::new(FileId(0), 0, 1),
            },
            next_span: 1000,
        };
        mir.decl(ret, Mutability::Mut, LocalKind::Return, None);
        mir
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

    /// `_0 = const (); return`.
    fn finish_unit(&mut self, bb: BasicBlock) -> Span {
        self.assign(bb, local(Local::RETURN), unit());
        self.terminate(bb, TerminatorKind::Return)
    }

    fn ret(&mut self, bb: BasicBlock) -> Span {
        self.terminate(bb, TerminatorKind::Return)
    }

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

fn copy(place: Place) -> Operand {
    Operand::Copy(place)
}

fn mv(place: Place) -> Operand {
    Operand::Move(place)
}

fn use_int(v: i128) -> Rvalue {
    Rvalue::Use(int(v))
}

fn use_copy(place: Place) -> Rvalue {
    Rvalue::Use(copy(place))
}

fn local(l: Local) -> Place {
    Place::local(l)
}

fn deref(l: Local) -> Place {
    Place::local(l).deref()
}

fn shared(place: Place) -> Rvalue {
    Rvalue::Ref(Mutability::Not, place)
}

fn mutable(place: Place) -> Rvalue {
    Rvalue::Ref(Mutability::Mut, place)
}

fn point_aggregate(fx: &Fixture) -> Rvalue {
    Rvalue::Aggregate(fx.point, vec![int(1), int(2)])
}

// ----------------------------------------------------------------------------------------
// E0408 / E0409: conflicting borrows
// ----------------------------------------------------------------------------------------

#[test]
fn mutable_borrow_while_shared_borrow_is_live() {
    // let r = &x; let m = &mut x; *m = 2; let t = *r;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let m = mir.user("m", Ty::mut_ref(Ty::I32), Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, x);
    mir.assign(bb0, local(x), use_int(1));
    mir.live(bb0, r);
    let first = mir.assign(bb0, local(r), shared(local(x)));
    mir.live(bb0, m);
    let second = mir.assign(bb0, local(m), mutable(local(x)));
    mir.assign(bb0, deref(m), use_int(2));
    mir.live(bb0, t);
    let later = mir.assign(bb0, local(t), use_copy(deref(r)));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0408 cannot borrow `x` as mutable because it is also borrowed as immutable"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (second, "mutable borrow occurs here"),
            (first, "immutable borrow occurs here"),
            (later, "borrow later used here"),
        ]
    );
    assert!(diags[0].labels[0].primary && !diags[0].labels[1].primary);
}

#[test]
fn shared_borrow_while_mutable_borrow_is_live() {
    // let m = &mut x; let r = &x; *m = 2;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let m = mir.user("m", Ty::mut_ref(Ty::I32), Mutability::Not);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    let first = mir.assign(bb0, local(m), mutable(local(x)));
    let second = mir.assign(bb0, local(r), shared(local(x)));
    let later = mir.assign(bb0, deref(m), use_int(2));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0408 cannot borrow `x` as immutable because it is also borrowed as mutable"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (second, "immutable borrow occurs here"),
            (first, "mutable borrow occurs here"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn two_mutable_borrows() {
    // let a = &mut x; let b = &mut x; *a = 1; *b = 2;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let a = mir.user("a", Ty::mut_ref(Ty::I32), Mutability::Not);
    let c = mir.user("c", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    let first = mir.assign(bb0, local(a), mutable(local(x)));
    let second = mir.assign(bb0, local(c), mutable(local(x)));
    let later = mir.assign(bb0, deref(a), use_int(1));
    mir.assign(bb0, deref(c), use_int(2));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0409 cannot borrow `x` as mutable more than once at a time"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (second, "second mutable borrow occurs here"),
            (first, "first mutable borrow occurs here"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn two_shared_borrows_are_fine() {
    // let r = &x; let s = &x; let t = *r + *s;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let s = mir.user("s", Ty::shared_ref(Ty::I32), Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(r), shared(local(x)));
    mir.assign(bb0, local(s), shared(local(x)));
    mir.assign(
        bb0,
        local(t),
        Rvalue::BinaryOp(sabiko_mir::BinOp::Add, copy(deref(r)), copy(deref(s))),
    );
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn borrow_ends_at_its_last_use() {
    // let r = &mut x; *r = 1; x = 2; let s = &x; x = 3;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::mut_ref(Ty::I32), Mutability::Not);
    let s = mir.user("s", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(r), mutable(local(x)));
    mir.assign(bb0, deref(r), use_int(1));
    mir.assign(bb0, local(x), use_int(2));
    mir.assign(bb0, local(s), shared(local(x)));
    mir.assign(bb0, local(x), use_int(3));
    mir.dead(bb0, s);
    mir.dead(bb0, r);
    mir.dead(bb0, x);
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

// ----------------------------------------------------------------------------------------
// E0410 / E0411 / E0412: access while borrowed
// ----------------------------------------------------------------------------------------

#[test]
fn assign_while_borrowed() {
    // let r = &mut x; x = 2; *r = 1;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    let borrow = mir.assign(bb0, local(r), mutable(local(x)));
    let assign = mir.assign(bb0, local(x), use_int(2));
    let later = mir.assign(bb0, deref(r), use_int(1));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0410 cannot assign to `x` because it is borrowed"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (assign, "assignment to borrowed `x` occurs here"),
            (borrow, "`x` is borrowed here"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn assign_to_field_while_struct_is_borrowed() {
    // let r = &p; p.X = 3; let t = (*r).Y;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let r = mir.user("r", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.assign(bb0, local(r), shared(local(p)));
    mir.assign(bb0, local(p).field(0, Ty::I32), use_int(3));
    mir.assign(bb0, local(t), use_copy(deref(r).field(1, Ty::I32)));
    mir.finish_unit(bb0);
    let body = mir.finish();

    assert_eq!(
        fx.messages(&body),
        ["E0410 cannot assign to `p.X` because it is borrowed"]
    );
}

#[test]
fn use_while_mutably_borrowed() {
    // let r = &mut x; let y = x; *r = y;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::mut_ref(Ty::I32), Mutability::Not);
    let y = mir.user("y", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    let borrow = mir.assign(bb0, local(r), mutable(local(x)));
    let read = mir.assign(bb0, local(y), use_copy(local(x)));
    let later = mir.assign(bb0, deref(r), use_copy(local(y)));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0411 cannot use `x` because it was mutably borrowed"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (read, "use of borrowed `x`"),
            (borrow, "`x` is borrowed here"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn read_while_shared_borrowed_is_fine() {
    // let r = &x; let y = x; let t = *r;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let y = mir.user("y", Ty::I32, Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(r), shared(local(x)));
    mir.assign(bb0, local(y), use_copy(local(x)));
    mir.assign(bb0, local(t), use_copy(deref(r)));
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn move_while_borrowed() {
    // let r = &p; let q = p; let t = (*r).X;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let p = mir.user("p", fx.point_ty(), Mutability::Not);
    let r = mir.user("r", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let q = mir.user("q", fx.point_ty(), Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(p), point_aggregate(&fx));
    let borrow = mir.assign(bb0, local(r), shared(local(p)));
    let moved = mir.assign(bb0, local(q), Rvalue::Use(mv(local(p))));
    let later = mir.assign(bb0, local(t), use_copy(deref(r).field(0, Ty::I32)));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0412 cannot move out of `p` because it is borrowed"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (moved, "move out of `p` occurs here"),
            (borrow, "borrow of `p` occurs here"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn moving_a_reference_whose_referent_is_reborrowed() {
    // let t = &mut *m; let m2 = m; *t = 1;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let m = mir.user("m", Ty::mut_ref(Ty::I32), Mutability::Not);
    let t = mir.user("t", Ty::mut_ref(Ty::I32), Mutability::Not);
    let m2 = mir.user("m2", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(m), mutable(local(x)));
    mir.assign(bb0, local(t), mutable(deref(m)));
    mir.assign(bb0, local(m2), Rvalue::Use(mv(local(m))));
    mir.assign(bb0, deref(t), use_int(1));
    mir.assign(bb0, deref(m2), use_int(2));
    mir.finish_unit(bb0);
    let body = mir.finish();

    assert_eq!(
        fx.messages(&body),
        ["E0412 cannot move out of `m` because it is borrowed"]
    );
}

// ----------------------------------------------------------------------------------------
// E0413 / E0414: the referent dies first
// ----------------------------------------------------------------------------------------

#[test]
fn does_not_live_long_enough() {
    // let r; { let x = 1; r = &x; } let t = *r;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, r);
    mir.live(bb0, x);
    mir.assign(bb0, local(x), use_int(1));
    let borrow = mir.assign(bb0, local(r), shared(local(x)));
    let drop = mir.dead(bb0, x);
    mir.live(bb0, t);
    let later = mir.assign(bb0, local(t), use_copy(deref(r)));
    mir.dead(bb0, t);
    mir.dead(bb0, r);
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0413 `x` does not live long enough"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (borrow, "borrowed value does not live long enough"),
            (drop, "`x` dropped here while still borrowed"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn storage_dead_after_last_use_is_fine() {
    // { let x = 1; let r = &x; let t = *r; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, x);
    mir.assign(bb0, local(x), use_int(1));
    mir.live(bb0, r);
    mir.assign(bb0, local(r), shared(local(x)));
    mir.live(bb0, t);
    mir.assign(bb0, local(t), use_copy(deref(r)));
    mir.dead(bb0, t);
    mir.dead(bb0, r);
    mir.dead(bb0, x);
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn temporary_dropped_while_borrowed() {
    // let s = &<temp>; <temp dies>; let t = *s;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let s = mir.user("s", Ty::shared_ref(Ty::I32), Mutability::Not);
    let tmp = mir.temp(Ty::I32);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, s);
    mir.live(bb0, tmp);
    mir.assign(bb0, local(tmp), use_int(0));
    let borrow = mir.assign(bb0, local(s), shared(local(tmp)));
    let drop = mir.dead(bb0, tmp);
    let later = mir.assign(bb0, local(t), use_copy(deref(s)));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0413 temporary value dropped while borrowed"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (
                borrow,
                "creates a temporary value which is freed while still in use"
            ),
            (
                drop,
                "temporary value is freed at the end of this statement"
            ),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn return_reference_to_local() {
    // fn Ret(p: &i32) -> &i32 { let x = 1; &x }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.ret, "Ret", Ty::shared_ref(Ty::I32));
    mir.arg("p", Ty::shared_ref(Ty::I32), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, x);
    mir.assign(bb0, local(x), use_int(1));
    let borrow = mir.assign(bb0, local(Local::RETURN), shared(local(x)));
    mir.dead(bb0, x);
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0414 cannot return reference to local variable `x`"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            borrow,
            "returns a reference to data owned by the current function"
        )]
    );
}

#[test]
fn return_reference_to_local_without_storage_dead() {
    // The same, with the local's storage never ended explicitly: `return` ends it.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.ret, "Ret", Ty::shared_ref(Ty::I32));
    mir.arg("p", Ty::shared_ref(Ty::I32), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(Local::RETURN), shared(local(x)));
    mir.ret(bb0);
    let body = mir.finish();

    assert_eq!(
        fx.messages(&body),
        ["E0414 cannot return reference to local variable `x`"]
    );
}

#[test]
fn return_reference_to_parameter_data_is_fine() {
    // fn First(p: &Point) -> &i32 { &(*p).X }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.first, "First", Ty::shared_ref(Ty::I32));
    let p = mir.arg("p", Ty::shared_ref(fx.point_ty()), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(
        bb0,
        local(Local::RETURN),
        shared(deref(p).field(0, Ty::I32)),
    );
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn return_parameter_copied_through_a_local_is_fine() {
    // fn Ret(p: &i32) -> &i32 { let q = p; &*q }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.ret, "Ret", Ty::shared_ref(Ty::I32));
    let p = mir.arg("p", Ty::shared_ref(Ty::I32), Mutability::Not);
    let q = mir.user("q", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, q);
    mir.assign(bb0, local(q), use_copy(local(p)));
    mir.assign(bb0, local(Local::RETURN), shared(deref(q)));
    mir.dead(bb0, q);
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

// ----------------------------------------------------------------------------------------
// Precision: disjoint fields, kills, reborrows, calls
// ----------------------------------------------------------------------------------------

#[test]
fn disjoint_fields_are_fine() {
    // let a = &mut p.X; let b = &mut p.Y; *a = 1; *b = 2;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let a = mir.user("a", Ty::mut_ref(Ty::I32), Mutability::Not);
    let c = mir.user("c", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.assign(bb0, local(a), mutable(local(p).field(0, Ty::I32)));
    mir.assign(bb0, local(c), mutable(local(p).field(1, Ty::I32)));
    mir.assign(bb0, deref(a), use_int(1));
    mir.assign(bb0, deref(c), use_int(2));
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reassigning_the_reference_kills_its_loan() {
    // let mut r = &mut x; r = &mut y; x = 5; *r = 1;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let y = mir.user("y", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::mut_ref(Ty::I32), Mutability::Mut);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(y), use_int(1));
    mir.assign(bb0, local(r), mutable(local(x)));
    mir.assign(bb0, local(r), mutable(local(y)));
    mir.assign(bb0, local(x), use_int(5));
    mir.assign(bb0, deref(r), use_int(1));
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reborrow_then_use_of_original_is_fine_after_last_use() {
    // let m = &mut x; let m2 = &mut *m; *m2 = 1; *m = 2;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let m = mir.user("m", Ty::mut_ref(Ty::I32), Mutability::Not);
    let m2 = mir.user("m2", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(m), mutable(local(x)));
    mir.assign(bb0, local(m2), mutable(deref(m)));
    mir.assign(bb0, deref(m2), use_int(1));
    mir.assign(bb0, deref(m), use_int(2));
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn write_through_original_while_reborrow_is_live() {
    // let m = &mut x; let m2 = &mut *m; *m = 2; *m2 = 1;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let m = mir.user("m", Ty::mut_ref(Ty::I32), Mutability::Not);
    let m2 = mir.user("m2", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(m), mutable(local(x)));
    mir.assign(bb0, local(m2), mutable(deref(m)));
    mir.assign(bb0, deref(m), use_int(2));
    mir.assign(bb0, deref(m2), use_int(1));
    mir.finish_unit(bb0);
    let body = mir.finish();

    assert_eq!(
        fx.messages(&body),
        ["E0410 cannot assign to `*m` because it is borrowed"]
    );
}

#[test]
fn loan_in_a_loop_ends_before_the_next_write() {
    // loop { let w = &mut x; *w = 1; x = 2; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let w = mir.user("w", Ty::mut_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.goto(bb0, bb1);
    mir.live(bb1, w);
    mir.assign(bb1, local(w), mutable(local(x)));
    mir.assign(bb1, deref(w), use_int(1));
    mir.assign(bb1, local(x), use_int(2));
    mir.dead(bb1, w);
    mir.goto(bb1, bb1);
    fx.assert_clean(&mir.finish());
}

#[test]
fn loan_carried_around_a_loop_is_live_at_the_write() {
    // let mut r = &x; loop { x = 2; let t = *r; r = &x; }
    // The loan issued at the end of one iteration is live at the write of the next.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Mut);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(r), shared(local(x)));
    mir.goto(bb0, bb1);
    mir.assign(bb1, local(x), use_int(2));
    mir.assign(bb1, local(t), use_copy(deref(r)));
    mir.assign(bb1, local(r), shared(local(x)));
    mir.goto(bb1, bb1);
    let body = mir.finish();

    // One error per loan: the initial borrow and the one made in the loop.
    assert_eq!(
        fx.messages(&body),
        [
            "E0410 cannot assign to `x` because it is borrowed",
            "E0410 cannot assign to `x` because it is borrowed",
        ]
    );
}

#[test]
fn dead_store_of_the_reference_ends_the_loan() {
    // let mut r = &x; loop { let t = *r; x = 2; r = &x; }
    // `r` is overwritten before its next use, so no loan is live at `x = 2`.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let x = mir.user("x", Ty::I32, Mutability::Mut);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Mut);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.assign(bb0, local(x), use_int(1));
    mir.assign(bb0, local(r), shared(local(x)));
    mir.goto(bb0, bb1);
    mir.assign(bb1, local(t), use_copy(deref(r)));
    mir.assign(bb1, local(x), use_int(2));
    mir.assign(bb1, local(r), shared(local(x)));
    mir.goto(bb1, bb1);
    fx.assert_clean(&mir.finish());
}

#[test]
fn call_result_carries_the_argument_loan() {
    // let t = &p; let g = First(copy t); p.X = 5; let u = *g;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let t = mir.temp(Ty::shared_ref(fx.point_ty()));
    let g = mir.user("g", Ty::shared_ref(Ty::I32), Mutability::Not);
    let u = mir.user("u", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.assign(bb0, local(p), point_aggregate(&fx));
    let borrow = mir.assign(bb0, local(t), shared(local(p)));
    mir.call(bb0, fx.first, vec![copy(local(t))], local(g), bb1);
    mir.dead(bb1, t);
    let assign = mir.assign(bb1, local(p).field(0, Ty::I32), use_int(5));
    let later = mir.assign(bb1, local(u), use_copy(deref(g)));
    mir.finish_unit(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0410 cannot assign to `p.X` because it is borrowed"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (assign, "assignment to borrowed `p.X` occurs here"),
            (borrow, "`p` is borrowed here"),
            (later, "borrow later used here"),
        ]
    );
}

#[test]
fn call_result_loan_ends_at_its_last_use() {
    // let t = &p; let g = First(copy t); let u = *g; p.X = 5;
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let p = mir.user("p", fx.point_ty(), Mutability::Mut);
    let t = mir.temp(Ty::shared_ref(fx.point_ty()));
    let g = mir.user("g", Ty::shared_ref(Ty::I32), Mutability::Not);
    let u = mir.user("u", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.assign(bb0, local(p), point_aggregate(&fx));
    mir.assign(bb0, local(t), shared(local(p)));
    mir.call(bb0, fx.first, vec![copy(local(t))], local(g), bb1);
    mir.dead(bb1, t);
    mir.assign(bb1, local(u), use_copy(deref(g)));
    mir.assign(bb1, local(p).field(0, Ty::I32), use_int(5));
    mir.finish_unit(bb1);
    fx.assert_clean(&mir.finish());
}

#[test]
fn call_to_unelidable_signature_constrains_nothing() {
    // let r = Two(&a, &b); a = 2; let t = *r;  -- Two's own body reports E0416.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::new(&fx);
    let a = mir.user("a", Ty::I32, Mutability::Mut);
    let c = mir.user("c", Ty::I32, Mutability::Mut);
    let ra = mir.temp(Ty::shared_ref(Ty::I32));
    let rc = mir.temp(Ty::shared_ref(Ty::I32));
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let t = mir.user("t", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    let bb1 = mir.block();
    mir.assign(bb0, local(a), use_int(1));
    mir.assign(bb0, local(c), use_int(1));
    mir.assign(bb0, local(ra), shared(local(a)));
    mir.assign(bb0, local(rc), shared(local(c)));
    mir.call(
        bb0,
        fx.two,
        vec![copy(local(ra)), copy(local(rc))],
        local(r),
        bb1,
    );
    mir.assign(bb1, local(a), use_int(2));
    mir.assign(bb1, local(t), use_copy(deref(r)));
    mir.finish_unit(bb1);
    fx.assert_clean(&mir.finish());
}

// ----------------------------------------------------------------------------------------
// E0415 / E0416: signatures
// ----------------------------------------------------------------------------------------

#[test]
fn missing_lifetime_specifier() {
    // fn Two(a: &i32, b: &i32) -> &i32 { a }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.two, "Two", Ty::shared_ref(Ty::I32));
    let a = mir.arg("a", Ty::shared_ref(Ty::I32), Mutability::Not);
    mir.arg("b", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(Local::RETURN), use_copy(local(a)));
    mir.ret(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0416 missing lifetime specifier"]
    );
    let name_span = fx.typeck.items.fn_sig(fx.two).name_span;
    assert_eq!(
        labels(&diags[0]),
        [(name_span, "expected named lifetime parameter")]
    );
    assert_eq!(
        diags[0].notes,
        [
            "this function's return type contains a borrowed value, but the signature does \
             not say which one of the parameters it is borrowed from",
            "explicit lifetimes are planned for Phase 9; see docs/PLAN.md",
        ]
    );
}

#[test]
fn lifetime_may_not_live_long_enough() {
    // fn Short(x: &mut &i32, y: &i32) { *x = y; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.short, "Short", Ty::Unit);
    let x = mir.arg("x", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    let y = mir.arg("y", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    let assign = mir.assign(bb0, deref(x), use_copy(local(y)));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0415 lifetime may not live long enough"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            assign,
            "requires that the data `y` points to outlives the data `*x` points to"
        )]
    );
}

#[test]
fn writing_a_parameter_reference_back_into_itself_is_fine() {
    // fn Short(x: &mut &i32, y: &i32) { let z = *x; *x = z; }
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.short, "Short", Ty::Unit);
    let x = mir.arg("x", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    mir.arg("y", Ty::shared_ref(Ty::I32), Mutability::Not);
    let z = mir.user("z", Ty::shared_ref(Ty::I32), Mutability::Not);
    let bb0 = mir.block();
    mir.assign(bb0, local(z), use_copy(deref(x)));
    mir.assign(bb0, deref(x), use_copy(local(z)));
    mir.finish_unit(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn borrow_stored_in_a_parameter_is_used_when_the_function_returns() {
    // fn Short(x: &mut &i32, y: &i32) { let l = 1; *x = &l; }
    // Nothing in the body uses the loan after `l` dies; the caller does, so the "later
    // use" is the function's return.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.short, "Short", Ty::Unit);
    let x = mir.arg("x", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    mir.arg("y", Ty::shared_ref(Ty::I32), Mutability::Not);
    let l = mir.user("l", Ty::I32, Mutability::Not);
    let t = mir.temp(Ty::shared_ref(Ty::I32));
    let (bb0, bb1) = (mir.block(), mir.block());
    mir.live(bb0, l);
    mir.assign(bb0, local(l), use_int(1));
    let borrow = mir.assign(bb0, local(t), shared(local(l)));
    mir.assign(bb0, deref(x), Rvalue::Use(mv(local(t))));
    let dead = mir.dead(bb0, l);
    mir.goto(bb0, bb1);
    let ret = mir.finish_unit(bb1);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0413 `l` does not live long enough"]
    );
    assert_eq!(
        labels(&diags[0]),
        [
            (borrow, "borrowed value does not live long enough"),
            (dead, "`l` dropped here while still borrowed"),
            (ret, "borrow later used here, when the function returns"),
        ]
    );
}

#[test]
fn local_borrow_beside_a_reference_parameter_is_fine() {
    // fn Ret(p: &i32) -> &i32 { let x = 1; let r = &x; let v = *r; p }
    // The parameter's placeholder loan must not be confused with the loan of `x`.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.ret, "Ret", Ty::shared_ref(Ty::I32));
    let p = mir.arg("p", Ty::shared_ref(Ty::I32), Mutability::Not);
    let x = mir.user("x", Ty::I32, Mutability::Not);
    let r = mir.user("r", Ty::shared_ref(Ty::I32), Mutability::Not);
    let v = mir.user("v", Ty::I32, Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, x);
    mir.assign(bb0, local(x), use_int(1));
    mir.live(bb0, r);
    mir.assign(bb0, local(r), shared(local(x)));
    mir.live(bb0, v);
    mir.assign(bb0, local(v), use_copy(deref(r)));
    mir.assign(bb0, local(Local::RETURN), use_copy(local(p)));
    mir.dead(bb0, v);
    mir.dead(bb0, r);
    mir.dead(bb0, x);
    mir.ret(bb0);
    fx.assert_clean(&mir.finish());
}

#[test]
fn reborrow_of_a_parameter_carries_its_lifetime() {
    // fn Short(x: &mut &i32, y: &i32) { *x = &*y; }
    // The reborrow must outlive `*x`, so `y` must: the signature does not say so.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.short, "Short", Ty::Unit);
    let x = mir.arg("x", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    let y = mir.arg("y", Ty::shared_ref(Ty::I32), Mutability::Not);
    let t = mir.temp(Ty::shared_ref(Ty::I32));
    let bb0 = mir.block();
    mir.assign(bb0, local(t), shared(deref(y)));
    let assign = mir.assign(bb0, deref(x), Rvalue::Use(mv(local(t))));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0415 lifetime may not live long enough"]
    );
    assert_eq!(diags[0].primary_span(), Some(assign));
}

#[test]
fn writing_through_a_moved_mut_reference_is_invariant() {
    // fn Short(x: &mut &i32, y: &i32) { let z = x; *z = y; }
    // `&mut &'a i32` must not become `&mut &'short i32` by moving it: the write through
    // `z` is a write through `x`.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.short, "Short", Ty::Unit);
    let x = mir.arg("x", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    let y = mir.arg("y", Ty::shared_ref(Ty::I32), Mutability::Not);
    let z = mir.user("z", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    let bb0 = mir.block();
    mir.live(bb0, z);
    mir.assign(bb0, local(z), Rvalue::Use(mv(local(x))));
    let assign = mir.assign(bb0, deref(z), use_copy(local(y)));
    mir.dead(bb0, z);
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0415 lifetime may not live long enough"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            assign,
            "requires that the data `y` points to outlives the data `*x` points to"
        )]
    );
}

#[test]
fn reborrow_through_shared_references_constrains_only_the_innermost() {
    // fn Deep(x: &mut &i32, y: &&i32) { *x = &**y; }
    // `**y` is reached through a shared `*y`, whose referent is only copied out: the new
    // reference needs `*y` to outlive it, not `y`.
    let fx = Fixture::new();
    let mut mir = BodyBuilder::for_fn(fx.deep, "Deep", Ty::Unit);
    let x = mir.arg("x", Ty::mut_ref(Ty::shared_ref(Ty::I32)), Mutability::Not);
    let y = mir.arg(
        "y",
        Ty::shared_ref(Ty::shared_ref(Ty::I32)),
        Mutability::Not,
    );
    let t = mir.temp(Ty::shared_ref(Ty::I32));
    let bb0 = mir.block();
    mir.assign(bb0, local(t), shared(deref(y).deref()));
    let assign = mir.assign(bb0, deref(x), Rvalue::Use(mv(local(t))));
    mir.finish_unit(bb0);
    let body = mir.finish();

    let diags = fx.check(&body);
    assert_eq!(
        diags.iter().map(render).collect::<Vec<_>>(),
        ["E0415 lifetime may not live long enough"]
    );
    assert_eq!(
        labels(&diags[0]),
        [(
            assign,
            "requires that the data `*y` points to outlives the data `*x` points to"
        )]
    );
}
