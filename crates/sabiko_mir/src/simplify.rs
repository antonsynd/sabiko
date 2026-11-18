//! Control-flow clean-up after lowering.
//!
//! Lowering leaves two kinds of clutter behind: blocks nothing jumps to (the code after a
//! `return` or `break`, the exit of a `loop` nobody leaves) and blocks that only forward to
//! another block (join points with nothing in them). [`simplify_body`] retargets jumps
//! past the forwarding blocks, then drops every block unreachable from `bb0` and renumbers
//! the rest, keeping their relative order, and does the same for the cleanup scopes no
//! block is in any more.

use sabiko_span::fx::HashSet;

use crate::{
    BasicBlock, Body, BorrowKind, CleanupScope, Operand, Place, Rvalue, SliceBounds, Statement,
    StatementKind, Terminator, TerminatorKind, Ty,
};

pub fn simplify_all(bodies: &mut [Body]) {
    for body in bodies {
        simplify_body(body);
    }
}

pub fn simplify_body(body: &mut Body) {
    merge_gotos(body);
    remove_unreachable(body);
    remove_unused_cleanups(body);
}

/// Removes every storage marker, then simplifies. Storage markers only matter to the
/// borrow checker, drop elaboration and copy forwarding; once those ran, a block that
/// only ended the storage of a condition's temporaries is an empty jump like any other.
pub fn strip_storage_markers(bodies: &mut [Body]) {
    for body in bodies {
        for data in &mut body.blocks {
            data.statements.retain(|s| {
                !matches!(
                    s.kind,
                    StatementKind::StorageLive(_) | StatementKind::StorageDead(_)
                )
            });
        }
        simplify_body(body);
    }
}

/// Ends each block at its first read of a value of an uninhabited type (`!`, `&!`, a tuple
/// holding one), which never runs: no such value exists (decision 103). The statement or
/// call that reads it becomes a mention of each indexed place it reads up to that operand,
/// so the bounds checks before it still run in order (`let y = e[i]` for `e: [!; 0]`
/// panics), and the block ends in `unreachable`. Such reads are left in reachable MIR by
/// the arm of a match on a payload of type `!` (`Some(x) => PrintLine(x)` on an
/// `Option<!>`) and in an instance at `!` (`Some(v) => Consume((1, v))` in `Pair::<!>`);
/// code generation has no value to give them, and [`crate::validate::uninhabited_operands`]
/// checks that none is left. A two-phase borrow whose call is removed becomes an ordinary
/// mutable borrow. Runs after borrow checking and copy forwarding; simplification then
/// drops the blocks only the removed code reached.
pub fn cut_uninhabited_reads(body: &mut Body) {
    // An uninhabited operand's type mentions `!`, and so does the type of its local: a
    // struct or variant field's type is `!` only through a generic argument (no user type
    // can write `!`, and no prelude type does).
    if !body.locals.iter().any(|d| mentions_never(&d.ty)) {
        return;
    }
    let mut changed = false;
    for bb in 0..body.blocks.len() {
        let data = &body.blocks[bb];
        let uninhabited = |op: &Operand| op.ty(body).is_uninhabited();
        let in_statement = data.statements.iter().enumerate().find_map(|(i, s)| {
            let StatementKind::Assign(_, rvalue) = &s.kind else {
                return None;
            };
            let n = rvalue_operands(rvalue).position(uninhabited)?;
            Some((i, mentions(rvalue_operands(rvalue).take(n + 1)), s.span))
        });
        // A switch's discriminant is an integer or a `bool`, never uninhabited, so only a
        // call's arguments can end a block at its terminator.
        let cut = in_statement.or_else(|| {
            let term = data.terminator();
            let TerminatorKind::Call { args, .. } = &term.kind else {
                return None;
            };
            let n = args.iter().position(uninhabited)?;
            Some((data.statements.len(), mentions(&args[..=n]), term.span))
        });
        let Some((at, mentions, span)) = cut else {
            continue;
        };
        let data = &mut body.blocks[bb];
        data.statements.truncate(at);
        data.statements
            .extend(mentions.into_iter().map(|place| Statement {
                kind: StatementKind::PlaceMention(place),
                span,
            }));
        data.terminator = Some(Terminator {
            kind: TerminatorKind::Unreachable,
            span,
        });
        changed = true;
    }
    if !changed {
        return;
    }
    simplify_body(body);
    // A two-phase borrow is activated by the call that takes it; with the call gone, it is
    // an ordinary mutable borrow nothing uses.
    let mut call_args = vec![false; body.locals.len()];
    for data in &body.blocks {
        if let TerminatorKind::Call { args, .. } = &data.terminator().kind {
            for p in args.iter().filter_map(Operand::place) {
                call_args[p.local.index()] = true;
            }
        }
    }
    for data in &mut body.blocks {
        for stmt in &mut data.statements {
            if let StatementKind::Assign(dest, Rvalue::Ref(BorrowKind::Mut { two_phase }, _)) =
                &mut stmt.kind
            {
                if *two_phase && !call_args[dest.local.index()] {
                    *two_phase = false;
                }
            }
        }
    }
}

/// The operands of `rvalue` in evaluation order, as [`Rvalue::operands`] lists them,
/// without collecting them.
fn rvalue_operands(rvalue: &Rvalue) -> impl Iterator<Item = &Operand> {
    let (one, two, many, bounds): (_, _, &[Operand], _) = match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp(_, op)
        | Rvalue::Cast(op, _)
        | Rvalue::Repeat(op, _)
        | Rvalue::PointerCoercion(op, _) => (Some(op), None, &[], None),
        Rvalue::BinaryOp(_, l, r) => (Some(l), Some(r), &[], None),
        Rvalue::Aggregate(_, ops) => (None, None, ops, None),
        Rvalue::Subslice(base, bounds) => (Some(base), None, &[], Some(&**bounds)),
        Rvalue::Ref(..) | Rvalue::Discriminant(_) | Rvalue::Len(_) => (None, None, &[], None),
    };
    one.into_iter()
        .chain(two)
        .chain(many)
        .chain(bounds.into_iter().flat_map(SliceBounds::operands))
}

/// The places among `ops` that go through an index, whose bounds checks a mention keeps.
fn mentions<'a>(ops: impl IntoIterator<Item = &'a Operand>) -> Vec<Place> {
    ops.into_iter()
        .filter_map(Operand::place)
        .filter(|p| p.has_index())
        .cloned()
        .collect()
}

/// Whether `ty` is `!` or contains it (an element, a referent, a generic argument).
fn mentions_never(ty: &Ty) -> bool {
    match ty {
        Ty::Never => true,
        Ty::Ref(_, inner) | Ty::Array(inner, _) | Ty::Slice(inner) => mentions_never(inner),
        Ty::Tuple(elems) => elems.iter().any(mentions_never),
        Ty::Adt(_, args) => args.iter().any(mentions_never),
        _ => false,
    }
}

/// Turns every `FalseEdge` into a `Goto` to its real target and simplifies, which drops
/// the blocks only an imaginary edge reached. Runs after borrow checking.
pub fn remove_false_edges(body: &mut Body) {
    let mut changed = false;
    for data in &mut body.blocks {
        if let Some(terminator) = &mut data.terminator {
            if let TerminatorKind::FalseEdge { real_target, .. } = terminator.kind {
                terminator.kind = TerminatorKind::Goto {
                    target: real_target,
                };
                changed = true;
            }
        }
    }
    if changed {
        simplify_body(body);
    }
}

/// A block, other than the start block, that holds nothing but `goto -> target`.
fn forwarding_target(body: &Body, bb: BasicBlock) -> Option<BasicBlock> {
    if bb == BasicBlock::START {
        return None;
    }
    let data = body.block(bb);
    if !data.statements.is_empty() {
        return None;
    }
    match data.terminator.as_ref().map(|t| &t.kind) {
        Some(TerminatorKind::Goto { target }) if *target != bb => Some(*target),
        _ => None,
    }
}

/// Follows a chain of forwarding blocks to its end, stopping at a cycle.
fn resolve_target(body: &Body, mut bb: BasicBlock) -> BasicBlock {
    let mut seen = HashSet::default();
    while let Some(next) = forwarding_target(body, bb) {
        if !seen.insert(bb) {
            break;
        }
        bb = next;
    }
    bb
}

/// Retargets every jump past blocks that only `goto` somewhere else; those blocks become
/// unreachable.
fn merge_gotos(body: &mut Body) {
    let targets: Vec<Vec<BasicBlock>> = body
        .blocks
        .iter()
        .map(|data| {
            data.terminator.as_ref().map_or_else(Vec::new, |t| {
                t.kind
                    .successors()
                    .into_iter()
                    .map(|s| resolve_target(body, s))
                    .collect()
            })
        })
        .collect();
    for (data, resolved) in body.blocks.iter_mut().zip(targets) {
        if let Some(term) = &mut data.terminator {
            for (succ, new) in term.kind.successors_mut().into_iter().zip(resolved) {
                *succ = new;
            }
        }
    }
}

/// Drops blocks unreachable from `bb0` and renumbers the survivors in their current order.
fn remove_unreachable(body: &mut Body) {
    let mut reachable = vec![false; body.blocks.len()];
    let mut stack = vec![BasicBlock::START];
    while let Some(bb) = stack.pop() {
        if reachable[bb.index()] {
            continue;
        }
        reachable[bb.index()] = true;
        if let Some(term) = &body.block(bb).terminator {
            stack.extend(term.kind.successors());
        }
    }
    if reachable.iter().all(|&r| r) {
        return;
    }
    let mut renumber = vec![None; body.blocks.len()];
    let mut next = 0u32;
    for (i, &live) in reachable.iter().enumerate() {
        if live {
            renumber[i] = Some(BasicBlock(next));
            next += 1;
        }
    }
    let blocks = std::mem::take(&mut body.blocks);
    body.blocks = blocks
        .into_iter()
        .zip(&reachable)
        .filter(|(_, &live)| live)
        .map(|(mut data, _)| {
            if let Some(term) = &mut data.terminator {
                for succ in term.kind.successors_mut() {
                    *succ = renumber[succ.index()]
                        .expect("successor of a reachable block is reachable");
                }
            }
            data
        })
        .collect();
}

/// Drops the cleanup scopes that no block is in, directly or through a nested scope (their
/// blocks became unreachable or were skipped by [`merge_gotos`]), and renumbers the rest
/// in their current order, so a parent still comes before its children.
fn remove_unused_cleanups(body: &mut Body) {
    let mut used = vec![false; body.cleanups.len()];
    for data in &body.blocks {
        let mut scope = data.cleanup;
        while let Some(s) = scope {
            if used[s.index()] {
                // Its ancestors are marked already.
                break;
            }
            used[s.index()] = true;
            scope = body.cleanups[s.index()].parent;
        }
    }
    if used.iter().all(|&u| u) {
        return;
    }
    let mut renumber = vec![None; used.len()];
    let mut next = 0u32;
    for (i, &u) in used.iter().enumerate() {
        if u {
            renumber[i] = Some(CleanupScope(next));
            next += 1;
        }
    }
    let renumbered = |s: CleanupScope| renumber[s.index()].expect("the scope of a block is in use");
    let cleanups = std::mem::take(&mut body.cleanups);
    body.cleanups = cleanups
        .into_iter()
        .zip(&used)
        .filter(|(_, &u)| u)
        .map(|(mut data, _)| {
            data.parent = data.parent.map(renumbered);
            data
        })
        .collect();
    for data in &mut body.blocks {
        data.cleanup = data.cleanup.map(renumbered);
    }
}
