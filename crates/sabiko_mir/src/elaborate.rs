//! Drop elaboration (decision 55): a MIR pass after borrow checking.
//!
//! MIR building puts a [`TerminatorKind::Drop`] wherever a value with drop glue goes out of
//! scope, whether or not it still holds a value, and a cleanup scope ([`CleanupData`])
//! around the blocks where a panic must drop it, which also does not know what holds a
//! value. This pass makes both exact, from two forward dataflow analyses over *drop paths*:
//!
//! - A local whose type has drop glue is the root of a tree of drop paths: a struct with
//!   its own `impl Drop` is a leaf (its fields cannot be moved out, E0418, so it holds a
//!   value as a whole or not at all); a struct without one, or a tuple, has a child per
//!   field (element) with drop glue, in declaration order. The analyses track the leaves:
//!   "maybe initialised" and "maybe uninitialised", union at joins. A write initialises
//!   every leaf under the written place, a move, a drop and both storage markers
//!   uninitialise them; arguments start initialised.
//! - A drop of a path whose leaves are all initialised drops it whole (one call of its drop
//!   glue); all uninitialised, it is removed; a leaf that is maybe initialised is dropped
//!   behind its *drop flag*; any other path *opens*: each child in declaration order, by
//!   the same rules. That leaves a moved field out and drops the rest (`let b = p.B;`).
//! - An assignment to a place that may hold a value drops the old value after the store:
//!   `old = move p; p = new; drop(old)`. If that drop panics, `p` already holds the new
//!   value and unwinding drops it in `p`'s place, as rustc does. Nothing is dropped when the
//!   place holds nothing (a first or deferred initialisation, or after a move).
//! - A cleanup scope drops its local under the union of the states at the points inside it
//!   that may panic (calls, drops, checked arithmetic, and the drop of an assignment's old
//!   value): unconditionally where it always holds a value there, behind its flag where
//!   only sometimes, not at all where never (then the scope needs no handler).
//! - An open drop runs its parts one after the other; while one part is dropped, a cleanup
//!   scope of its own drops the remaining parts, so a panic in one still drops the others.
//! - A flag is a `bool` local per leaf that needs one, kept equal to the leaf's state
//!   wherever the leaf is in scope: set when the leaf is written (after a call returns,
//!   on an edge block of its own), cleared before it is moved and at its storage markers,
//!   set on entry for an argument.
//!
//! The pass runs after borrow checking, which sees the drops as built, and before copy
//! forwarding and code generation. Run [`crate::simplify`] after it: removed drops leave
//! blocks that only jump.

use std::collections::{BTreeMap, HashMap};

use sabiko_resolve::Builtin;
use sabiko_span::Span;
use sabiko_types::{Mutability, TypeckResults};

use crate::{
    BasicBlock, BasicBlockData, BinOp, Body, CleanupData, CleanupDrop, CleanupScope, ConstValue,
    Constant, Local, LocalDecl, LocalKind, Operand, Place, PlaceElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, Ty, UnOp,
};

/// Elaborates the drops of every body.
pub fn elaborate_all(bodies: &mut [Body], typeck: &TypeckResults) {
    for body in bodies {
        elaborate_body(body, typeck);
    }
}

/// Elaborates the drops of one body (see the module docs).
pub fn elaborate_body(body: &mut Body, typeck: &TypeckResults) {
    let paths = DropPaths::new(body, typeck);
    if paths.nodes.is_empty() {
        return;
    }
    let states = Analysis::run(body, &paths);
    let plan = Plan::new(body, typeck, &paths, &states);
    plan.apply(body, &paths);
}

// ------------------------------------------------------------------------------------------
// Drop paths
// ------------------------------------------------------------------------------------------

/// One node of the drop-path forest.
#[derive(Debug)]
struct PathNode {
    place: Place,
    /// The children, by field (element) index, in declaration order.
    children: Vec<(usize, usize)>,
    /// For a leaf (a struct with its own `drop`), its index among the leaves.
    leaf: Option<usize>,
}

#[derive(Debug, Default)]
struct DropPaths {
    nodes: Vec<PathNode>,
    /// The root path of each local with drop glue, ordered so iterating it is
    /// deterministic (the flag writes for arguments are emitted in this order).
    roots: BTreeMap<Local, usize>,
    /// The node of each leaf.
    leaf_nodes: Vec<usize>,
}

/// What a place is to the drop paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    /// The place is this path.
    Node(usize),
    /// The place is part of the value of this leaf, a struct with its own `drop`.
    InLeaf(usize),
    /// The place has no drop path: it is behind a reference, has no drop glue, or is (in)
    /// the return place.
    Untracked,
}

impl DropPaths {
    fn new(body: &Body, typeck: &TypeckResults) -> Self {
        let mut paths = Self::default();
        for (index, decl) in body.locals.iter().enumerate() {
            if decl.kind == LocalKind::Return || !typeck.items.needs_drop(&decl.ty) {
                continue;
            }
            let local = Local(u32::try_from(index).expect("fewer than 2^32 locals"));
            let root = paths.add(&Place::local(local), &decl.ty, typeck);
            paths.roots.insert(local, root);
        }
        paths
    }

    fn add(&mut self, place: &Place, ty: &Ty, typeck: &TypeckResults) -> usize {
        let index = self.nodes.len();
        self.nodes.push(PathNode {
            place: place.clone(),
            children: Vec::new(),
            leaf: None,
        });
        let fields: Vec<Ty> = match ty {
            Ty::Adt(def) if typeck.items.drop_impl(*def).is_some() => {
                self.nodes[index].leaf = Some(self.leaf_nodes.len());
                self.leaf_nodes.push(index);
                return index;
            }
            Ty::Adt(def) => typeck
                .items
                .adt(*def)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect(),
            Ty::Tuple(elems) => elems.to_vec(),
            other => unreachable!("a type with drop glue is a struct or a tuple, not {other:?}"),
        };
        for (field, field_ty) in fields.iter().enumerate() {
            if typeck.items.needs_drop(field_ty) {
                let child = self.add(
                    &place.clone().field(field, field_ty.clone()),
                    field_ty,
                    typeck,
                );
                self.nodes[index].children.push((field, child));
            }
        }
        index
    }

    fn lookup(&self, place: &Place) -> Lookup {
        let Some(&mut_root) = self.roots.get(&place.local) else {
            return Lookup::Untracked;
        };
        let mut node = mut_root;
        for elem in &place.projection {
            let PlaceElem::Field(field, _) = elem else {
                return Lookup::Untracked;
            };
            if self.nodes[node].leaf.is_some() {
                return Lookup::InLeaf(node);
            }
            match self.nodes[node].children.iter().find(|(f, _)| f == field) {
                Some(&(_, child)) => node = child,
                None => return Lookup::Untracked,
            }
        }
        Lookup::Node(node)
    }

    /// The leaves under `node`, in declaration order.
    fn leaves(&self, node: usize) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(node, &mut out);
        out
    }

    fn collect_leaves(&self, node: usize, out: &mut Vec<usize>) {
        match self.nodes[node].leaf {
            Some(leaf) => out.push(leaf),
            None => {
                for &(_, child) in &self.nodes[node].children {
                    self.collect_leaves(child, out);
                }
            }
        }
    }

    /// The leaves a write to, or a move out of, `place` changes.
    fn leaves_of_place(&self, place: &Place) -> Vec<usize> {
        match self.lookup(place) {
            Lookup::Node(node) => self.leaves(node),
            Lookup::InLeaf(_) | Lookup::Untracked => Vec::new(),
        }
    }

    fn leaves_of_local(&self, local: Local) -> Vec<usize> {
        self.roots
            .get(&local)
            .map_or_else(Vec::new, |&root| self.leaves(root))
    }
}

// ------------------------------------------------------------------------------------------
// Dataflow
// ------------------------------------------------------------------------------------------

/// "Maybe initialised" and "maybe uninitialised", one bit per leaf each.
#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    init: Vec<bool>,
    uninit: Vec<bool>,
}

/// What a leaf holds at a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeafState {
    Init,
    Uninit,
    Maybe,
}

impl State {
    fn bottom(leaves: usize) -> Self {
        Self {
            init: vec![false; leaves],
            uninit: vec![false; leaves],
        }
    }

    fn set(&mut self, leaves: &[usize], init: bool) {
        for &leaf in leaves {
            self.init[leaf] = init;
            self.uninit[leaf] = !init;
        }
    }

    /// Merges `other` in; returns whether anything changed.
    fn join(&mut self, other: &State) -> bool {
        let mut changed = false;
        for (a, &b) in self
            .init
            .iter_mut()
            .chain(self.uninit.iter_mut())
            .zip(other.init.iter().chain(&other.uninit))
        {
            if b && !*a {
                *a = true;
                changed = true;
            }
        }
        changed
    }

    /// A leaf neither analysis reaches (dead code) counts as holding nothing.
    fn leaf(&self, leaf: usize) -> LeafState {
        match (self.init[leaf], self.uninit[leaf]) {
            (true, false) => LeafState::Init,
            (true, true) => LeafState::Maybe,
            (false, _) => LeafState::Uninit,
        }
    }
}

/// The places an rvalue moves out of.
fn rvalue_moves(rvalue: &Rvalue) -> Vec<&Place> {
    let ops: Vec<&Operand> = match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => vec![op],
        Rvalue::BinaryOp(_, l, r) => vec![l, r],
        Rvalue::Aggregate(_, ops) => ops.iter().collect(),
        Rvalue::Ref(..) => Vec::new(),
    };
    ops.into_iter()
        .filter_map(|op| match op {
            Operand::Move(place) => Some(place),
            _ => None,
        })
        .collect()
}

fn apply_statement(paths: &DropPaths, state: &mut State, stmt: &Statement) {
    match &stmt.kind {
        StatementKind::Assign(place, rvalue) => {
            for moved in rvalue_moves(rvalue) {
                state.set(&paths.leaves_of_place(moved), false);
            }
            state.set(&paths.leaves_of_place(place), true);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            state.set(&paths.leaves_of_local(*local), false);
        }
        StatementKind::PlaceMention(_) | StatementKind::Nop => {}
    }
}

/// The effect of a terminator before control leaves the block: a call's arguments are
/// moved into the callee, a drop takes its place's value.
fn apply_terminator_start(paths: &DropPaths, state: &mut State, term: &Terminator) {
    match &term.kind {
        TerminatorKind::Call { args, .. } => {
            for arg in args {
                if let Operand::Move(place) = arg {
                    state.set(&paths.leaves_of_place(place), false);
                }
            }
        }
        TerminatorKind::Drop { place, .. } => state.set(&paths.leaves_of_place(place), false),
        _ => {}
    }
}

/// The effect of a terminator as its successors see it.
fn apply_terminator(paths: &DropPaths, state: &mut State, term: &Terminator) {
    apply_terminator_start(paths, state, term);
    if let TerminatorKind::Call { destination, .. } = &term.kind {
        state.set(&paths.leaves_of_place(destination), true);
    }
}

/// The state on entry to every block.
struct Analysis {
    entry: Vec<State>,
}

impl Analysis {
    fn run(body: &Body, paths: &DropPaths) -> Self {
        let leaves = paths.leaf_nodes.len();
        let mut entry = vec![State::bottom(leaves); body.blocks.len()];
        let mut start = State::bottom(leaves);
        for (&local, &root) in &paths.roots {
            let is_arg = body.local_decl(local).kind == LocalKind::Arg;
            start.set(&paths.leaves(root), is_arg);
        }
        entry[0] = start;
        let mut visited = vec![false; body.blocks.len()];
        let mut queue = std::collections::VecDeque::from([BasicBlock::START]);
        while let Some(bb) = queue.pop_front() {
            visited[bb.index()] = true;
            let mut state = entry[bb.index()].clone();
            let data = body.block(bb);
            for stmt in &data.statements {
                apply_statement(paths, &mut state, stmt);
            }
            apply_terminator(paths, &mut state, data.terminator());
            for succ in data.terminator().kind.successors() {
                let changed = entry[succ.index()].join(&state);
                if (changed || !visited[succ.index()]) && !queue.contains(&succ) {
                    queue.push_back(succ);
                }
            }
        }
        Self { entry }
    }
}

// ------------------------------------------------------------------------------------------
// Decisions
// ------------------------------------------------------------------------------------------

/// One drop of an elaborated drop: `place`, behind the flag of a leaf if any.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Action {
    place: Place,
    flag: Option<usize>,
}

/// How to drop the path `node` given the state of each leaf (see the module docs).
fn drop_actions(paths: &DropPaths, node: usize, leaf: &dyn Fn(usize) -> LeafState) -> Vec<Action> {
    let mut out = Vec::new();
    collect_actions(paths, node, leaf, &mut out);
    out
}

fn collect_actions(
    paths: &DropPaths,
    node: usize,
    leaf: &dyn Fn(usize) -> LeafState,
    out: &mut Vec<Action>,
) {
    let states: Vec<LeafState> = paths.leaves(node).into_iter().map(leaf).collect();
    if states.iter().all(|&s| s == LeafState::Uninit) {
        return;
    }
    let place = paths.nodes[node].place.clone();
    if states.iter().all(|&s| s == LeafState::Init) {
        out.push(Action { place, flag: None });
        return;
    }
    if let Some(leaf) = paths.nodes[node].leaf {
        out.push(Action {
            place,
            flag: Some(leaf),
        });
        return;
    }
    for &(_, child) in &paths.nodes[node].children {
        collect_actions(paths, child, leaf, out);
    }
}

/// Whether a statement may panic: checked integer arithmetic, division, remainder and
/// shifts (in any build mode, so the MIR is the same in both).
fn statement_may_panic(body: &Body, stmt: &Statement) -> bool {
    let StatementKind::Assign(_, rvalue) = &stmt.kind else {
        return false;
    };
    match rvalue {
        Rvalue::BinaryOp(
            BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Rem
            | BinOp::Shl
            | BinOp::Shr,
            lhs,
            _,
        ) => lhs.ty(body).is_integer(),
        Rvalue::UnaryOp(UnOp::Neg, op) => op.ty(body).is_integer(),
        _ => false,
    }
}

/// Whether a terminator may panic: a call (other than printing) or a drop.
fn terminator_may_panic(typeck: &TypeckResults, term: &Terminator) -> bool {
    match &term.kind {
        TerminatorKind::Call { func, .. } => !matches!(
            typeck.items.builtin(*func),
            Some(Builtin::Print | Builtin::PrintLine)
        ),
        TerminatorKind::Drop { .. } => true,
        _ => false,
    }
}

/// Everything the rewrite needs, decided from the analysis before anything changes.
struct Plan {
    /// For each unelaborated `Drop` terminator, by block: what it drops.
    drops: HashMap<BasicBlock, Vec<Action>>,
    /// For each assignment (statement location) or call (terminator) to a place that may
    /// hold a value: how to drop the old value, as places of the assigned place.
    old_values: HashMap<(BasicBlock, usize), Vec<Action>>,
    /// Each cleanup scope's drops.
    handlers: Vec<Vec<Action>>,
    /// Which leaves need a drop flag.
    flagged: Vec<bool>,
}

impl Plan {
    fn new(body: &Body, typeck: &TypeckResults, paths: &DropPaths, analysis: &Analysis) -> Self {
        let leaves = paths.leaf_nodes.len();
        // The union of the states at the points that may panic in each cleanup scope.
        let mut panic_states = vec![State::bottom(leaves); body.cleanups.len()];
        let mut drops = HashMap::new();
        let mut old_values = HashMap::new();
        let panic_at = |panic_states: &mut Vec<State>, chain: &[CleanupScope], state: &State| {
            for scope in chain {
                panic_states[scope.index()].join(state);
            }
        };
        for bb in body.block_ids() {
            let data = body.block(bb);
            let chain = body.cleanup_chain(bb);
            let mut state = analysis.entry[bb.index()].clone();
            for (i, stmt) in data.statements.iter().enumerate() {
                if statement_may_panic(body, stmt) {
                    panic_at(&mut panic_states, &chain, &state);
                }
                let before = state.clone();
                apply_statement(paths, &mut state, stmt);
                if let StatementKind::Assign(place, _) = &stmt.kind {
                    let actions = old_value_actions(body, typeck, paths, place, &before);
                    if !actions.is_empty() {
                        // The old value is dropped after the store.
                        panic_at(&mut panic_states, &chain, &state);
                        old_values.insert((bb, i), actions);
                    }
                }
            }
            let term = data.terminator();
            if let TerminatorKind::Call { destination, .. } = &term.kind {
                let actions = old_value_actions(body, typeck, paths, destination, &state);
                if !actions.is_empty() {
                    old_values.insert((bb, data.statements.len()), actions);
                }
            }
            let before_term = state.clone();
            apply_terminator_start(paths, &mut state, term);
            if let TerminatorKind::Drop { place, .. } = &term.kind {
                let actions = match paths.lookup(place) {
                    Lookup::Node(node) => drop_actions(paths, node, &|l| before_term.leaf(l)),
                    Lookup::InLeaf(_) | Lookup::Untracked => Vec::new(),
                };
                if !actions.is_empty() {
                    panic_at(&mut panic_states, &chain, &state);
                }
                drops.insert(bb, actions);
            } else if terminator_may_panic(typeck, term) {
                panic_at(&mut panic_states, &chain, &state);
            }
        }
        let handlers: Vec<Vec<Action>> = body
            .cleanups
            .iter()
            .zip(&panic_states)
            .map(|(cleanup, state)| match paths.roots.get(&cleanup.local) {
                Some(&root) => drop_actions(paths, root, &|l| state.leaf(l)),
                None => Vec::new(),
            })
            .collect();
        let mut flagged = vec![false; leaves];
        for action in drops
            .values()
            .chain(old_values.values())
            .chain(&handlers)
            .flatten()
        {
            if let Some(leaf) = action.flag {
                flagged[leaf] = true;
            }
        }
        Self {
            drops,
            old_values,
            handlers,
            flagged,
        }
    }
}

/// How to drop the value `place` holds before an assignment overwrites it.
fn old_value_actions(
    body: &Body,
    typeck: &TypeckResults,
    paths: &DropPaths,
    place: &Place,
    state: &State,
) -> Vec<Action> {
    if place.local == Local::RETURN || !typeck.items.needs_drop(&place.ty(body)) {
        return Vec::new();
    }
    match paths.lookup(place) {
        Lookup::Node(node) => drop_actions(paths, node, &|l| state.leaf(l)),
        // A field of a value with its own `drop` holds a value whenever that value does.
        Lookup::InLeaf(node) => {
            let leaf = paths.nodes[node].leaf.expect("a leaf");
            match state.leaf(leaf) {
                LeafState::Init => vec![Action {
                    place: place.clone(),
                    flag: None,
                }],
                LeafState::Maybe => vec![Action {
                    place: place.clone(),
                    flag: Some(leaf),
                }],
                LeafState::Uninit => Vec::new(),
            }
        }
        // Behind a reference: always a value.
        Lookup::Untracked => vec![Action {
            place: place.clone(),
            flag: None,
        }],
    }
}

// ------------------------------------------------------------------------------------------
// Rewrite
// ------------------------------------------------------------------------------------------

struct Rewriter<'a> {
    body: &'a mut Body,
    paths: &'a DropPaths,
    /// The flag local of each flagged leaf.
    flags: Vec<Option<Local>>,
}

impl Plan {
    fn apply(self, body: &mut Body, paths: &DropPaths) {
        let mut rw = Rewriter {
            body,
            paths,
            flags: vec![None; paths.leaf_nodes.len()],
        };
        for (leaf, &needed) in self.flagged.iter().enumerate() {
            if needed {
                let place = &paths.nodes[paths.leaf_nodes[leaf]].place;
                let span = rw.body.local_decl(place.local).span;
                rw.flags[leaf] = Some(rw.new_temp(Ty::BOOL, span));
            }
        }
        for (i, actions) in self.handlers.iter().enumerate() {
            let flags = rw.flags.clone();
            let drops = Rewriter::cleanup_drops(actions, &|leaf| flags[leaf]);
            rw.body.cleanups[i].drops = drops;
        }
        let original = rw.body.blocks.len();
        for b in 0..original {
            let bb = BasicBlock(u32::try_from(b).expect("fewer than 2^32 blocks"));
            rw.rewrite_block(bb, &self);
        }
    }
}

impl Rewriter<'_> {
    fn new_temp(&mut self, ty: Ty, span: sabiko_span::Span) -> Local {
        let local = Local(u32::try_from(self.body.locals.len()).expect("fewer than 2^32 locals"));
        self.body.locals.push(LocalDecl {
            ty,
            mutability: Mutability::Mut,
            kind: LocalKind::Temp,
            name: None,
            span,
            source: None,
        });
        local
    }

    fn new_block(&mut self, cleanup: Option<CleanupScope>) -> BasicBlock {
        let bb = BasicBlock(u32::try_from(self.body.blocks.len()).expect("fewer than 2^32 blocks"));
        self.body.blocks.push(BasicBlockData {
            statements: Vec::new(),
            terminator: None,
            cleanup,
        });
        bb
    }

    fn cleanup_drops(
        actions: &[Action],
        flag_of: &dyn Fn(usize) -> Option<Local>,
    ) -> Vec<CleanupDrop> {
        actions
            .iter()
            .map(|a| CleanupDrop {
                place: a.place.clone(),
                flag: a.flag.and_then(flag_of),
            })
            .collect()
    }

    /// `flag = value` for the flagged leaves among `leaves`.
    fn flag_writes(
        &self,
        leaves: &[usize],
        value: bool,
        span: sabiko_span::Span,
    ) -> Vec<Statement> {
        leaves
            .iter()
            .filter_map(|&leaf| self.flags[leaf])
            .map(|flag| Statement {
                kind: StatementKind::Assign(
                    Place::local(flag),
                    Rvalue::Use(Operand::Constant(Constant {
                        ty: Ty::BOOL,
                        value: ConstValue::Bool(value),
                        span,
                    })),
                ),
                span,
            })
            .collect()
    }

    fn rewrite_block(&mut self, bb: BasicBlock, plan: &Plan) {
        let data = std::mem::take(&mut self.body.blocks[bb.index()]);
        let cleanup = data.cleanup;
        let statement_count = data.statements.len();
        let mut cur = bb;
        let mut stmts: Vec<Statement> = Vec::new();
        if bb == BasicBlock::START {
            // Arguments hold their values on entry.
            for (&local, &root) in &self.paths.roots {
                if self.body.local_decl(local).kind == LocalKind::Arg {
                    let span = self.body.local_decl(local).span;
                    stmts.extend(self.flag_writes(&self.paths.leaves(root), true, span));
                }
            }
        }
        for (i, stmt) in data.statements.into_iter().enumerate() {
            match &stmt.kind {
                StatementKind::Assign(place, rvalue) => {
                    let place = place.clone();
                    for moved in rvalue_moves(rvalue) {
                        stmts.extend(self.flag_writes(
                            &self.paths.leaves_of_place(moved),
                            false,
                            stmt.span,
                        ));
                    }
                    let written = self.paths.leaves_of_place(&place);
                    let span = stmt.span;
                    if let Some(actions) = plan.old_values.get(&(bb, i)) {
                        let (old, flags) = self.save_old_value(&place, actions, &mut stmts, span);
                        stmts.push(stmt);
                        stmts.extend(self.flag_writes(&written, true, span));
                        let cont = self.new_block(cleanup);
                        let entry = self.drop_chain(&old, &flags, cleanup, cont, span);
                        self.finish(
                            cur,
                            stmts,
                            TerminatorKind::Goto { target: entry },
                            span,
                            cleanup,
                        );
                        cur = cont;
                        stmts = Vec::new();
                    } else {
                        stmts.push(stmt);
                        stmts.extend(self.flag_writes(&written, true, span));
                    }
                }
                StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                    let leaves = self.paths.leaves_of_local(*local);
                    let span = stmt.span;
                    stmts.push(stmt);
                    stmts.extend(self.flag_writes(&leaves, false, span));
                }
                StatementKind::PlaceMention(_) | StatementKind::Nop => stmts.push(stmt),
            }
        }
        let term = data.terminator.expect("a built block has a terminator");
        let call_old = plan.old_values.get(&(bb, statement_count));
        let drops = plan.drops.get(&bb);
        let kind =
            self.rewrite_terminator(term.kind, &mut stmts, call_old, drops, cleanup, term.span);
        self.finish(cur, stmts, kind, term.span, cleanup);
    }

    /// The terminator of a rewritten block, with the statements before it: flags cleared
    /// for the arguments a call moves and set for its destination (on an edge block of its
    /// own), and a `Drop` replaced by its elaborated drops.
    fn rewrite_terminator(
        &mut self,
        kind: TerminatorKind,
        stmts: &mut Vec<Statement>,
        call_old: Option<&Vec<Action>>,
        drops: Option<&Vec<Action>>,
        cleanup: Option<CleanupScope>,
        span: Span,
    ) -> TerminatorKind {
        match kind {
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
            } => {
                for arg in &args {
                    if let Operand::Move(place) = arg {
                        stmts.extend(self.flag_writes(
                            &self.paths.leaves_of_place(place),
                            false,
                            span,
                        ));
                    }
                }
                let mut after =
                    self.flag_writes(&self.paths.leaves_of_place(&destination), true, span);
                let mut saved = None;
                if let Some(actions) = call_old {
                    // Not produced by MIR building, which evaluates the right-hand side of
                    // an assignment into a temporary first; kept correct regardless.
                    debug_assert!(
                        false,
                        "a call writes into a place holding a value with drop glue"
                    );
                    saved = Some(self.save_old_value(&destination, actions, stmts, span));
                }
                let target = match target {
                    Some(target) if !after.is_empty() || saved.is_some() => {
                        let edge = self.new_block(cleanup);
                        let exit = match saved {
                            Some((old, flags)) => {
                                self.drop_chain(&old, &flags, cleanup, target, span)
                            }
                            None => target,
                        };
                        let after = std::mem::take(&mut after);
                        self.finish(
                            edge,
                            after,
                            TerminatorKind::Goto { target: exit },
                            span,
                            cleanup,
                        );
                        Some(edge)
                    }
                    other => other,
                };
                TerminatorKind::Call {
                    func,
                    args,
                    destination,
                    target,
                }
            }
            TerminatorKind::Drop { target, .. } => {
                let actions = drops.cloned().unwrap_or_default();
                let flags: Vec<Option<Local>> = actions
                    .iter()
                    .map(|a| a.flag.and_then(|l| self.flags[l]))
                    .collect();
                let entry = self.drop_chain(&actions, &flags, cleanup, target, span);
                TerminatorKind::Goto { target: entry }
            }
            other => other,
        }
    }

    /// Before an assignment to `place`: moves its old value into a fresh temporary and
    /// copies the flags the old value's drop reads (the assignment sets them). Returns the
    /// drops of the old value, as places of the temporary, and their flags.
    fn save_old_value(
        &mut self,
        place: &Place,
        actions: &[Action],
        stmts: &mut Vec<Statement>,
        span: sabiko_span::Span,
    ) -> (Vec<Action>, Vec<Option<Local>>) {
        let ty = place.ty(self.body);
        let old = self.new_temp(ty, span);
        let mut flags = Vec::new();
        let mut rebased = Vec::new();
        for action in actions {
            let flag = match action.flag.and_then(|leaf| self.flags[leaf]) {
                Some(flag) => {
                    let copy = self.new_temp(Ty::BOOL, span);
                    stmts.push(Statement {
                        kind: StatementKind::Assign(
                            Place::local(copy),
                            Rvalue::Use(Operand::Copy(Place::local(flag))),
                        ),
                        span,
                    });
                    Some(copy)
                }
                None => None,
            };
            flags.push(flag);
            let mut projection = vec![];
            projection.extend_from_slice(&action.place.projection[place.projection.len()..]);
            rebased.push(Action {
                place: Place {
                    local: old,
                    projection,
                },
                flag: None,
            });
        }
        stmts.push(Statement {
            kind: StatementKind::Assign(
                Place::local(old),
                Rvalue::Use(Operand::Move(place.clone())),
            ),
            span,
        });
        (rebased, flags)
    }

    /// Blocks that drop `actions` one after the other, each behind its flag if it has one,
    /// then go to `exit`; returns the first. While one is dropped, a cleanup scope of its
    /// own drops the ones after it.
    fn drop_chain(
        &mut self,
        actions: &[Action],
        flags: &[Option<Local>],
        cleanup: Option<CleanupScope>,
        exit: BasicBlock,
        span: sabiko_span::Span,
    ) -> BasicBlock {
        let mut next = exit;
        for k in (0..actions.len()).rev() {
            let rest: Vec<CleanupDrop> = actions[k + 1..]
                .iter()
                .zip(&flags[k + 1..])
                .map(|(a, &flag)| CleanupDrop {
                    place: a.place.clone(),
                    flag,
                })
                .collect();
            let drop_cleanup = if rest.is_empty() {
                cleanup
            } else {
                let scope = CleanupScope(
                    u32::try_from(self.body.cleanups.len())
                        .expect("fewer than 2^32 cleanup scopes"),
                );
                self.body.cleanups.push(CleanupData {
                    parent: cleanup,
                    local: actions[k].place.local,
                    drops: rest,
                });
                Some(scope)
            };
            let drop_block = self.new_block(drop_cleanup);
            self.finish(
                drop_block,
                Vec::new(),
                TerminatorKind::Drop {
                    place: actions[k].place.clone(),
                    target: next,
                },
                span,
                drop_cleanup,
            );
            next = match flags[k] {
                Some(flag) => {
                    let switch = self.new_block(cleanup);
                    self.finish(
                        switch,
                        Vec::new(),
                        TerminatorKind::SwitchInt {
                            discr: Operand::Copy(Place::local(flag)),
                            targets: vec![(0, next)],
                            otherwise: drop_block,
                        },
                        span,
                        cleanup,
                    );
                    switch
                }
                None => drop_block,
            };
        }
        next
    }

    fn finish(
        &mut self,
        bb: BasicBlock,
        statements: Vec<Statement>,
        kind: TerminatorKind,
        span: sabiko_span::Span,
        cleanup: Option<CleanupScope>,
    ) {
        self.body.blocks[bb.index()] = BasicBlockData {
            statements,
            terminator: Some(Terminator { kind, span }),
            cleanup,
        };
    }
}
