//! Copy forwarding: an optimisation run after borrow checking, before code generation
//! (decision 51).
//!
//! MIR building copies every operand into a fresh temporary when it is evaluated
//! (decision 36): `a + b` becomes `_3 = copy a; _4 = copy b; _2 = Add(move _3, move _4)`.
//! The copies keep the order of reads right (an operand is read before a later sibling
//! operand runs) and give the borrow checker rustc's MIR shape, so building keeps them.
//! Once the borrow checker is done they only cost IL size and locals, so this pass
//! forwards the source place into the single use of such a temporary and deletes the
//! copy, when that cannot change the value read:
//!
//! - the temporary `t` is a compiler temporary, assigned exactly once, by
//!   `t = copy p` or `t = move p` of a place of exactly its type, and mentioned exactly
//!   once more, as a whole-local operand (`copy t`/`move t`) later in the same block,
//!   in a statement or the block's terminator; it is never borrowed or projected;
//! - the local of the source `p` is never borrowed anywhere in the body (no `&p`,
//!   `&p.F`, ...; a reborrow through a reference, `&(*p).F`, does not count), so no
//!   reference can reach its storage;
//! - no statement strictly between the copy and the use assigns to a place based on
//!   `p`'s local or ends or restarts its storage; and when `p` reads through a reference
//!   (`(*r).F`), none assigns through a reference or to a borrowed local either, which
//!   are the only writes that can reach memory behind a reference. Calls are terminators,
//!   so none can run in between; when the use is a call's argument, arguments are read
//!   before the call.
//!
//! Under those conditions nothing between the copy and the use can write the memory the
//! place reads, so reading it later gives the same value as reading the copy. Every
//! operand that outlives a call (`F(p.A, Bump(&mut p))`, the miscompile decision 36
//! fixed) or a write that may alias it keeps its temporary. A temporary assigned a copy
//! and never read is deleted too (reads have no side effects). The pass removes the
//! storage markers of the temporaries it deletes, and the `nop`s it leaves.

use crate::{
    Body, Local, LocalKind, Operand, Place, PlaceElem, Rvalue, StatementKind, TerminatorKind,
};

/// Runs copy forwarding on every body.
pub fn forward_all(bodies: &mut [Body]) {
    for body in bodies {
        forward_body(body);
    }
}

/// Runs copy forwarding on one body until no candidate is left.
pub fn forward_body(body: &mut Body) {
    let mut changed = false;
    while let Some(candidate) = find_candidate(body) {
        apply(body, &candidate);
        changed = true;
    }
    if changed {
        for data in &mut body.blocks {
            data.statements
                .retain(|s| !matches!(s.kind, StatementKind::Nop));
        }
    }
}

/// How the places of a body mention each local.
#[derive(Default, Clone)]
struct Mentions {
    /// Whole-local assignments (`_t = ...`, a call's destination `_t`).
    defs: usize,
    /// Whole-local operands (`copy _t`, `move _t`).
    uses: usize,
    /// Anything else: a projected place based on the local (read or written) or a
    /// borrow of it or of part of it.
    other: usize,
    /// Borrowed without a dereference in the borrowed place: its storage is reachable.
    borrowed: bool,
}

fn mentions(body: &Body) -> Vec<Mentions> {
    let mut m = vec![Mentions::default(); body.locals.len()];
    let operand = |m: &mut Vec<Mentions>, op: &Operand| {
        if let Some(p) = op.place() {
            if p.is_local() {
                m[p.local.index()].uses += 1;
            } else {
                m[p.local.index()].other += 1;
            }
        }
    };
    let write = |m: &mut Vec<Mentions>, p: &Place| {
        if p.is_local() {
            m[p.local.index()].defs += 1;
        } else {
            m[p.local.index()].other += 1;
        }
    };
    for data in &body.blocks {
        for stmt in &data.statements {
            if let StatementKind::PlaceMention(place) = &stmt.kind {
                // A mention keeps its local (and the temporary it names) in the body.
                m[place.local.index()].other += 1;
                continue;
            }
            let StatementKind::Assign(place, rvalue) = &stmt.kind else {
                continue;
            };
            write(&mut m, place);
            match rvalue {
                Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => {
                    operand(&mut m, op);
                }
                Rvalue::BinaryOp(_, l, r) => {
                    operand(&mut m, l);
                    operand(&mut m, r);
                }
                Rvalue::Aggregate(_, ops) => {
                    for op in ops {
                        operand(&mut m, op);
                    }
                }
                Rvalue::Ref(_, p) => {
                    let entry = &mut m[p.local.index()];
                    entry.other += 1;
                    if !p.projection.contains(&PlaceElem::Deref) {
                        entry.borrowed = true;
                    }
                }
            }
        }
        match &data.terminator().kind {
            TerminatorKind::SwitchInt { discr, .. } => operand(&mut m, discr),
            TerminatorKind::Call {
                args, destination, ..
            } => {
                for a in args {
                    operand(&mut m, a);
                }
                write(&mut m, destination);
            }
            // A dropped local is never forwarded (nor is anything with drop glue below).
            TerminatorKind::Drop { place, .. } => m[place.local.index()].other += 1,
            TerminatorKind::Goto { .. } | TerminatorKind::Return | TerminatorKind::Unreachable => {}
        }
    }
    // A value with drop glue is dropped where its cleanup scopes and drops say: forwarding
    // it would move it at another point, changing what a panic in between drops and in
    // which order (decision 55). Every such local that is ever written has a cleanup scope.
    for cleanup in &body.cleanups {
        m[cleanup.local.index()].other += 1;
        for drop in &cleanup.drops {
            m[drop.place.local.index()].other += 1;
        }
    }
    m
}

/// Where a candidate's copy and use are.
struct Candidate {
    block: usize,
    /// Index of the statement `t = copy p`.
    def: usize,
    temp: Local,
    /// The operand that replaces the use (`copy p` or `move p`) and where the use is.
    /// `None` when `t` is never read and the copy is simply deleted.
    source: Option<(Operand, UseSite)>,
}

/// Where in its block a temporary is read.
#[derive(Clone, Copy, Debug)]
enum UseSite {
    Statement(usize),
    Terminator,
}

fn find_candidate(body: &Body) -> Option<Candidate> {
    let m = mentions(body);
    for (b, data) in body.blocks.iter().enumerate() {
        for (i, stmt) in data.statements.iter().enumerate() {
            let StatementKind::Assign(dest, Rvalue::Use(source)) = &stmt.kind else {
                continue;
            };
            let Some(src) = source.place() else {
                continue;
            };
            if !dest.is_local() {
                continue;
            }
            let temp = dest.local;
            let decl = body.local_decl(temp);
            let tm = &m[temp.index()];
            if decl.kind != LocalKind::Temp || tm.defs != 1 || tm.other != 0 || src.local == temp {
                continue;
            }
            if tm.uses == 0 {
                return Some(Candidate {
                    block: b,
                    def: i,
                    temp,
                    source: None,
                });
            }
            let sm = &m[src.local.index()];
            if tm.uses != 1 || sm.borrowed || src.ty(body) != decl.ty {
                continue;
            }
            let Some(site) = find_use(body, &m, b, i, temp, src) else {
                continue;
            };
            return Some(Candidate {
                block: b,
                def: i,
                temp,
                source: Some((source.clone(), site)),
            });
        }
    }
    None
}

/// The use of `temp` after statement `def` of block `b`, if it is in that block and no
/// statement in between may write the memory `src` reads or touches its storage.
fn find_use(
    body: &Body,
    m: &[Mentions],
    b: usize,
    def: usize,
    temp: Local,
    src: &Place,
) -> Option<UseSite> {
    let through_ref = src.projection.contains(&PlaceElem::Deref);
    let data = &body.blocks[b];
    for (j, stmt) in data.statements.iter().enumerate().skip(def + 1) {
        if statement_reads(&stmt.kind, temp) {
            return Some(UseSite::Statement(j));
        }
        match &stmt.kind {
            StatementKind::Assign(place, _) => {
                if place.local == src.local {
                    return None;
                }
                // Memory behind a reference is changed only through a reference or by a
                // write to a local some reference may point to.
                if through_ref
                    && (place.projection.contains(&PlaceElem::Deref)
                        || m[place.local.index()].borrowed)
                {
                    return None;
                }
            }
            StatementKind::StorageLive(l) | StatementKind::StorageDead(l) if *l == src.local => {
                return None;
            }
            _ => {}
        }
    }
    let reads = match &data.terminator().kind {
        TerminatorKind::SwitchInt { discr, .. } => is_temp(discr, temp),
        TerminatorKind::Call { args, .. } => args.iter().any(|a| is_temp(a, temp)),
        TerminatorKind::Goto { .. }
        | TerminatorKind::Drop { .. }
        | TerminatorKind::Return
        | TerminatorKind::Unreachable => false,
    };
    reads.then_some(UseSite::Terminator)
}

fn is_temp(op: &Operand, temp: Local) -> bool {
    op.place().is_some_and(|p| p.is_local() && p.local == temp)
}

fn statement_reads(kind: &StatementKind, temp: Local) -> bool {
    let StatementKind::Assign(_, rvalue) = kind else {
        return false;
    };
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => is_temp(op, temp),
        Rvalue::BinaryOp(_, l, r) => is_temp(l, temp) || is_temp(r, temp),
        Rvalue::Aggregate(_, ops) => ops.iter().any(|o| is_temp(o, temp)),
        Rvalue::Ref(..) => false,
    }
}

fn replace(op: &mut Operand, temp: Local, source: &Operand) -> bool {
    if is_temp(op, temp) {
        *op = source.clone();
        true
    } else {
        false
    }
}

fn apply(body: &mut Body, c: &Candidate) {
    let data = &mut body.blocks[c.block];
    if let Some((source, site)) = &c.source {
        let replaced = match *site {
            UseSite::Statement(j) => match &mut data.statements[j].kind {
                StatementKind::Assign(_, rvalue) => match rvalue {
                    Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => {
                        replace(op, c.temp, source)
                    }
                    Rvalue::BinaryOp(_, l, r) => {
                        replace(l, c.temp, source) || replace(r, c.temp, source)
                    }
                    Rvalue::Aggregate(_, ops) => ops.iter_mut().any(|o| replace(o, c.temp, source)),
                    Rvalue::Ref(..) => false,
                },
                _ => false,
            },
            UseSite::Terminator => match &mut data.terminator.as_mut().expect("terminator").kind {
                TerminatorKind::SwitchInt { discr, .. } => replace(discr, c.temp, source),
                TerminatorKind::Call { args, .. } => {
                    args.iter_mut().any(|a| replace(a, c.temp, source))
                }
                _ => false,
            },
        };
        assert!(replaced, "copy forwarding lost the use of _{}", c.temp.0);
    }
    data.statements[c.def].kind = StatementKind::Nop;
    // The temporary is gone; so are its storage markers, wherever they are.
    for data in &mut body.blocks {
        for stmt in &mut data.statements {
            if let StatementKind::StorageLive(l) | StatementKind::StorageDead(l) = stmt.kind {
                if l == c.temp {
                    stmt.kind = StatementKind::Nop;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Hand-built bodies: one test per condition of the pass, each with the smallest body
    //! where breaking that condition would change what the program computes.

    use sabiko_resolve::DefId;
    use sabiko_span::Span;

    use super::forward_body;
    use crate::{
        BasicBlock, BasicBlockData, BinOp, Body, ConstValue, Constant, Local, LocalDecl, LocalKind,
        Mutability, Operand, Place, Rvalue, Statement, StatementKind, Terminator, TerminatorKind,
        Ty,
    };

    fn decl(kind: LocalKind, ty: Ty) -> LocalDecl {
        LocalDecl {
            ty,
            mutability: Mutability::Mut,
            kind,
            name: None,
            span: Span::dummy(),
            source: None,
        }
    }

    /// `_0: i32`, `_1: i32` (argument), then the given locals.
    fn body(locals: &[(LocalKind, Ty)], blocks: Vec<BasicBlockData>) -> Body {
        let mut all = vec![
            decl(LocalKind::Return, Ty::I32),
            decl(LocalKind::Arg, Ty::I32),
        ];
        all.extend(locals.iter().map(|(k, t)| decl(*k, t.clone())));
        Body {
            def: DefId(0),
            name: "F".to_owned(),
            locals: all,
            arg_count: 1,
            blocks,
            cleanups: Vec::new(),
            span: Span::dummy(),
        }
    }

    fn stmt(kind: StatementKind) -> Statement {
        Statement {
            kind,
            span: Span::dummy(),
        }
    }

    fn assign(local: u32, rvalue: Rvalue) -> Statement {
        stmt(StatementKind::Assign(Place::local(Local(local)), rvalue))
    }

    fn copy(local: u32) -> Operand {
        Operand::Copy(Place::local(Local(local)))
    }

    fn one() -> Operand {
        Operand::Constant(Constant {
            ty: Ty::I32,
            value: ConstValue::Int(1),
            span: Span::dummy(),
        })
    }

    fn block(statements: Vec<Statement>, kind: TerminatorKind) -> BasicBlockData {
        BasicBlockData {
            statements,
            terminator: Some(Terminator {
                kind,
                span: Span::dummy(),
            }),
            cleanup: None,
        }
    }

    fn ret(statements: Vec<Statement>) -> BasicBlockData {
        block(statements, TerminatorKind::Return)
    }

    fn add(l: Operand, r: Operand) -> Rvalue {
        Rvalue::BinaryOp(BinOp::Add, l, r)
    }

    const TEMP: (LocalKind, Ty) = (LocalKind::Temp, Ty::I32);
    const USER: (LocalKind, Ty) = (LocalKind::User, Ty::I32);

    /// Runs the pass and returns the statements of `bb0`.
    fn forwarded(mut b: Body) -> Vec<StatementKind> {
        forward_body(&mut b);
        b.blocks[0]
            .statements
            .iter()
            .map(|s| s.kind.clone())
            .collect()
    }

    fn kept(out: &[StatementKind], local: u32, rvalue: Rvalue) -> bool {
        out.contains(&StatementKind::Assign(Place::local(Local(local)), rvalue))
    }

    #[test]
    fn forwards_a_copy_into_its_single_use() {
        // _2 = copy _1; _0 = Add(copy _2, 1)  =>  _0 = Add(copy _1, 1), markers gone.
        let b = body(
            &[TEMP],
            vec![ret(vec![
                stmt(StatementKind::StorageLive(Local(2))),
                assign(2, Rvalue::Use(copy(1))),
                assign(0, add(copy(2), one())),
                stmt(StatementKind::StorageDead(Local(2))),
            ])],
        );
        assert_eq!(
            forwarded(b),
            vec![StatementKind::Assign(
                Place::local(Local::RETURN),
                add(copy(1), one())
            )]
        );
    }

    #[test]
    fn forwards_chains_and_into_terminators() {
        // _2 = copy _1; _3 = copy _2; switchInt(copy _3)  =>  switchInt(copy _1)
        let mut b = body(
            &[TEMP, TEMP],
            vec![
                block(
                    vec![
                        assign(2, Rvalue::Use(copy(1))),
                        assign(3, Rvalue::Use(copy(2))),
                    ],
                    TerminatorKind::SwitchInt {
                        discr: copy(3),
                        targets: vec![(0, BasicBlock(1))],
                        otherwise: BasicBlock(1),
                    },
                ),
                ret(vec![]),
            ],
        );
        forward_body(&mut b);
        assert!(b.blocks[0].statements.is_empty());
        let TerminatorKind::SwitchInt { discr, .. } = &b.blocks[0].terminator().kind else {
            panic!("the terminator changed")
        };
        assert_eq!(*discr, copy(1));
    }

    #[test]
    fn a_write_to_the_source_in_the_using_statement_is_fine() {
        // _3 = copy _2 (a user local); _2 = Add(copy _3, 1): the rvalue reads _2 before
        // the store, so `_2 = Add(copy _2, 1)` computes the same.
        let b = body(
            &[USER, TEMP],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(3, Rvalue::Use(copy(2))),
                assign(2, add(copy(3), one())),
                assign(0, Rvalue::Use(copy(2))),
            ])],
        );
        let out = forwarded(b);
        assert!(kept(&out, 2, add(copy(2), one())), "{out:?}");
        assert!(!kept(&out, 3, Rvalue::Use(copy(2))), "{out:?}");
    }

    /// `_2` is a user local `= copy _1`; `_3 = copy _2`, then `between`, then
    /// `_0 = Add(copy _3, copy _2)`. Returns whether `_3`'s copy survived.
    fn copy_kept_with(between: Vec<Statement>, extra: Vec<(LocalKind, Ty)>) -> bool {
        let mut locals = vec![USER, TEMP];
        locals.extend(extra);
        let mut stmts = vec![
            assign(2, Rvalue::Use(copy(1))),
            assign(3, Rvalue::Use(copy(2))),
        ];
        stmts.extend(between);
        stmts.push(assign(0, add(copy(3), copy(2))));
        kept(
            &forwarded(body(&locals, vec![ret(stmts)])),
            3,
            Rvalue::Use(copy(2)),
        )
    }

    #[test]
    fn keeps_the_copy_when_the_source_is_written_before_the_use() {
        assert!(copy_kept_with(vec![assign(2, Rvalue::Use(one()))], vec![]));
        // A write to another local does not matter.
        assert!(!copy_kept_with(
            vec![assign(4, Rvalue::Use(one()))],
            vec![USER]
        ));
    }

    #[test]
    fn keeps_the_copy_when_the_source_storage_ends_or_restarts() {
        assert!(copy_kept_with(
            vec![stmt(StatementKind::StorageDead(Local(2)))],
            vec![]
        ));
        assert!(copy_kept_with(
            vec![stmt(StatementKind::StorageLive(Local(2)))],
            vec![]
        ));
    }

    #[test]
    fn keeps_the_copy_when_the_source_is_borrowed_anywhere() {
        // A borrow, even after the use, means a reference may write the source.
        let r = Ty::Ref(Mutability::Mut, Box::new(Ty::I32));
        let b = body(
            &[USER, TEMP, (LocalKind::User, r)],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(3, Rvalue::Use(copy(2))),
                assign(0, add(copy(3), one())),
                assign(
                    4,
                    Rvalue::Ref(Mutability::Mut.into(), Place::local(Local(2))),
                ),
            ])],
        );
        let out = forwarded(b);
        assert!(kept(&out, 3, Rvalue::Use(copy(2))), "{out:?}");
    }

    /// `_1: &mut i32`; `_2 = copy (*_1)`, then `between`, then `_0 = Add(copy _2, 1)`.
    /// Returns whether `_2`'s copy survived.
    fn deref_copy_kept_with(between: Vec<Statement>, extra: Vec<(LocalKind, Ty)>) -> bool {
        let mut locals = vec![TEMP];
        locals.extend(extra);
        let deref = Operand::Copy(Place::local(Local(1)).deref());
        let mut stmts = vec![assign(2, Rvalue::Use(deref.clone()))];
        stmts.extend(between);
        stmts.push(assign(0, add(copy(2), one())));
        let mut b = body(&locals, vec![ret(stmts)]);
        b.locals[1].ty = Ty::Ref(Mutability::Mut, Box::new(Ty::I32));
        kept(&forwarded(b), 2, Rvalue::Use(deref))
    }

    #[test]
    fn forwards_reads_through_a_reference_only_past_writes_that_cannot_reach_it() {
        let r = Ty::Ref(Mutability::Mut, Box::new(Ty::I32));
        // Nothing in between, or a write to a local no reference can reach: forwarded.
        assert!(!deref_copy_kept_with(vec![], vec![]));
        assert!(!deref_copy_kept_with(
            vec![assign(3, Rvalue::Use(one()))],
            vec![USER]
        ));
        // A write through a reference may write the referent.
        let through = stmt(StatementKind::Assign(
            Place::local(Local(3)).deref(),
            Rvalue::Use(one()),
        ));
        assert!(deref_copy_kept_with(
            vec![through],
            vec![(LocalKind::User, r.clone())]
        ));
        // A write to a borrowed local may write the referent: `_1` may point to `_3`.
        assert!(deref_copy_kept_with(
            vec![
                assign(
                    4,
                    Rvalue::Ref(Mutability::Mut.into(), Place::local(Local(3)))
                ),
                assign(3, Rvalue::Use(one())),
            ],
            vec![USER, (LocalKind::User, r)]
        ));
        // Reassigning the reference itself changes what the place reads.
        assert!(deref_copy_kept_with(
            vec![assign(1, Rvalue::Use(copy(1)))],
            vec![]
        ));
    }

    #[test]
    fn keeps_copies_used_in_another_block_or_twice() {
        let b = body(
            &[TEMP],
            vec![
                block(
                    vec![assign(2, Rvalue::Use(copy(1)))],
                    TerminatorKind::Goto {
                        target: BasicBlock(1),
                    },
                ),
                ret(vec![assign(0, add(copy(2), one()))]),
            ],
        );
        assert_eq!(forwarded(b).len(), 1);
        let b = body(
            &[TEMP],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(0, add(copy(2), copy(2))),
            ])],
        );
        assert_eq!(forwarded(b).len(), 2);
    }

    #[test]
    fn keeps_user_locals_borrowed_temps_and_retyped_copies() {
        // A user local is not a temporary.
        let b = body(
            &[USER],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(0, add(copy(2), one())),
            ])],
        );
        assert_eq!(forwarded(b).len(), 2);
        // A copy into a temporary of another type keeps its temporary.
        let b = body(
            &[(LocalKind::Temp, Ty::Prim(sabiko_resolve::PrimTy::I64))],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(0, add(copy(2), one())),
            ])],
        );
        assert_eq!(forwarded(b).len(), 2);
        // A temporary that is also borrowed keeps its copy.
        let r = Ty::Ref(Mutability::Not, Box::new(Ty::I32));
        let b = body(
            &[TEMP, (LocalKind::Temp, r)],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(
                    3,
                    Rvalue::Ref(Mutability::Not.into(), Place::local(Local(2))),
                ),
                assign(0, add(copy(2), one())),
            ])],
        );
        assert_eq!(forwarded(b).len(), 3);
    }

    #[test]
    fn deletes_copies_nothing_reads() {
        let b = body(
            &[TEMP],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(0, Rvalue::Use(one())),
            ])],
        );
        assert_eq!(
            forwarded(b),
            vec![StatementKind::Assign(
                Place::local(Local::RETURN),
                Rvalue::Use(one())
            )]
        );
    }

    #[test]
    fn keeps_temporaries_assigned_twice() {
        // _2 = copy _1; _2 = 1; _0 = Add(copy _2, 1): the use reads the second value.
        let b = body(
            &[TEMP],
            vec![ret(vec![
                assign(2, Rvalue::Use(copy(1))),
                assign(2, Rvalue::Use(one())),
                assign(0, add(copy(2), one())),
            ])],
        );
        let out = forwarded(b);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(kept(&out, 0, add(copy(2), one())), "{out:?}");
    }
}
