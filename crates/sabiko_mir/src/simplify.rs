//! Control-flow clean-up after lowering.
//!
//! Lowering leaves two kinds of clutter behind: blocks nothing jumps to (the code after a
//! `return` or `break`, the exit of a `loop` nobody leaves) and blocks that only forward to
//! another block (join points with nothing in them). [`simplify_body`] retargets jumps
//! past the forwarding blocks, then drops every block unreachable from `bb0` and renumbers
//! the rest, keeping their relative order, and does the same for the cleanup scopes no
//! block is in any more.

use std::collections::HashSet;

use crate::{BasicBlock, Body, CleanupScope, StatementKind, TerminatorKind};

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
    let mut seen = HashSet::new();
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
