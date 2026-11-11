//! AST to MIR lowering.
//!
//! One [`Body`] is built per function whose type check succeeded. The structure follows
//! rustc's `mir_build`: an expression is lowered in one of three ways depending on what
//! the context needs from it.
//!
//! - [`Builder::as_operand`] produces an [`Operand`]: a constant, or a `copy`/`move` of a
//!   place, introducing a temporary for anything else.
//! - [`Builder::as_place`] produces a [`Place`] for place expressions (locals, field
//!   accesses with their autoderefs, `*e`); any other expression is evaluated into a
//!   temporary first.
//! - [`Builder::expr_into_dest`] writes the expression's value into a destination place.
//!   Control flow lives here: `if`, loops, `&&`/`||` and blocks all branch and finally
//!   write their value into the destination.
//!
//! Storage markers follow rustc too: a `StorageLive` when a user local or temporary comes
//! into existence, `StorageDead` at the end of its scope (the enclosing block for `let`
//! bindings, the enclosing statement for temporaries) in reverse declaration order, and
//! on every early exit (`break`, `continue`, `return`) for all scopes being left.
//! Temporaries go to rustc's temporary scope (edition 2024, decision 74): the innermost
//! statement, `if` or `while` condition, `if let` scrutinee (live through `then`, dropped
//! before `else`), operand of `&&`/`||`, body of an `if`, `else`, `while` or `loop`, or
//! tail expression of a block, whose temporaries are dropped before the block's locals.
//! The exception is an operand with drop glue: it belongs to the call, aggregate or
//! assignment that consumes it (decision 71), which for a tail expression is the tail's
//! own scope.
//!
//! Drops (decision 55): a local whose type has drop glue gets a `Drop` terminator right
//! before its `StorageDead`, on every path out of its scope, whether or not it still holds
//! a value; drop elaboration sorts that out after borrow checking. Function arguments are
//! dropped when the function's outermost scopes end, the last argument first, each after
//! the bindings of its pattern (rustc's order). While a local with drop glue may hold a
//! value, the blocks are protected by a cleanup scope ([`crate::CleanupData`]): it opens
//! after the local's first write when that write is the only one its initialiser makes
//! (`let d = Make();`), and at the declaration otherwise (`let d;`, `let d = if ..`), and
//! closes before the local's drop. The open cleanup scopes form a chain ordered like the
//! drops at the ends of the scopes, innermost first; a scope opened below others (the
//! binding of `let d = F(&Make());`, written while the statement's temporary still lives)
//! closes those and reopens them inside it, so cleanup scopes always nest and each has a
//! single entry. A block that holds anything that may panic never changes chains half-way:
//! the chain changes at a new block.
//!
//! Lowering itself never reports diagnostics: the type checker already rejected every
//! invalid program, and bodies with type errors are skipped.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{
    BinOp, Block, Expr, ExprKind, FnDecl, Item, ItemKind, Lit, NodeId, Pat, PatKind, RangeEnd,
    SourceFile, Stmt, StmtKind, UnOp,
};
use sabiko_diagnostics::DiagnosticSink;
use sabiko_resolve::{Builtin, CoreItem, DefId, DefKind, LocalId, PrimTy, Res, ResolveResults};
use sabiko_span::Span;
use sabiko_types::{BodyTypes, ConstValue, Mutability, Ty, TypeckResults};

mod matches;

use matches::{has_or, BindMode};

use crate::{
    AggregateKind, BasicBlock, BasicBlockData, Body, BorrowKind, CleanupData, CleanupDrop,
    CleanupScope, Constant, FakeBorrowKind, Local, LocalDecl, LocalKind, Operand, Place, Rvalue,
    SliceBounds, Statement, StatementKind, Terminator, TerminatorKind,
};

/// Lowers every function in `file` whose body type-checked without errors, in source
/// order. Constants have no bodies. A function that refers to a constant whose evaluation
/// failed is skipped as well: its errors were reported with the constant.
///
/// `sink` receives the failures of evaluating the trait constants a body names at a
/// type (#116); lowering itself never reports, the validator does.
#[must_use]
pub fn build_bodies(
    file: &SourceFile,
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    sink: &mut DiagnosticSink,
) -> Vec<Body> {
    let mut bodies = Vec::new();
    for item in file.all_items() {
        let ItemKind::Fn(f) = &item.kind else {
            continue;
        };
        if f.body.is_none() {
            // A required method of a trait: calls of it run an implementation's.
            continue;
        }
        let def = resolve.def_of_item(item.id);
        let types = typeck.body(def);
        if types.has_errors {
            continue;
        }
        if let Some(mut body) = Builder::new(resolve, typeck, types, def, item, f).build(f) {
            crate::mono::eval_assoc_consts(&mut body, resolve, typeck, false, sink);
            bodies.push(body);
        }
    }
    bodies
}

/// Where an `if`, `if let` or `match` writes its value, and whether that value is in an
/// extending position of a `let` initialiser ([`Builder::extending_into`]), as are then
/// its branches' or arms' values.
#[derive(Clone, Copy)]
struct ValueDest<'p> {
    place: &'p Place,
    extending: bool,
}

impl<'p> ValueDest<'p> {
    fn plain(place: &'p Place) -> Self {
        ValueDest {
            place,
            extending: false,
        }
    }

    fn extending(place: &'p Place) -> Self {
        ValueDest {
            place,
            extending: true,
        }
    }
}

/// A loop being lowered: where `continue` and `break` go, and where `break value`
/// writes.
struct LoopScope {
    header: BasicBlock,
    /// Created on the first `break`; a loop nobody breaks out of has no exit.
    exit: Option<BasicBlock>,
    /// `None` when the loop's value is discarded.
    dest: Option<Place>,
    /// `scopes.len()` when the loop was entered; `break`/`continue` leave every scope
    /// above it.
    scope_depth: usize,
}

struct Builder<'a> {
    resolve: &'a ResolveResults,
    typeck: &'a TypeckResults,
    types: &'a BodyTypes,
    body: Body,
    /// Resolver bindings to MIR locals.
    local_map: HashMap<LocalId, Local>,
    /// Locals to drop and `StorageDead` when each open scope ends, innermost last, each in
    /// declaration order. Blocks, statements and the other temporary scopes each open one;
    /// the function's arguments are in scopes of their own, outermost.
    scopes: Vec<Vec<Local>>,
    /// Index into `scopes` of the scope temporaries go to (see the module docs).
    temp_scope: usize,
    /// Index into `scopes` of the scope of the call, aggregate or assignment whose operands
    /// are being lowered ([`Self::in_operand_scope`]), while it is the innermost one.
    operand_scope: Option<usize>,
    /// The temporary scope of a block's tail expression and the expression (its parentheses
    /// stripped): rustc's scope of that expression itself, so a call, aggregate or
    /// assignment that is the tail puts its operands there (see [`Self::in_tail_scope`]).
    tail_scope: Option<(usize, NodeId)>,
    /// Locals with drop glue declared but not written yet, whose cleanup scope opens after
    /// their first write.
    pending: HashSet<Local>,
    /// The open cleanup scopes, outermost first, with the local each protects.
    chain: Vec<(Local, CleanupScope)>,
    /// While lowering the initialiser of `let x = ..;`: the block's scope and the position
    /// in it where temporaries extended to the block go, before the binding (rustc drops
    /// the binding first).
    extend_at: Option<(usize, usize)>,
    /// The temporaries, declared ahead ([`Self::declare_extended`]), that hold the
    /// operands of `&` in an extending position under a block, `if` or `match`, keyed by
    /// the operand.
    extended: HashMap<NodeId, Local>,
    loops: Vec<LoopScope>,
    /// The block holding the single `return`, created on first use.
    return_block: Option<BasicBlock>,
    /// The block statements are currently appended to.
    current: BasicBlock,
    /// Set when a constant without a value was referenced; the body is discarded.
    failed: bool,
    /// The span of the outermost parentheses around an expression, for the expressions
    /// that have them (see [`Builder::span_of`]).
    paren_spans: HashMap<NodeId, Span>,
    /// Expressions whose pointer coercion (`BodyTypes::coercions`) is being applied: inside,
    /// the expression is lowered as its own type.
    coercing: std::collections::HashSet<NodeId>,
    /// While a match guard is lowered: the place each of its arm's bindings denotes (a
    /// shared reference to its part of the scrutinee, see `matches`).
    guard_places: HashMap<LocalId, Place>,
    /// The locals of the `ref`/`ref mut` bindings of the candidate whose guard is being
    /// lowered, bound before the guard (rustc's `RefWithinGuard`): binding the candidate
    /// for its arm leaves them as they are.
    bound_for_guard: Vec<Local>,
    /// While a `match` arm's candidates are lowered: what their `let` guards hold when they
    /// hold (#98), the scrutinee temporaries and bindings of the guard's `let`s, which the
    /// arm's body scope takes over.
    guard_held: Vec<Local>,
    /// The locals of each guard `let`'s bindings, by the `let`'s node: made once per arm,
    /// so that every candidate of the arm binds the same locals.
    prepared_lets: HashMap<NodeId, Vec<Local>>,
    /// How the place being lowered by `as_place` is used: `Mut` where it is written,
    /// borrowed `&mut` or bound `ref mut`. A sub-slice inside it (`&mut a[1..][0]`)
    /// borrows its base the same way, as rustc picks `IndexMut` over `Index`. Values
    /// evaluated on the way (an index, a bound) are lowered as `Not` again.
    place_mutability: Mutability,
    /// While a candidate's bindings are made: the span of the scrutinee (or initialiser)
    /// they are bound out of, recorded as [`LocalDecl::match_span`].
    binding_scrutinee: Option<Span>,
    /// What the candidates of the match being lowered have tested (see `matches`).
    tests: matches::TestCache,
}

impl<'a> Builder<'a> {
    fn new(
        resolve: &'a ResolveResults,
        typeck: &'a TypeckResults,
        types: &'a BodyTypes,
        def: DefId,
        item: &Item,
        f: &FnDecl,
    ) -> Self {
        let sig = typeck.items.fn_sig(def);
        let mut locals = vec![LocalDecl {
            ty: sig.output.clone(),
            mutability: Mutability::Mut,
            kind: LocalKind::Return,
            name: None,
            span: f.ret.as_ref().map_or(f.name.span, |t| t.span),
            source: None,
            match_span: None,
        }];
        let mut local_map = HashMap::new();
        let mut inputs = sig.inputs.iter();
        if let Some(sp) = &f.self_param {
            let ty = inputs
                .next()
                .expect("a method's signature starts with its receiver");
            let source = resolve.local_of_pat(sp.id);
            if let Some(id) = source {
                local_map.insert(id, Local(1));
            }
            locals.push(LocalDecl {
                ty: ty.clone(),
                mutability: Mutability::from_bool(matches!(
                    sp.kind,
                    sabiko_ast::SelfKind::Value { mutable: true }
                )),
                kind: LocalKind::Arg,
                name: Some("self".to_owned()),
                span: sp.span,
                source,
                match_span: None,
            });
        }
        for (param, ty) in f.params.iter().zip(inputs) {
            let (name, mutability, source) = match &param.pat.kind {
                PatKind::Ident {
                    by_ref: false,
                    mutable,
                    name,
                    sub: None,
                } if resolve.local_of_pat(param.pat.id).is_some() => (
                    Some(name.name.clone()),
                    Mutability::from_bool(*mutable),
                    resolve.local_of_pat(param.pat.id),
                ),
                // Any other pattern destructures the argument at the start of the body. The
                // unnamed argument is mutable, as rustc declares it: `(ref mut a, _): (i32,
                // i32)` borrows it mutably.
                _ => (None, Mutability::Mut, None),
            };
            let local = Local(u32::try_from(locals.len()).expect("fewer than 2^32 locals"));
            if let Some(id) = source {
                local_map.insert(id, local);
            }
            locals.push(LocalDecl {
                ty: ty.clone(),
                mutability,
                kind: LocalKind::Arg,
                name,
                span: param.span,
                source,
                match_span: None,
            });
        }
        let body = Body {
            def,
            name: crate::mono::instance_name(resolve, &typeck.items, def, &[]),
            instance: Vec::new(),
            locals,
            arg_count: f.params.len() + usize::from(f.self_param.is_some()),
            blocks: vec![BasicBlockData::default()],
            cleanups: Vec::new(),
            span: item.span,
        };
        Self {
            resolve,
            typeck,
            types,
            body,
            local_map,
            scopes: Vec::new(),
            temp_scope: 0,
            operand_scope: None,
            tail_scope: None,
            pending: HashSet::new(),
            chain: Vec::new(),
            extend_at: None,
            extended: HashMap::new(),
            loops: Vec::new(),
            return_block: None,
            current: BasicBlock::START,
            failed: false,
            paren_spans: HashMap::new(),
            coercing: std::collections::HashSet::new(),
            guard_places: HashMap::new(),
            bound_for_guard: Vec::new(),
            guard_held: Vec::new(),
            prepared_lets: HashMap::new(),
            place_mutability: Mutability::Not,
            binding_scrutinee: None,
            tests: matches::TestCache::default(),
        }
    }

    fn build(mut self, f: &FnDecl) -> Option<Body> {
        let block = f
            .body
            .as_ref()
            .expect("only functions with a body are built");
        let span = block.span;
        // Each argument has a scope of its own, the first outermost, holding the argument
        // and the bindings of its pattern, which live for the whole body: rustc drops the
        // bindings, then the argument, the last argument first.
        let first_param = 1 + usize::from(f.self_param.is_some());
        let args: Vec<Local> = self.body.args().collect();
        for arg in args {
            self.push_scope();
            self.scopes.last_mut().expect("pushed above").push(arg);
            self.track_drop(arg);
            self.open_pending(arg);
            let Some(param) = arg.index().checked_sub(first_param).map(|i| &f.params[i]) else {
                continue;
            };
            if self.body.local_decl(arg).source.is_none()
                && !matches!(param.pat.kind, PatKind::Wild)
            {
                let saved = self.push_temp_scope();
                self.bind_pat(
                    &param.pat,
                    Place::local(arg),
                    &matches::Choice::new(),
                    BindMode::Declare,
                );
                self.close_temp_scope(saved, param_span(f));
            }
        }
        // The body's own temporary scope, around its block (whose tail has a scope of its
        // own inside it).
        self.push_temp_scope();
        self.lower_block_into(&Place::local(Local::RETURN), block, false);
        while !self.scopes.is_empty() {
            self.pop_scope(span);
        }
        let ret = self.return_block();
        self.terminate(TerminatorKind::Goto { target: ret }, span);
        self.enter(ret);
        self.terminate(TerminatorKind::Return, span);
        debug_assert!(self.scopes.is_empty() && self.loops.is_empty());
        if self.failed {
            None
        } else {
            Some(self.body)
        }
    }

    // ----- CFG plumbing -----

    fn new_block(&mut self) -> BasicBlock {
        let bb = BasicBlock(u32::try_from(self.body.blocks.len()).expect("fewer than 2^32 blocks"));
        self.body.blocks.push(BasicBlockData::default());
        bb
    }

    fn return_block(&mut self) -> BasicBlock {
        if let Some(bb) = self.return_block {
            return bb;
        }
        let bb = self.new_block();
        self.return_block = Some(bb);
        bb
    }

    fn push_stmt(&mut self, kind: StatementKind, span: Span) {
        self.body.blocks[self.current.index()]
            .statements
            .push(Statement { kind, span });
    }

    /// Continues in `bb`, protected by the cleanup scopes open now.
    fn enter(&mut self, bb: BasicBlock) {
        self.current = bb;
        self.body.blocks[bb.index()].cleanup = self.cleanup_head();
    }

    /// `place = rvalue`. The first write to a whole pending local opens its cleanup scope.
    fn assign(&mut self, place: Place, rvalue: Rvalue, span: Span) {
        let written = place.is_local().then_some(place.local);
        self.push_stmt(StatementKind::Assign(place, rvalue), span);
        if let Some(local) = written {
            self.open_pending(local);
        }
    }

    /// Ends `bb` with `kind`. Every block is terminated exactly once.
    fn terminate_block(&mut self, bb: BasicBlock, kind: TerminatorKind, span: Span) {
        let data = &mut self.body.blocks[bb.index()];
        debug_assert!(data.terminator.is_none(), "bb{} terminated twice", bb.0);
        data.terminator = Some(Terminator { kind, span });
    }

    /// Ends the current block with `kind`.
    fn terminate(&mut self, kind: TerminatorKind, span: Span) {
        let bb = self.current;
        self.terminate_block(bb, kind, span);
    }

    /// Ends the current block with a `switchInt` on `discr`. A discriminant of type `!`
    /// (`if loop {} {}`, `if return {}`) never produces a value: the block is unreachable
    /// instead, and the branches lowered after it are dead code `simplify` removes.
    fn switch_int(
        &mut self,
        discr: Operand,
        targets: Vec<(u128, BasicBlock)>,
        otherwise: BasicBlock,
        span: Span,
    ) {
        if discr.ty(&self.body).is_never() {
            self.terminate(TerminatorKind::Unreachable, span);
        } else {
            self.terminate(
                TerminatorKind::SwitchInt {
                    discr,
                    targets,
                    otherwise,
                },
                span,
            );
        }
    }

    // ----- drops and cleanup scopes (decision 55) -----

    fn needs_drop(&self, local: Local) -> bool {
        self.typeck
            .items
            .needs_drop(&self.body.local_decl(local).ty)
    }

    /// Starts tracking `local` if its type has drop glue: its cleanup scope opens after its
    /// first whole write, or earlier at [`Self::open_pending`].
    fn track_drop(&mut self, local: Local) {
        if self.needs_drop(local) {
            self.pending.insert(local);
        }
    }

    /// Opens the cleanup scope of `local` if it is still pending: after its first write,
    /// or before it when its initialiser writes it in several places (`let d = if c { A() }
    /// else { B() };`) or later (`let d;`), or for an argument.
    fn open_pending(&mut self, local: Local) {
        if self.pending.remove(&local) {
            self.open_cleanup(local);
        }
    }

    /// Where `local` is among the open scopes: the scope's index and the position in it.
    /// Unwinding drops locals in the reverse of this order.
    fn rank(&self, local: Local) -> (usize, usize) {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, scope)| scope.iter().position(|&l| l == local).map(|p| (i, p)))
            .expect("a local with a cleanup scope is in an open scope")
    }

    /// Opens a cleanup scope for `local` at its place in the chain: the scopes of locals a
    /// panic drops before it (in inner scopes, or declared later) close and reopen inside
    /// it.
    fn open_cleanup(&mut self, local: Local) {
        let rank = self.rank(local);
        let mut above = Vec::new();
        while let Some(&(top, _)) = self.chain.last() {
            if self.rank(top) <= rank {
                break;
            }
            above.push(top);
            self.chain.pop();
        }
        self.push_cleanup(local);
        for l in above.into_iter().rev() {
            self.push_cleanup(l);
        }
        self.chain_changed();
    }

    fn push_cleanup(&mut self, local: Local) {
        let scope = CleanupScope(
            u32::try_from(self.body.cleanups.len()).expect("fewer than 2^32 cleanup scopes"),
        );
        self.body.cleanups.push(CleanupData {
            parent: self.cleanup_head(),
            local,
            drops: vec![CleanupDrop {
                place: Place::local(local),
                flag: None,
                variants: Vec::new(),
            }],
        });
        self.chain.push((local, scope));
    }

    fn cleanup_head(&self) -> Option<CleanupScope> {
        self.chain.last().map(|&(_, scope)| scope)
    }

    /// The chain of open cleanup scopes changed: what follows is protected by the new
    /// chain. The current block takes it over while it holds nothing that may panic (only
    /// storage markers); otherwise a new block starts here.
    fn chain_changed(&mut self) {
        let head = self.cleanup_head();
        let data = &self.body.blocks[self.current.index()];
        if data.cleanup == head {
            return;
        }
        let only_markers = data.statements.iter().all(|s| {
            matches!(
                s.kind,
                StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::PlaceMention(_)
                    | StatementKind::Nop
            )
        });
        if only_markers {
            self.body.blocks[self.current.index()].cleanup = head;
        } else {
            let span = data.statements.last().map_or(self.body.span, |s| s.span);
            let next = self.new_block();
            self.terminate(TerminatorKind::Goto { target: next }, span);
            self.enter(next);
        }
    }

    /// The drop of `local` if its type has drop glue, outside its own cleanup scope.
    fn drop_local(&mut self, local: Local, span: Span) {
        if !self.needs_drop(local) {
            return;
        }
        self.pending.remove(&local);
        if self.chain.last().is_some_and(|&(l, _)| l == local) {
            self.chain.pop();
            self.chain_changed();
        }
        let next = self.new_block();
        self.terminate(
            TerminatorKind::Drop {
                place: Place::local(local),
                target: next,
            },
            span,
        );
        self.enter(next);
    }

    /// `temp`'s value is computed. rustc schedules a temporary's drop at this point, so it
    /// is dropped before the temporaries made while computing it (`Make(1).Next();` drops
    /// `Next`'s result, then `Make(1)`'s): `temp` moves to the end of its scope (of the
    /// extended temporaries of a `let`, for one of those), and its cleanup scope, if it
    /// opened before the value was complete, moves to its new rank.
    fn temp_complete(&mut self, temp: Local) {
        if !self.needs_drop(temp) {
            return;
        }
        let (scope, pos) = self.rank(temp);
        let end = match self.extend_at {
            Some((block_scope, at)) if scope == block_scope && pos < at => at - 1,
            _ => self.scopes[scope].len() - 1,
        };
        let moved = self.scopes[scope].remove(pos);
        self.scopes[scope].insert(end, moved);
        let Some(index) = self.chain.iter().position(|&(l, _)| l == temp) else {
            return;
        };
        if index + 1 == self.chain.len() {
            return;
        }
        let mut reopen: Vec<Local> = self.chain.drain(index..).map(|(l, _)| l).collect();
        reopen.sort_by_key(|&l| self.rank(l));
        for local in reopen {
            self.push_cleanup(local);
        }
        self.chain_changed();
    }

    /// The end of `local`'s scope: its drop, then its `StorageDead` (arguments have no
    /// storage markers).
    fn end_local(&mut self, local: Local, span: Span) {
        self.drop_local(local, span);
        if self.body.local_decl(local).kind != LocalKind::Arg {
            self.push_stmt(StatementKind::StorageDead(local), span);
        }
    }

    /// Leaves every scope at `depth` and above (an early exit), innermost first, then
    /// jumps to `target`. The scopes stay open: the code that follows is unreachable but
    /// is still lowered in the scopes it is written in, under the cleanup scopes it had.
    fn exit_to(&mut self, depth: usize, target: BasicBlock, span: Span) {
        self.leave_to(depth, target, span);
        let next = self.new_block();
        self.enter(next);
    }

    /// [`Self::exit_to`] without a block to continue in: the caller enters one.
    fn leave_to(&mut self, depth: usize, target: BasicBlock, span: Span) {
        let saved = (self.chain.clone(), self.pending.clone());
        let locals: Vec<Local> = self.scopes[depth..]
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev().copied())
            .collect();
        for local in locals {
            self.end_local(local, span);
        }
        self.terminate(TerminatorKind::Goto { target }, span);
        (self.chain, self.pending) = saved;
    }

    // ----- locals and scopes -----

    fn new_local(&mut self, decl: LocalDecl) -> Local {
        let local = Local(u32::try_from(self.body.locals.len()).expect("fewer than 2^32 locals"));
        self.body.locals.push(decl);
        local
    }

    /// Declares a temporary in the enclosing block's scope (not the statement's), so it
    /// outlives the statement that creates it. In `let x = ..;` it goes before `x`: rustc
    /// drops the binding first.
    fn new_temp_in_block_scope(&mut self, ty: Ty, span: Span) -> Local {
        let local = self.new_local(LocalDecl {
            ty,
            mutability: Mutability::Mut,
            kind: LocalKind::Temp,
            name: None,
            span,
            source: None,
            match_span: None,
        });
        if let Some((block_scope, at)) = self.extend_at {
            self.scopes[block_scope].insert(at, local);
            self.extend_at = Some((block_scope, at + 1));
        } else {
            // The statement's scope is the temporary scope (an operand scope may be open
            // inside it: `let (a, b) = (&F(), 1);`).
            let block_scope = self.temp_scope - 1;
            self.scopes[block_scope].push(local);
        }
        self.push_stmt(StatementKind::StorageLive(local), span);
        self.track_drop(local);
        local
    }

    /// Declares a temporary in the temporary scope and marks it live.
    fn new_temp(&mut self, ty: Ty, span: Span) -> Local {
        self.new_temp_in(self.temp_scope, ty, span)
    }

    /// Declares the temporary holding an operand's value and marks it live: in the scope of
    /// the call, aggregate or assignment that consumes it when its type has drop glue (see
    /// [`Self::in_operand_scope`]), in the temporary scope otherwise.
    fn new_operand_temp(&mut self, ty: Ty, span: Span) -> Local {
        let scope = match self.operand_scope {
            Some(scope) if scope + 1 == self.scopes.len() && self.typeck.items.needs_drop(&ty) => {
                scope
            }
            _ => self.temp_scope,
        };
        self.new_temp_in(scope, ty, span)
    }

    fn new_temp_in(&mut self, scope: usize, ty: Ty, span: Span) -> Local {
        let local = self.new_local(LocalDecl {
            ty,
            mutability: Mutability::Mut,
            kind: LocalKind::Temp,
            name: None,
            span,
            source: None,
            match_span: None,
        });
        self.scopes[scope].push(local);
        self.push_stmt(StatementKind::StorageLive(local), span);
        self.track_drop(local);
        local
    }

    /// Declares the local of a `let` binding in the enclosing block scope (the innermost
    /// scope is the statement's own) and marks it live.
    fn declare_user_local(&mut self, id: LocalId, span: Span) -> Local {
        let info = self.resolve.local(id);
        let local = self.new_local(LocalDecl {
            ty: self.types.local_ty(id).clone(),
            mutability: Mutability::from_bool(info.mutable),
            kind: LocalKind::User,
            name: Some(info.name.clone()),
            span: info.span,
            source: Some(id),
            match_span: None,
        });
        self.local_map.insert(id, local);
        let block_scope = self.scopes.len() - 2;
        self.scopes[block_scope].push(local);
        self.push_stmt(StatementKind::StorageLive(local), span);
        self.track_drop(local);
        local
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    /// Opens a scope temporaries go to (a statement, a condition, a body); returns the
    /// temporary scope to restore when it closes.
    fn push_temp_scope(&mut self) -> usize {
        self.push_scope();
        std::mem::replace(&mut self.temp_scope, self.scopes.len() - 1)
    }

    /// Closes the innermost scope, dropping and ending the storage of its locals in
    /// reverse order.
    fn pop_scope(&mut self, span: Span) {
        let locals = self.scopes.pop().expect("a scope is open");
        for &local in locals.iter().rev() {
            self.end_local(local, span);
        }
    }

    /// Closes a scope opened by [`Self::push_temp_scope`], restoring the temporary scope
    /// it returned.
    fn close_temp_scope(&mut self, saved: usize, span: Span) {
        self.temp_scope = saved;
        self.pop_scope(span);
    }

    /// Lowers `f`, which evaluates the operands of a call, an aggregate or an assignment
    /// and consumes them, in a scope of its own: rustc's scope of the consuming expression,
    /// where `as_operand` puts the temporaries of its operands. An operand with drop glue
    /// evaluated into a temporary goes there, so a panic in a later operand drops it before
    /// the statement's temporaries made since (`F(D { .. }, G(&D { .. }))`) and before the
    /// locals of a block whose tail it is part of (#54). The consumer moves it, so its drop
    /// when the scope ends finds it uninitialised; drop elaboration removes that.
    ///
    /// When `expr` is a block's tail, its scope is the tail's temporary scope, already open:
    /// the operands go there, among the tail's other temporaries.
    fn in_operand_scope<T>(
        &mut self,
        expr: Option<&Expr>,
        span: Span,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let innermost = self.scopes.len() - 1;
        if expr.is_some_and(|e| self.tail_scope == Some((innermost, e.id))) {
            let saved = self.operand_scope.replace(innermost);
            let out = f(self);
            self.operand_scope = saved;
            return out;
        }
        self.push_scope();
        let saved = self.operand_scope.replace(self.scopes.len() - 1);
        let out = f(self);
        self.operand_scope = saved;
        self.pop_scope(span);
        out
    }

    /// Lowers `f`, which lowers `tail`, the tail expression of a block, in a temporary
    /// scope of its own (edition 2024): its temporaries are dropped once its value is
    /// written, before the block's locals. In rustc the scope is `tail`'s own, which is also
    /// the scope of a call, aggregate or assignment `tail` is.
    fn in_tail_scope(&mut self, tail: &Expr, f: impl FnOnce(&mut Self)) {
        let saved = self.push_temp_scope();
        let tail_scope = (self.scopes.len() - 1, strip_parens(tail).id);
        let saved_tail = self.tail_scope.replace(tail_scope);
        f(self);
        self.tail_scope = saved_tail;
        self.close_temp_scope(saved, tail.span);
    }

    /// Lowers `f` in a temporary scope of its own: an `if`, `else`, `while` or `loop` body,
    /// whose tail expression's temporaries are dropped at its end, after its locals.
    fn in_temp_scope(&mut self, span: Span, f: impl FnOnce(&mut Self)) {
        let saved = self.push_temp_scope();
        f(self);
        self.close_temp_scope(saved, span);
    }

    fn storage_dead(&mut self, locals: &[Local], span: Span) {
        for &local in locals.iter().rev() {
            self.push_stmt(StatementKind::StorageDead(local), span);
        }
    }

    /// Evaluates the condition of an `if` or `while`, or the left operand of `&&`/`||`, in
    /// a temporary scope of its own. Its temporaries with drop glue are dropped at once,
    /// before the branch on the result (a `bool`, never one of them); the caller ends the
    /// storage of all of them at the start of both successors.
    fn condition(&mut self, cond: &Expr, span: Span) -> (Operand, Vec<Local>) {
        let saved = self.push_temp_scope();
        let discr = self.as_operand(cond);
        self.temp_scope = saved;
        let temps = self.scopes.pop().expect("pushed above");
        for &local in temps.iter().rev() {
            self.drop_local(local, span);
        }
        (discr, temps)
    }

    // ----- helpers -----

    /// A `Print`/`PrintLine` argument dereferenced down to the value it prints: `&x`,
    /// `&&s` (rustc's `Display` for references, #26). The printed value is `Copy`. The
    /// argument itself is passed by value, as to the harness's `PrintLine<T: Display>(x:
    /// T)`: a `&mut` is moved, not reborrowed (rustc reborrows at a coercion site only, and
    /// a generic parameter is none), so it is moved into a temporary before the reads
    /// through it (#124).
    fn printed_value(&mut self, mut op: Operand, span: Span) -> Operand {
        let derefs = sabiko_types::print_derefs(&op.ty(&self.body)).unwrap_or(0);
        for _ in 0..derefs {
            let place = match op {
                Operand::Copy(p) => p,
                Operand::Move(_) | Operand::Constant(_) => {
                    let temp = self.new_temp(op.ty(&self.body), span);
                    self.assign(Place::local(temp), Rvalue::Use(op), span);
                    Place::local(temp)
                }
            };
            op = Operand::Copy(place.deref());
        }
        op
    }

    fn ty(&self, expr: &Expr) -> &Ty {
        self.types.expr_ty(expr.id)
    }

    fn mir_local(&self, id: LocalId) -> Local {
        self.local_map[&id]
    }

    /// `copy` for `Copy` types, `move` otherwise.
    fn place_operand(&self, place: Place) -> Operand {
        let env = self.typeck.items.param_env(self.body.def);
        if self.typeck.items.is_copy_in(&place.ty(&self.body), env) {
            Operand::Copy(place)
        } else {
            Operand::Move(place)
        }
    }

    /// How the place expression `expr` of type `&mut T` is reborrowed where it is coerced
    /// (`&mut *p`, or `&*p` where a `&T` is expected, which leaves other shared borrows of
    /// `*p` usable), as the type checker decided (`BodyTypes::reborrows`, #132); `None`
    /// when it is moved, or is not such a place.
    fn reborrow(&self, expr: &Expr) -> Option<Mutability> {
        if !is_place_expr(self.resolve, expr) {
            return None;
        }
        self.types.reborrow(expr)
    }

    /// `expr`'s value as an operand, as [`Self::as_operand`] gives it: a `&mut T` place the
    /// type checker reborrows at this coercion site is reborrowed instead of moved
    /// ([`Self::reborrow`]), so passing a mutable reference to `fn F(x: &mut u8)` does not
    /// consume it, while passing it to `fn Id<T>(x: T)` does. A call argument
    /// (`call_arg`) is reborrowed two-phase, as rustc does at that coercion site only:
    /// `Use(m, m.X)` reads `*m` after the reborrow, before the call.
    fn coerced_operand(&mut self, expr: &Expr, call_arg: bool) -> Operand {
        if let Some(mutability) = self.reborrow(expr) {
            if let Ty::Ref(Mutability::Mut, referent) = self.ty(expr) {
                let referent = (**referent).clone();
                let place = self.as_place(expr);
                if place.local != Local::RETURN || !place.projection.is_empty() {
                    let temp = self.new_temp(Ty::Ref(mutability, Box::new(referent)), expr.span);
                    // A reborrow that is coerced at once (`&mut [T; N]` to `&mut [T]`) is
                    // used, so activated, right there: an ordinary borrow.
                    let coerced = self.types.coercion(expr).is_some();
                    let kind = match mutability {
                        Mutability::Mut => BorrowKind::Mut {
                            two_phase: call_arg && !coerced,
                        },
                        Mutability::Not => BorrowKind::Shared,
                    };
                    self.assign(
                        Place::local(temp),
                        Rvalue::Ref(kind, place.deref()),
                        expr.span,
                    );
                    let op = self.place_operand(Place::local(temp));
                    return self.pointer_coercion(expr, op);
                }
            }
        }
        self.as_operand_moved(expr)
    }

    /// Applies the pointer coercion the type checker recorded for `expr` (unsizing, or
    /// `&mut [T]` to `&[T]`) to `op`, `expr`'s value (reborrowed), into a temporary.
    fn pointer_coercion(&mut self, expr: &Expr, op: Operand) -> Operand {
        let Some(target) = self.types.coercion(expr).cloned() else {
            return op;
        };
        if op.ty(&self.body) == target {
            return op;
        }
        let span = self.span_of(expr);
        let temp = self.new_temp(target.clone(), span);
        self.assign(
            Place::local(temp),
            Rvalue::PointerCoercion(op, target),
            span,
        );
        self.place_operand(Place::local(temp))
    }

    /// `expr`'s value as an operand of its own type, before its pointer coercion.
    fn uncoerced_operand(&mut self, expr: &Expr) -> Operand {
        self.coercing.insert(strip_parens(expr).id);
        let op = self.as_operand(expr);
        self.coercing.remove(&strip_parens(expr).id);
        op
    }

    /// Whether `expr` has a pointer coercion still to apply.
    fn has_pending_coercion(&self, expr: &Expr) -> bool {
        self.types.coercion(expr).is_some() && !self.coercing.contains(&strip_parens(expr).id)
    }

    fn unit_rvalue(span: Span) -> Rvalue {
        Rvalue::Use(Operand::Constant(Constant {
            ty: Ty::Unit,
            value: ConstValue::Unit,
            span,
        }))
    }

    fn bool_constant(value: bool, span: Span) -> Operand {
        Operand::Constant(Constant {
            ty: Ty::BOOL,
            value: ConstValue::Bool(value),
            span,
        })
    }

    /// `-<integer literal>` (through parentheses) as one constant, as rustc builds it: the
    /// exact negation, `-2147483648i32` included, and `-2^127` for `i128`, whose literal
    /// wraps to `i128::MIN` (see [`Self::lit_constant`]). So only a constant item is ever
    /// negated at run time, where negating the minimum overflows.
    fn negated_literal(&mut self, expr: &Expr) -> Option<Constant> {
        let ExprKind::Unary(UnOp::Neg, operand) = &strip_parens(expr).kind else {
            return None;
        };
        let operand = strip_parens(operand);
        let ExprKind::Lit(lit @ Lit::Int { .. }) = &operand.kind else {
            return None;
        };
        let ty = self.ty(operand).clone();
        let mut c = self.lit_constant(lit, &ty, operand.span);
        let ConstValue::Int(v) = c.value else {
            return None;
        };
        c.value = ConstValue::Int(v.wrapping_neg());
        c.span = self.span_of(expr);
        Some(c)
    }

    fn lit_constant(&mut self, lit: &Lit, ty: &Ty, span: Span) -> Constant {
        let value = match lit {
            Lit::Int { value, .. } => {
                if ty.as_prim().is_some_and(PrimTy::is_signed_int) {
                    // The type checker only lets `2^127` through as the operand of a
                    // negation (`-170141183460469231731687303715884105728i128`); wrapping
                    // it to `i128::MIN` keeps `-MIN == MIN` exact. Saturating it to
                    // `i128::MAX` made that literal `MIN + 1`.
                    #[allow(clippy::cast_possible_wrap)]
                    ConstValue::Int(*value as i128)
                } else {
                    ConstValue::Uint(*value)
                }
            }
            Lit::Float { text, .. } => {
                if let Ok(f) = sabiko_lexer::literal::parse_float(text) {
                    ConstValue::Float(f.value)
                } else {
                    // The lexer validated the literal; this cannot happen.
                    self.failed = true;
                    ConstValue::Float(0.0)
                }
            }
            Lit::Bool(b) => ConstValue::Bool(*b),
            Lit::Char(c) => ConstValue::Char(*c),
            Lit::Str(s) => ConstValue::Str(s.clone()),
        };
        Constant {
            ty: ty.clone(),
            value,
            span,
        }
    }

    /// The value of a `const` item. One that failed to evaluate (its errors were reported
    /// with the item) poisons this body.
    /// The constant the path expression `node` names (`def`, as the resolver found it). A
    /// trait's constant is left for `mono::eval_assoc_consts` to evaluate at its `Self`
    /// (#116).
    fn const_constant(&mut self, node: NodeId, def: DefId, ty: &Ty, span: Span) -> Constant {
        let def = self.types.path_defs.get(&node).copied().unwrap_or(def);
        if sabiko_types::consts::is_assoc_const(self.resolve, def) {
            let args = self
                .types
                .generic_args
                .get(&node)
                .cloned()
                .unwrap_or_default();
            return Constant {
                ty: ty.clone(),
                value: ConstValue::Unevaluated(def, args),
                span,
            };
        }
        let value = if let Some(v) = self.typeck.const_value(def) {
            v.clone()
        } else {
            self.failed = true;
            ConstValue::Unit
        };
        Constant {
            ty: ty.clone(),
            value,
            span,
        }
    }

    /// `&e` / `&mut e` where `e` is not a place: the borrow of a temporary. A path naming
    /// a constant (or a unit struct or variant) is a value, not a place: `let r = &A;`
    /// borrows a temporary, extended like `&7`'s.
    fn ref_of_temporary<'e>(&self, expr: &'e Expr) -> Option<(bool, &'e Expr)> {
        if let ExprKind::Ref {
            mutable,
            expr: inner,
        } = &strip_parens(expr).kind
        {
            if !is_place_expr(self.resolve, inner) {
                return Some((*mutable, inner));
            }
        }
        None
    }

    fn path_res(&self, expr: &Expr) -> Option<Res> {
        match &expr.kind {
            // A trait's method the type checker chose for `Type::Name`.
            ExprKind::Path(_) => Some(
                self.types
                    .path_defs
                    .get(&expr.id)
                    .map_or_else(|| self.resolve.res(expr.id), |&d| Res::Def(d)),
            ),
            ExprKind::Paren(inner) => self.path_res(inner),
            _ => None,
        }
    }

    /// The span statements lowered from `expr` carry: its own, or, inside parentheses, that
    /// of the outermost parentheses, as rustc's HIR has it (`(a + b)` overflows at the
    /// `(`), so panic locations agree with rustc's.
    fn span_of(&self, expr: &Expr) -> Span {
        self.paren_spans.get(&expr.id).copied().unwrap_or(expr.span)
    }

    /// Records that `inner`, the contents of the parenthesised `paren`, takes the span of
    /// the outermost parentheses, and returns `inner`.
    fn enter_paren<'e>(&mut self, paren: &Expr, inner: &'e Expr) -> &'e Expr {
        let span = self.span_of(paren);
        self.paren_spans.insert(inner.id, span);
        inner
    }

    fn def_kind(&self, def: DefId) -> DefKind {
        self.resolve.def(def).kind
    }

    // ----- expressions -----

    /// Lowers `expr` to an operand: a constant, or a fresh temporary holding its value.
    ///
    /// Place expressions are copied (or moved) into the temporary at this point rather
    /// than read lazily by the consuming statement, exactly as rustc's `as_operand` does.
    /// The order matters: in `F(p.A, Bump(&mut p))` the first argument must see `p.A`
    /// before `Bump` runs, and a reference temporary such as the result of `Weigh(&mut n)`
    /// in `*Weigh(&mut n) + n.W` must die once its referent is copied, so the sibling
    /// operand can read `n`.
    fn as_operand(&mut self, expr: &Expr) -> Operand {
        if self.reborrow(expr).is_some() {
            return self.coerced_operand(expr, false);
        }
        self.as_operand_moved(expr)
    }

    /// [`Self::as_operand`] without the reborrow of a coerced `&mut` place.
    fn as_operand_moved(&mut self, expr: &Expr) -> Operand {
        if self.place_mutability.is_mut() {
            let saved = std::mem::replace(&mut self.place_mutability, Mutability::Not);
            let op = self.as_operand_moved(expr);
            self.place_mutability = saved;
            return op;
        }
        if self.has_pending_coercion(expr) {
            let op = self.uncoerced_operand(expr);
            return self.pointer_coercion(expr, op);
        }
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.as_operand(inner)
            }
            ExprKind::Lit(lit) => {
                let ty = self.ty(expr).clone();
                Operand::Constant(self.lit_constant(lit, &ty, expr.span))
            }
            ExprKind::Unit => Operand::Constant(Constant {
                ty: Ty::Unit,
                value: ConstValue::Unit,
                span: expr.span,
            }),
            ExprKind::Path(_) if matches!(self.resolve.res(expr.id), Res::Def(def) if self.def_kind(def) == DefKind::Const) =>
            {
                let Res::Def(def) = self.resolve.res(expr.id) else {
                    unreachable!()
                };
                let ty = self.ty(expr).clone();
                Operand::Constant(self.const_constant(expr.id, def, &ty, expr.span))
            }
            ExprKind::Unary(UnOp::Neg, _) if self.negated_literal(expr).is_some() => {
                Operand::Constant(self.negated_literal(expr).expect("checked above"))
            }
            _ => {
                let temp = self.as_temp_in(expr, true);
                self.place_operand(Place::local(temp))
            }
        }
    }

    /// Lowers `expr` to a place. Non-place expressions are evaluated into a temporary.
    fn as_place(&mut self, expr: &Expr) -> Place {
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.as_place(inner)
            }
            ExprKind::Path(_) => match self.resolve.res(expr.id) {
                Res::Local(id) => self.binding_place(id),
                _ => Place::local(self.as_temp(expr)),
            },
            ExprKind::Field(base, _) => {
                let access = self.types.field_accesses[&expr.id];
                let field_ty = self.ty(expr).clone();
                let mut place = self.as_place(base);
                for _ in 0..access.autoderefs {
                    place = place.deref();
                }
                place.field(access.index, field_ty)
            }
            ExprKind::Deref(inner) => {
                let place = self.as_place(inner);
                if place.has_index() {
                    // `*a[i]` (#43): rustc checks `a[i]` at its own span, which starts
                    // after the `*`. A mention checks the indices and reads nothing.
                    self.push_stmt(StatementKind::PlaceMention(place.clone()), inner.span);
                }
                place.deref()
            }
            ExprKind::Index(base, index) => {
                let derefs = self.types.index_derefs[&expr.id];
                let mut place = self.as_place(base);
                for _ in 0..derefs {
                    place = place.deref();
                }
                if let ExprKind::Range(lo, hi, end) = &index.kind {
                    let bounds = (lo.as_deref(), hi.as_deref(), *end == RangeEnd::Included);
                    return self.subslice_place(place, self.span_of(base), bounds, index.span);
                }
                if place.has_index() {
                    // `m[i][j]`: `m[i]` is checked before `j` runs, as in rustc. A
                    // mention of the place checks its indices and reads nothing.
                    self.push_stmt(StatementKind::PlaceMention(place.clone()), expr.span);
                }
                let index = self.as_temp(index);
                place.index(index)
            }
            _ => Place::local(self.as_temp(expr)),
        }
    }

    /// [`Self::as_place`] for a place used as `m` says (see `place_mutability`).
    fn as_place_mut(&mut self, expr: &Expr, m: Mutability) -> Place {
        let saved = std::mem::replace(&mut self.place_mutability, m);
        let place = self.as_place(expr);
        self.place_mutability = saved;
        place
    }

    /// `base[lo..hi]` as a place (#10), lowered as rustc lowers its `Index::index` (or
    /// `IndexMut::index_mut`) call: the base is borrowed first (`base_span`, an array's
    /// borrow unsized to a slice), then the bounds are evaluated, then the sub-slice is
    /// taken (`range_span`, which starts at the `[`, locates its panic). The place is the
    /// sub-slice behind the resulting reference.
    fn subslice_place(
        &mut self,
        base: Place,
        base_span: Span,
        (lo, hi, inclusive): (Option<&Expr>, Option<&Expr>, bool),
        range_span: Span,
    ) -> Place {
        let m = self.place_mutability;
        let base_ty = base.ty(&self.body);
        let borrow = self.new_temp(Ty::Ref(m, Box::new(base_ty.clone())), base_span);
        self.assign(
            Place::local(borrow),
            Rvalue::Ref(BorrowKind::new(m), base),
            base_span,
        );
        let mut reference = self.place_operand(Place::local(borrow));
        if let Ty::Array(elem, _) = base_ty {
            let slice = Ty::Ref(m, Box::new(Ty::Slice(elem)));
            let unsized_ref = self.new_temp(slice.clone(), base_span);
            self.assign(
                Place::local(unsized_ref),
                Rvalue::PointerCoercion(reference, slice),
                base_span,
            );
            reference = self.place_operand(Place::local(unsized_ref));
        }
        let lo = lo.map(|e| self.as_operand(e));
        let hi = hi.map(|e| self.as_operand(e));
        let ty = reference.ty(&self.body);
        let result = self.new_temp(ty, range_span);
        self.assign(
            Place::local(result),
            Rvalue::Subslice(reference, Box::new(SliceBounds { lo, hi, inclusive })),
            range_span,
        );
        Place::local(result).deref()
    }

    /// Evaluates `expr` into a fresh temporary.
    fn as_temp(&mut self, expr: &Expr) -> Local {
        self.as_temp_in(expr, false)
    }

    /// [`Self::as_temp`]; the temporary of an `operand` goes where
    /// [`Self::new_operand_temp`] puts it.
    fn as_temp_in(&mut self, expr: &Expr, operand: bool) -> Local {
        if self.place_mutability.is_mut() {
            let saved = std::mem::replace(&mut self.place_mutability, Mutability::Not);
            let temp = self.as_temp_in(expr, operand);
            self.place_mutability = saved;
            return temp;
        }
        let ty = self.ty(expr).clone();
        let temp = if operand {
            self.new_operand_temp(ty, expr.span)
        } else {
            self.new_temp(ty, expr.span)
        };
        if !writes_once(expr) {
            self.open_pending(temp);
        }
        self.expr_into_dest(&Place::local(temp), expr);
        self.temp_complete(temp);
        temp
    }

    /// Lowers `expr` and writes its value into `dest`. Diverging expressions write
    /// nothing and leave the builder in an unreachable block.
    #[allow(clippy::too_many_lines)]
    fn expr_into_dest(&mut self, dest: &Place, expr: &Expr) {
        let span = self.span_of(expr);
        if self.reborrow(expr).is_some() {
            // A coerced `&mut` place: a block's tail, an arm, a `break` value, an
            // annotated `let` initialiser or a returned value.
            let op = self.coerced_operand(expr, false);
            self.assign(dest.clone(), Rvalue::Use(op), span);
            return;
        }
        if self.has_pending_coercion(expr) {
            let op = self.uncoerced_operand(expr);
            let target = self.types.coercion(expr).expect("pending").clone();
            self.assign(dest.clone(), Rvalue::PointerCoercion(op, target), span);
            return;
        }
        // An aggregate's operands are in its scope; for any other expression the scope is
        // empty and leaves nothing behind.
        let done = self.in_operand_scope(Some(expr), span, |this| {
            let Some(rvalue) = this.aggregate_rvalue(expr) else {
                return false;
            };
            this.assign(dest.clone(), rvalue, span);
            true
        });
        if done {
            return;
        }
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.expr_into_dest(dest, inner);
            }
            ExprKind::Lit(_) | ExprKind::Unit => {
                let op = self.as_operand(expr);
                self.assign(dest.clone(), Rvalue::Use(op), span);
            }
            // A place read straight into the destination (`let x = p.A;`), as rustc does:
            // only operands of a larger expression go through a temporary.
            ExprKind::Field(..) | ExprKind::Deref(_) | ExprKind::Index(..) => {
                let place = self.as_place(expr);
                let op = self.place_operand(place);
                self.assign(dest.clone(), Rvalue::Use(op), span);
            }
            ExprKind::Repeat(value, _) => {
                // `[x; 0]` (any other repeat is an aggregate): it evaluates `x` and holds
                // nothing, so rustc drops `x` at once, before anything else in the
                // statement runs (`F([d; 0], G())` drops `d` before `G` runs). The drop at
                // the end of the statement's scope finds the temporary uninitialised; drop
                // elaboration removes it.
                let Ty::Array(elem, _) = self.ty(expr).clone() else {
                    unreachable!("a repeat expression has an array type")
                };
                let temp = self.as_temp(value);
                self.drop_local(temp, span);
                self.assign(
                    dest.clone(),
                    Rvalue::Aggregate(AggregateKind::Array(*elem), Vec::new()),
                    span,
                );
            }
            ExprKind::Array(_) | ExprKind::Tuple(_) | ExprKind::Struct { .. } => {
                unreachable!("an aggregate is lowered by `aggregate_rvalue`")
            }
            ExprKind::Path(_) => match self.resolve.res(expr.id) {
                Res::Local(id) => {
                    let op = self.place_operand(self.binding_place(id));
                    self.assign(dest.clone(), Rvalue::Use(op), span);
                }
                Res::Def(def) => match self.def_kind(def) {
                    DefKind::Const => {
                        let op = self.as_operand(expr);
                        self.assign(dest.clone(), Rvalue::Use(op), span);
                    }
                    DefKind::Struct | DefKind::Variant => {
                        unreachable!("a unit struct or variant is lowered by `aggregate_rvalue`")
                    }
                    DefKind::Fn
                    | DefKind::Enum
                    | DefKind::TyParam
                    | DefKind::Trait
                    | DefKind::Impl => {
                        unreachable!("functions, enums and type parameters as values are rejected")
                    }
                },
                Res::Prim(_) | Res::Err => unreachable!("unresolved path in a checked body"),
            },
            ExprKind::Unary(UnOp::Neg, _) if self.negated_literal(expr).is_some() => {
                let c = self.negated_literal(expr).expect("checked above");
                self.assign(dest.clone(), Rvalue::Use(Operand::Constant(c)), span);
            }
            ExprKind::Unary(op, operand) => {
                let operand = self.as_operand(operand);
                self.assign(dest.clone(), Rvalue::UnaryOp(*op, operand), span);
            }
            ExprKind::Binary(op @ (BinOp::And | BinOp::Or), lhs, rhs) => {
                self.lower_logical(dest, *op, lhs, rhs, span);
            }
            ExprKind::Binary(op, lhs, rhs)
                if op.is_comparison() && self.comparison_derefs(lhs, rhs) > 0 =>
            {
                self.lower_ref_comparison(dest, *op, lhs, rhs, span);
            }
            ExprKind::Binary(op, lhs, rhs) => {
                let (lhs, fake) = match self.compared_ref_place(*op, lhs) {
                    Some((lhs, fake)) => (lhs, Some(fake)),
                    None => (self.as_operand(lhs), None),
                };
                let rhs = self.as_operand(rhs);
                if let Some(fake) = fake {
                    self.push_stmt(StatementKind::PlaceMention(Place::local(fake)), span);
                }
                self.assign(dest.clone(), Rvalue::BinaryOp(*op, lhs, rhs), span);
            }
            ExprKind::Assign(..) | ExprKind::AssignOp(..) => {
                self.lower_assign(expr);
                self.assign(dest.clone(), Self::unit_rvalue(span), span);
            }
            ExprKind::DestructuringAssign(pat, rhs) => {
                self.lower_destructuring_assign(pat, rhs, span);
                self.assign(dest.clone(), Self::unit_rvalue(span), span);
            }
            ExprKind::Underscore => unreachable!("the parser rejects `_` outside an assignee"),
            ExprKind::Call { callee, args } => {
                let Some(Res::Def(func)) = self.path_res(callee) else {
                    unreachable!("callees are function paths in a checked body")
                };
                let generic_args = self
                    .types
                    .generic_args
                    .get(&strip_parens(callee).id)
                    .cloned()
                    .unwrap_or_default();
                let print = matches!(
                    self.typeck.items.builtin(func),
                    Some(Builtin::Print | Builtin::PrintLine)
                );
                self.in_operand_scope(Some(expr), span, |this| {
                    let args: Vec<Operand> = args
                        .iter()
                        .map(|a| {
                            if print {
                                let op = this.as_operand(a);
                                this.printed_value(op, a.span)
                            } else {
                                this.coerced_operand(a, true)
                            }
                        })
                        .collect();
                    this.call_into(dest, expr, func, generic_args, args, span);
                });
            }
            ExprKind::Ref {
                mutable,
                expr: inner,
            } => {
                // `&loop {}`, `&break 4`: the borrow of a diverging operand (typed `&!`)
                // has no value to borrow; only the operand runs, and what follows is
                // unreachable. The destination is never written.
                if self.ty(inner).is_never() {
                    self.lower_expr_discard(inner);
                } else {
                    let place = self.as_place_mut(inner, Mutability::from_bool(*mutable));
                    if place.has_index() {
                        // `&mut a[i]`: rustc checks the index at the indexing expression,
                        // before the borrow; a panic there is located at `a[i]`, not at
                        // the `&` (a mention checks the indices and reads nothing).
                        self.push_stmt(
                            StatementKind::PlaceMention(place.clone()),
                            self.span_of(inner),
                        );
                    }
                    self.assign(
                        dest.clone(),
                        Rvalue::Ref(BorrowKind::new(Mutability::from_bool(*mutable)), place),
                        span,
                    );
                }
            }
            ExprKind::Cast(inner, _) => {
                let target = self.ty(expr).clone();
                let operand = self.as_operand(inner);
                self.assign(dest.clone(), Rvalue::Cast(operand, target), span);
            }
            ExprKind::Block(block) => self.lower_block_into(dest, block, false),
            ExprKind::If { cond, then, els } => {
                self.lower_if(
                    Some(ValueDest::plain(dest)),
                    cond,
                    then,
                    els.as_deref(),
                    span,
                );
            }
            ExprKind::While { cond, body } => {
                self.lower_while(cond, body, span);
                self.assign(dest.clone(), Self::unit_rvalue(span), span);
            }
            ExprKind::Loop { body } => self.lower_loop(Some(dest.clone()), body, span),
            ExprKind::Break(value) => self.lower_break(value.as_deref(), span),
            ExprKind::Continue => self.lower_continue(span),
            ExprKind::Return(value) => self.lower_return(value.as_deref(), span),
            ExprKind::Try(operand) => self.lower_try(dest, expr, operand),
            ExprKind::Match { scrutinee, arms } => {
                self.lower_match(Some(ValueDest::plain(dest)), scrutinee, arms, span);
            }
            ExprKind::Let(..) => unreachable!("`let` is only lowered as an `if`/`while` condition"),
            ExprKind::Range(..) => unreachable!("a range is only lowered as the index of `e[..]`"),
            ExprKind::MethodCall { receiver, .. }
                if self.types.len_calls.contains_key(&expr.id) =>
            {
                // `e.len()`: the receiver is borrowed shared (rustc's autoref), and the
                // length read through the borrow.
                let mut place = self.as_place(receiver);
                for _ in 0..self.types.len_calls[&expr.id] {
                    place = place.deref();
                }
                let ty = Ty::shared_ref(place.ty(&self.body));
                let temp = self.new_temp(ty, receiver.span);
                self.assign(
                    Place::local(temp),
                    Rvalue::Ref(BorrowKind::Shared, place),
                    receiver.span,
                );
                self.assign(dest.clone(), Rvalue::Len(Place::local(temp).deref()), span);
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.in_operand_scope(Some(expr), span, |this| {
                    this.lower_method_call(dest, expr, receiver, args, span);
                });
            }
        }
    }

    /// Ends the current block with a call of `func` writing into `dest`, and continues
    /// after it. A callee returning `!` (`Panic`) has no return edge; what follows lands
    /// in an unreachable block, as after `return`.
    fn call_into(
        &mut self,
        dest: &Place,
        expr: &Expr,
        func: DefId,
        generic_args: Vec<Ty>,
        args: Vec<Operand>,
        span: Span,
    ) {
        let fn_span = match &expr.kind {
            ExprKind::MethodCall { method, .. } => {
                Span::new(span.file, method.span.start, span.end.max(method.span.end))
            }
            _ => span,
        };
        let next = self.new_block();
        let target = if self.ty(expr).is_never() {
            None
        } else {
            Some(next)
        };
        self.terminate(
            TerminatorKind::Call {
                func,
                generic_args,
                args,
                destination: dest.clone(),
                target,
                fn_span,
            },
            span,
        );
        self.enter(next);
        if target.is_some() && dest.is_local() {
            self.open_pending(dest.local);
        }
    }

    /// `receiver.Name(args)`: a call of the method the type checker picked, whose first
    /// argument is the receiver after its adjustments (decision 53). The receiver is
    /// evaluated first, then the arguments left to right, as in rustc:
    /// - an autoref'd receiver is a borrow of the receiver place (of a temporary for any
    ///   other expression), dereferenced `autoderefs` times. A `&mut` one is two-phase
    ///   (#15): only reserved here and activated by the call, so the arguments may still
    ///   read the place (`v.Push(v.Len())`);
    /// - a by-value receiver is copied or moved into a temporary before the arguments run
    ///   (decision 36): `c.Plus(Change(&mut c))` passes the old `c`.
    fn lower_method_call(
        &mut self,
        dest: &Place,
        expr: &Expr,
        receiver: &Expr,
        args: &[Expr],
        span: Span,
    ) {
        let callee = self.types.method_calls[&expr.id];
        let generic_args = self
            .types
            .generic_args
            .get(&expr.id)
            .cloned()
            .unwrap_or_default();
        let recv = match callee.autoref {
            Some(m) => {
                let mut place = self.as_place_mut(receiver, m);
                for _ in 0..callee.autoderefs {
                    place = place.deref();
                }
                let ty = Ty::Ref(m, Box::new(place.ty(&self.body)));
                let temp = self.new_temp(ty, receiver.span);
                // A borrow unsized at once is used, so activated, right there: an
                // ordinary one.
                let kind = match m {
                    Mutability::Mut => BorrowKind::Mut {
                        two_phase: !callee.unsize,
                    },
                    Mutability::Not => BorrowKind::Shared,
                };
                self.assign(Place::local(temp), Rvalue::Ref(kind, place), receiver.span);
                let borrowed = self.place_operand(Place::local(temp));
                match self.body.local_decl(temp).ty.clone() {
                    // `a.get(i)` on an array: the borrow is unsized to the slice the
                    // method takes, as rustc's probe adjusts it.
                    Ty::Ref(m, inner) if callee.unsize => {
                        let Ty::Array(elem, _) = *inner else {
                            unreachable!("only an array receiver is unsized")
                        };
                        let slice = Ty::Ref(m, Box::new(Ty::Slice(elem)));
                        let unsized_ref = self.new_temp(slice.clone(), receiver.span);
                        self.assign(
                            Place::local(unsized_ref),
                            Rvalue::PointerCoercion(borrowed, slice),
                            receiver.span,
                        );
                        self.place_operand(Place::local(unsized_ref))
                    }
                    _ => borrowed,
                }
            }
            None if callee.autoderefs == 0 => self.as_operand(receiver),
            None => {
                let mut place = self.as_place(receiver);
                for _ in 0..callee.autoderefs {
                    place = place.deref();
                }
                let temp = self.new_temp(place.ty(&self.body), receiver.span);
                let op = self.place_operand(place);
                self.assign(Place::local(temp), Rvalue::Use(op), receiver.span);
                self.place_operand(Place::local(temp))
            }
        };
        let mut operands = vec![recv];
        for a in args {
            operands.push(self.coerced_operand(a, true));
        }
        self.call_into(dest, expr, callee.def, generic_args, operands, span);
    }

    /// An aggregate expression's operands, evaluated left to right, and the aggregate
    /// built from them, not yet stored anywhere (rustc's `as_rvalue`): an array, a repeat
    /// of at least one element, a tuple, a struct or variant literal, a variant
    /// constructor call or a unit struct or variant. `None` for any other expression, and
    /// for an aggregate with a pointer coercion still to apply; nothing is lowered then.
    /// `place = [..]` uses it to evaluate the place between the operands and the store
    /// (`lower_assign`).
    /// A struct or struct-variant literal: its operands in declaration order, evaluated in
    /// source order.
    fn struct_literal(&mut self, expr: &Expr, fields: &[sabiko_ast::FieldInit]) -> Rvalue {
        let Res::Def(def) = self.resolve.res(expr.id) else {
            unreachable!("unresolved struct literal in a checked body")
        };
        let variant = self.types.variant_exprs.get(&expr.id).copied();
        let generic = self.adt_args(expr);
        let (kind, adt, mut field_defs) = match variant {
            Some((adt, index)) => (
                AggregateKind::Variant(adt, index, generic.clone()),
                adt,
                self.typeck.items.adt(adt).variant(index).fields.clone(),
            ),
            None => (
                AggregateKind::Adt(def, generic.clone()),
                def,
                self.typeck.items.adt(def).fields.clone(),
            ),
        };
        let adt_def = self.typeck.items.adt(adt);
        for f in &mut field_defs {
            f.ty = adt_def.field_ty(&f.ty, &generic);
        }
        // Initialisers run in source order; the aggregate lists declaration order.
        let mut operands: Vec<Option<Operand>> = vec![None; field_defs.len()];
        let indices: Vec<usize> = fields
            .iter()
            .map(|f| {
                field_defs
                    .iter()
                    .position(|d| d.name == f.name.name)
                    .expect("struct literal fields exist in a checked body")
            })
            .collect();
        for (field, index) in fields.iter().zip(indices) {
            operands[index] = Some(self.as_operand(&field.value));
        }
        let operands = operands
            .into_iter()
            .map(|op| op.expect("every field is initialised in a checked body"))
            .collect();
        Rvalue::Aggregate(kind, operands)
    }

    /// The generic arguments of the struct or enum `expr` builds (its type's).
    fn adt_args(&self, expr: &Expr) -> Box<[Ty]> {
        match self.ty(expr) {
            Ty::Adt(_, args) => args.clone(),
            _ => Box::new([]),
        }
    }

    fn aggregate_rvalue(&mut self, expr: &Expr) -> Option<Rvalue> {
        if self.has_pending_coercion(expr) {
            return None;
        }
        let rvalue = match &expr.kind {
            ExprKind::Array(elems) => {
                let Ty::Array(elem, _) = self.ty(expr).clone() else {
                    unreachable!("an array expression has an array type")
                };
                // Each element is a coercion site, evaluated left to right.
                let operands = elems.iter().map(|e| self.as_operand(e)).collect();
                Rvalue::Aggregate(AggregateKind::Array(*elem), operands)
            }
            ExprKind::Repeat(value, _) => {
                let n = self.types.repeat_counts[&expr.id];
                if n == 0 {
                    // Lowered by `expr_into_dest`, which drops `value` at once.
                    return None;
                }
                let op = self.as_operand(value);
                Rvalue::Repeat(op, n)
            }
            ExprKind::Tuple(_) => {
                Rvalue::Aggregate(AggregateKind::Tuple, self.tuple_operands(expr, false))
            }
            ExprKind::Path(_) => match self.resolve.res(expr.id) {
                Res::Def(def) if self.def_kind(def) == DefKind::Struct => {
                    let args = self.adt_args(expr);
                    Rvalue::Aggregate(AggregateKind::Adt(def, args), Vec::new())
                }
                Res::Def(def) if self.def_kind(def) == DefKind::Variant => {
                    let (adt, index) = self.types.variant_exprs[&expr.id];
                    let args = self.adt_args(expr);
                    Rvalue::Aggregate(AggregateKind::Variant(adt, index, args), Vec::new())
                }
                _ => return None,
            },
            ExprKind::Call { args, .. } if self.types.variant_exprs.contains_key(&expr.id) => {
                // `E::V(a, b)`: the variant's fields, in order.
                let (adt, index) = self.types.variant_exprs[&expr.id];
                let generic = self.adt_args(expr);
                let operands = args.iter().map(|a| self.as_operand(a)).collect();
                Rvalue::Aggregate(AggregateKind::Variant(adt, index, generic), operands)
            }
            ExprKind::Struct { fields, .. } => self.struct_literal(expr, fields),
            _ => return None,
        };
        Some(rvalue)
    }

    /// Lowers `expr` into `dest`, in an extending position ([`Self::extending_into`]) or
    /// not.
    fn value_into(&mut self, dest: &Place, expr: &Expr, extending: bool) {
        if extending {
            self.extending_into(dest, expr);
        } else {
            self.expr_into_dest(dest, expr);
        }
    }

    /// Lowers `expr`, an *extending expression* of a `let` initialiser, into `dest`:
    /// rustc's temporary lifetime extension. The initialiser itself is one, and so are the
    /// elements of an extending tuple, the tail of an extending block, both branches of an
    /// extending `if`/`else` and the arms of an extending `match`. The operand of `&` at
    /// one of these positions lives as long as the `let`'s bindings, not to the end of its
    /// statement or its block (`let r = { &Make(1) };`, `let r = if c { &F() } else {
    /// &G() };`).
    fn extending_into(&mut self, dest: &Place, expr: &Expr) {
        let span = self.span_of(expr);
        if let Some((mutable, inner)) = self
            .ref_of_temporary(expr)
            .filter(|(_, inner)| !self.ty(inner).is_never())
        {
            self.extended_borrow_into(dest, expr, mutable, inner);
            return;
        }
        match &expr.kind {
            // `let p = (&<rvalue>, ..);` extends the rvalue too.
            ExprKind::Tuple(_) => self.tuple_into(dest, expr, true),
            // A coerced value (`let s: &[i32] = { &[1, 2] };` unsizes the block's value)
            // is not extended: lowered as usual, rustc's extension of it is rejected
            // (E0413), never miscompiled.
            _ if self.has_pending_coercion(expr) => self.expr_into_dest(dest, expr),
            // `let a = [&F(), &G()];`
            ExprKind::Array(elems) => self.extending_array_into(dest, expr, elems),
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.extending_into(dest, inner);
            }
            ExprKind::Block(block) => {
                self.declare_extended(expr);
                self.lower_block_into(dest, block, true);
            }
            ExprKind::If { cond, then, els } => {
                self.declare_extended(expr);
                self.lower_if(
                    Some(ValueDest::extending(dest)),
                    cond,
                    then,
                    els.as_deref(),
                    span,
                );
            }
            ExprKind::Match { scrutinee, arms } => {
                self.declare_extended(expr);
                self.lower_match(Some(ValueDest::extending(dest)), scrutinee, arms, span);
            }
            _ => self.expr_into_dest(dest, expr),
        }
    }

    /// Evaluates `inner`, the operand of `&` in an extending position, into the temporary
    /// that lives as long as the `let`'s bindings, and returns it: the one declared ahead
    /// for it, or a new one.
    fn extended_referent(&mut self, inner: &Expr) -> Local {
        if let Some(referent) = self.extended.remove(&inner.id) {
            self.extending_into(&Place::local(referent), inner);
            return referent;
        }
        let ty = self.ty(inner).clone();
        let referent = self.new_temp_in_block_scope(ty, inner.span);
        if !writes_once(inner) {
            self.open_pending(referent);
        }
        // The operand of an extending borrow is in an extending position itself:
        // `let d = &&&x;` keeps both temporaries.
        self.extending_into(&Place::local(referent), inner);
        self.temp_complete(referent);
        referent
    }

    /// Declares, before a block, `if` or `match` in an extending position, the temporaries
    /// of the borrows in extending positions under it, in source order, with their cleanup
    /// scopes open from here (as for `let d; if c { d = A(); }`). A borrow under a branch
    /// writes its temporary on that path only, and one in a block's tail outlives the
    /// block's locals, whose cleanup scopes open later: a cleanup scope opened at the
    /// write would have several entries, or not nest.
    fn declare_extended(&mut self, expr: &Expr) {
        let mut operands = Vec::new();
        self.extended_borrows(expr, &mut operands);
        for inner in operands {
            if self.extended.contains_key(&inner.id) {
                continue;
            }
            let ty = self.ty(inner).clone();
            let referent = self.new_temp_in_block_scope(ty, inner.span);
            self.open_pending(referent);
            self.extended.insert(inner.id, referent);
        }
    }

    /// Whether a borrow of a temporary is in an extending position of `expr`.
    fn has_extended_borrow(&self, expr: &Expr) -> bool {
        let mut operands = Vec::new();
        self.extended_borrows(expr, &mut operands);
        !operands.is_empty()
    }

    /// The operands of the borrows of temporaries in extending positions of `expr`, in
    /// source order: the positions [`Self::extending_into`] extends.
    fn extended_borrows<'e>(&self, expr: &'e Expr, out: &mut Vec<&'e Expr>) {
        if let Some((_, inner)) = self.ref_of_temporary(expr) {
            if !self.ty(inner).is_never() {
                out.push(inner);
                self.extended_borrows(inner, out);
            }
            return;
        }
        if !matches!(expr.kind, ExprKind::Tuple(_)) && self.has_pending_coercion(expr) {
            return;
        }
        match &expr.kind {
            ExprKind::Paren(inner) => self.extended_borrows(inner, out),
            ExprKind::Tuple(elems) | ExprKind::Array(elems) => {
                for e in elems {
                    self.extended_borrows(e, out);
                }
            }
            ExprKind::Block(block) => {
                if let Some(tail) = &block.tail {
                    self.extended_borrows(tail, out);
                }
            }
            ExprKind::If { then, els, .. } => {
                if let Some(tail) = &then.tail {
                    self.extended_borrows(tail, out);
                }
                if let Some(els) = els {
                    self.extended_borrows(els, out);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.extended_borrows(&arm.body, out);
                }
            }
            _ => {}
        }
    }

    /// `&<rvalue>` in an extending position, into `dest`: the rvalue goes into a temporary
    /// that lives as long as the `let`'s bindings (see [`Self::new_temp_in_block_scope`]).
    fn extended_borrow_into(&mut self, dest: &Place, expr: &Expr, mutable: bool, inner: &Expr) {
        let ty = self.ty(inner).clone();
        let temp = self.extended_referent(inner);
        let mutability = Mutability::from_bool(mutable);
        let borrow = Rvalue::Ref(BorrowKind::new(mutability), Place::local(temp));
        if let Some(target) = self.types.coercion(expr).cloned() {
            // `let s: &[i32] = &[1, 2];`: the borrow is unsized.
            let r = self.new_temp(Ty::Ref(mutability, Box::new(ty)), expr.span);
            self.assign(Place::local(r), borrow, expr.span);
            let op = self.place_operand(Place::local(r));
            self.assign(dest.clone(), Rvalue::PointerCoercion(op, target), expr.span);
        } else {
            self.assign(dest.clone(), borrow, expr.span);
        }
    }

    /// `[a, b, ...]` in an extending position, into `dest`: as for an array anywhere else
    /// (each element is a coercion site), but each element is in an extending position.
    fn extending_array_into(&mut self, dest: &Place, expr: &Expr, elems: &[Expr]) {
        let Ty::Array(elem, _) = self.ty(expr).clone() else {
            unreachable!("an array expression has an array type")
        };
        let span = self.span_of(expr);
        self.in_operand_scope(Some(expr), span, |this| {
            let mut operands = Vec::with_capacity(elems.len());
            for e in elems {
                let op = match this.extended_operand(e) {
                    Some(op) => op,
                    None => this.as_operand(e),
                };
                operands.push(op);
            }
            this.assign(
                dest.clone(),
                Rvalue::Aggregate(AggregateKind::Array(*elem), operands),
                span,
            );
        });
    }

    /// `(a, b, ...)` into `dest`. See [`Self::tuple_operands`].
    fn tuple_into(&mut self, dest: &Place, expr: &Expr, extending: bool) {
        let span = self.span_of(expr);
        self.in_operand_scope(Some(expr), span, |this| {
            let operands = this.tuple_operands(expr, extending);
            this.assign(
                dest.clone(),
                Rvalue::Aggregate(AggregateKind::Tuple, operands),
                span,
            );
        });
    }

    /// The elements of `(a, b, ...)`, left to right; at a coercion site each is coerced to
    /// its element type (a `&mut` place is reborrowed), elsewhere a `&mut` place element
    /// is moved, as in rustc. In a `let` initialiser (`extending`), the operand of an
    /// element `&<rvalue>` lives as long as the binding, and so on into nested tuples
    /// (rustc's temporary lifetime extension: `let p = (&f(), 1);` keeps `f()`'s value).
    fn tuple_operands(&mut self, expr: &Expr, extending: bool) -> Vec<Operand> {
        let ExprKind::Tuple(elems) = &strip_parens(expr).kind else {
            unreachable!("not a tuple expression")
        };
        elems
            .iter()
            .map(|e| {
                if extending {
                    if let Some(op) = self.extended_operand(e) {
                        return op;
                    }
                }
                self.as_operand(e)
            })
            .collect()
    }

    /// An operand of a `let` initialiser whose temporary rustc extends to the binding's
    /// block: `&<rvalue>` (the rvalue goes into a block-scoped temporary) or a tuple
    /// holding one. `None` for anything else, which is lowered as usual.
    fn extended_operand(&mut self, e: &Expr) -> Option<Operand> {
        if let Some(temp) = self.extended_ref(e) {
            return Some(self.place_operand(Place::local(temp)));
        }
        let nested = match strip_parens(e).kind {
            ExprKind::Tuple(_) => true,
            ExprKind::Array(_)
            | ExprKind::Block(_)
            | ExprKind::If { .. }
            | ExprKind::Match { .. } => self.has_extended_borrow(e),
            _ => false,
        };
        if nested {
            // `let p = ((&F(), 1), { &G() });`
            let ty = self.ty(e).clone();
            let temp = self.new_operand_temp(ty, e.span);
            self.extending_into(&Place::local(temp), e);
            self.temp_complete(temp);
            return Some(self.place_operand(Place::local(temp)));
        }
        None
    }

    /// `&<rvalue>` in a `let` initialiser: evaluates the rvalue into a temporary that lives
    /// as long as the enclosing block, and returns a statement temporary holding the
    /// reference to it. `None` when `e` is not such a borrow (or borrows a diverging
    /// expression, which has nothing to keep).
    fn extended_ref(&mut self, e: &Expr) -> Option<Local> {
        let (mutable, inner) = self
            .ref_of_temporary(e)
            .filter(|(_, inner)| !self.ty(inner).is_never())?;
        let referent = self.extended_referent(inner);
        let ref_ty = self.ty(e).clone();
        let temp = self.new_temp(ref_ty, e.span);
        self.assign(
            Place::local(temp),
            Rvalue::Ref(
                BorrowKind::new(Mutability::from_bool(mutable)),
                Place::local(referent),
            ),
            e.span,
        );
        if let Some(target) = self.types.coercion(e).cloned() {
            // An unsized borrow (`&[1, 2]` as a `&[i32]` element).
            let op = self.place_operand(Place::local(temp));
            let coerced = self.new_temp(target.clone(), e.span);
            self.assign(
                Place::local(coerced),
                Rvalue::PointerCoercion(op, target),
                e.span,
            );
            return Some(coerced);
        }
        Some(temp)
    }

    /// Lowers an expression whose value is not needed (an expression statement). The
    /// statement-like forms need no destination; anything else goes into a temporary that
    /// dies with the statement.
    fn lower_expr_discard(&mut self, expr: &Expr) {
        let span = self.span_of(expr);
        match &expr.kind {
            ExprKind::Paren(inner) => {
                let inner = self.enter_paren(expr, inner);
                self.lower_expr_discard(inner);
            }
            ExprKind::Assign(..) | ExprKind::AssignOp(..) => self.lower_assign(expr),
            ExprKind::DestructuringAssign(pat, rhs) => {
                self.lower_destructuring_assign(pat, rhs, span);
            }
            ExprKind::Block(block) => self.lower_block_discard(block),
            ExprKind::If { cond, then, els } => {
                self.lower_if(None, cond, then, els.as_deref(), span);
            }
            ExprKind::While { cond, body } => self.lower_while(cond, body, span),
            ExprKind::Match { scrutinee, arms } => self.lower_match(None, scrutinee, arms, span),
            ExprKind::Loop { body } => self.lower_loop(None, body, span),
            ExprKind::Break(value) => self.lower_break(value.as_deref(), span),
            ExprKind::Continue => self.lower_continue(span),
            ExprKind::Return(value) => self.lower_return(value.as_deref(), span),
            _ => {
                self.as_temp(expr);
            }
        }
    }

    /// How many references both operands of a comparison have: `r == s` for `r, s: &&i32`
    /// compares `**r` with `**s`. Zero for primitive operands and when either side
    /// diverges.
    fn comparison_derefs(&self, lhs: &Expr, rhs: &Expr) -> usize {
        let (mut a, mut b) = (self.ty(lhs), self.ty(rhs));
        let mut derefs = 0;
        while let (Ty::Ref(_, x), Ty::Ref(_, y)) = (a, b) {
            if **x == Ty::Prim(PrimTy::Str) {
                // `&str` operands are compared as they are (#10): `str` is unsized.
                break;
            }
            a = x;
            b = y;
            derefs += 1;
        }
        derefs
    }

    /// `r == s` (or any comparison) on references compares the referents, as Rust's
    /// `PartialEq`/`PartialOrd` impls for `&A` do. Each operand is evaluated to a shared
    /// reference first and both are dereferenced in the comparison itself, after both
    /// were evaluated: rustc passes `&r` and `&s` to `PartialEq::eq`, so the referents
    /// stay borrowed while the right operand runs (`r == { *r = 3; s }` is a conflict),
    /// and a `&mut` operand is only borrowed, not moved. The reference `r` itself stays
    /// borrowed too (`r == { r = &y; r }` is a conflict): a left place operand also gets a
    /// shallow fake borrow, mentioned once the right operand ran (decision 63). The right
    /// operand needs none, as nothing runs between its evaluation and the comparison.
    fn lower_ref_comparison(
        &mut self,
        dest: &Place,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) {
        let derefs = self.comparison_derefs(lhs, rhs);
        let (mut lhs, fake) = self.comparison_operand(lhs, true);
        let (mut rhs, _) = self.comparison_operand(rhs, false);
        if let Some(fake) = fake {
            self.push_stmt(StatementKind::PlaceMention(Place::local(fake)), span);
        }
        for _ in 0..derefs {
            lhs = lhs.deref();
            rhs = rhs.deref();
        }
        let lhs = self.place_operand(lhs);
        let rhs = self.place_operand(rhs);
        self.assign(dest.clone(), Rvalue::BinaryOp(op, lhs, rhs), span);
    }

    /// A reference operand of a comparison, as a temporary holding the reference. A place
    /// is reborrowed shared (`&*r`), so a `&mut` place is not moved; anything else is
    /// evaluated into the temporary. With `fake`, a place also gets a shallow fake borrow
    /// (`&fake shallow r`, see [`FakeBorrowKind::Shallow`]), whose temporary is returned
    /// for the caller to mention where the borrow must still be live.
    fn comparison_operand(&mut self, expr: &Expr, fake: bool) -> (Place, Option<Local>) {
        let is_place = match &strip_parens(expr).kind {
            ExprKind::Path(_) => matches!(self.path_res(expr), Some(Res::Local(_))),
            ExprKind::Field(..) | ExprKind::Deref(_) | ExprKind::Index(..) => true,
            _ => false,
        };
        if !is_place {
            return (Place::local(self.as_temp(expr)), None);
        }
        let ty = self.ty(expr).clone();
        let Ty::Ref(_, referent) = ty.clone() else {
            unreachable!("comparison operands with derefs are references")
        };
        let place = self.as_place(expr);
        let temp = self.new_temp(Ty::shared_ref(*referent), expr.span);
        self.assign(
            Place::local(temp),
            Rvalue::Ref(BorrowKind::Shared, place.clone().deref()),
            expr.span,
        );
        let fake = fake.then(|| {
            let fake = self.new_temp(Ty::shared_ref(ty), expr.span);
            self.assign(
                Place::local(fake),
                Rvalue::Ref(BorrowKind::Fake(FakeBorrowKind::Shallow), place),
                expr.span,
            );
            fake
        });
        (Place::local(temp), fake)
    }

    /// The left operand of a comparison of references compared as they are (`&str`, #10),
    /// when it is a place: copied into a temporary as any operand, with a shallow fake
    /// borrow of the place (`&fake shallow s`, see [`FakeBorrowKind::Shallow`]) whose
    /// temporary the caller mentions once the right operand ran. rustc passes `&s` to
    /// `PartialEq::eq`, so `s == { s = "b"; s }` is a conflict (decision 63).
    fn compared_ref_place(&mut self, op: BinOp, lhs: &Expr) -> Option<(Operand, Local)> {
        let ty = self.ty(lhs).clone();
        if !op.is_comparison()
            || !matches!(ty, Ty::Ref(Mutability::Not, _))
            || !is_place_expr(self.resolve, lhs)
            || self.has_pending_coercion(lhs)
            || self.place_mutability.is_mut()
        {
            return None;
        }
        let place = self.as_place(lhs);
        let temp = self.new_temp(ty.clone(), lhs.span);
        let value = self.place_operand(place.clone());
        self.assign(Place::local(temp), Rvalue::Use(value), lhs.span);
        let fake = self.new_temp(Ty::shared_ref(ty), lhs.span);
        self.assign(
            Place::local(fake),
            Rvalue::Ref(BorrowKind::Fake(FakeBorrowKind::Shallow), place),
            lhs.span,
        );
        Some((self.place_operand(Place::local(temp)), fake))
    }

    /// `p = e` and `p op= e`. The right-hand side is evaluated first, then the place, as
    /// in rustc.
    fn lower_assign(&mut self, expr: &Expr) {
        let span = self.span_of(expr);
        match &expr.kind {
            // The operands are in the assignment's scope, as in rustc: a panic in the place
            // drops them before the statement's temporaries.
            ExprKind::Assign(lhs, rhs) => self.in_operand_scope(Some(expr), span, |this| {
                this.lower_plain_assign(lhs, rhs, span);
            }),
            ExprKind::AssignOp(op, lhs, rhs) => {
                let rhs = self.as_operand(rhs);
                let place = self.as_place_mut(lhs, Mutability::Mut);
                let current = self.place_operand(place.clone());
                self.assign(place, Rvalue::BinaryOp(*op, current, rhs), span);
            }
            _ => unreachable!("not an assignment"),
        }
    }

    /// `lhs = rhs`.
    fn lower_plain_assign(&mut self, lhs: &Expr, rhs: &Expr, span: Span) {
        let target = self.ty(lhs).clone();
        // An array, repeat or tuple is built in the place after the place is evaluated,
        // from operands evaluated before it, as rustc does: a panic in the place drops the
        // operands, each a temporary of its own, in reverse order. rustc builds a struct or
        // variant literal into a temporary first.
        let stripped = strip_parens(rhs);
        let rvalue = match stripped.kind {
            ExprKind::Array(_) | ExprKind::Repeat(..) | ExprKind::Tuple(_) => {
                self.aggregate_rvalue(stripped)
            }
            _ => None,
        };
        let rhs = rvalue.unwrap_or_else(|| Rvalue::Use(self.as_operand(rhs)));
        let place = self.as_place_mut(lhs, Mutability::Mut);
        if place.has_index() && rvalue_moves(&rhs) && self.typeck.items.needs_drop(&target) {
            // `a[i] = v`: the index is checked before `v` moves (rustc's bounds check
            // precedes the store), so a panic there still drops `v`.
            self.push_stmt(StatementKind::PlaceMention(place.clone()), span);
        }
        self.assign(place, rhs, span);
    }

    /// `(a, b) = rhs`, lowered as what rustc desugars it to, `{ let (x, y) = rhs; a = x;
    /// b = y; }`: every part of the value is moved (or copied) into a fresh local before
    /// any place is assigned, so `(t.1, t.0) = t` swaps; then each place is evaluated and
    /// assigned in turn, dropping its old value. The `let` is a statement of its own: its
    /// temporaries (what `_` left of an rvalue) die before the first assignment.
    fn lower_destructuring_assign(&mut self, pat: &Pat, rhs: &Expr, span: Span) {
        self.push_scope();
        let saved = self.push_temp_scope();
        if let PatKind::Wild = pat.kind {
            // `_ = e;` is `let _ = e;`: a place is only mentioned.
            if is_place_expr(self.resolve, rhs) {
                let place = self.as_place(rhs);
                self.push_stmt(StatementKind::PlaceMention(place), rhs.span);
            } else {
                self.as_temp(rhs);
            }
        } else {
            self.lower_let_tuple(pat, Some(rhs));
        }
        self.close_temp_scope(saved, span);
        let mut assignees = Vec::new();
        pat.walk_assignees(&mut |p, place| assignees.push((p, place)));
        for (p, target) in assignees {
            // The resolver gives every assignee a local: one naming a unit struct or a
            // variant compares, and is a path pattern by now.
            let id = self
                .resolve
                .local_of_pat(p.id)
                .expect("an assignee pattern has a local");
            let saved = self.push_temp_scope();
            self.in_operand_scope(None, target.span, |this| {
                this.assign_part(p.id, id, target);
            });
            self.close_temp_scope(saved, target.span);
        }
        self.pop_scope(span);
    }

    /// `target = part`, one assignment of a destructuring assignment, where `part` is the
    /// local of the assignee pattern `assignee`. As in rustc, a part with drop glue is moved into
    /// an operand temporary of the assignment before the place is evaluated, so a panic in
    /// the place (`(a[9], b) = ..`) drops it first, then the parts not yet assigned.
    fn assign_part(&mut self, assignee: NodeId, part: LocalId, target: &Expr) {
        let local = self.mir_local(part);
        let mut value = self.place_operand(Place::local(local));
        let ty = self.body.local_decl(local).ty.clone();
        if self.typeck.items.needs_drop(&ty) {
            let temp = self.new_operand_temp(ty, target.span);
            self.assign(Place::local(temp), Rvalue::Use(value), target.span);
            value = self.place_operand(Place::local(temp));
        }
        if let Some(ty) = self.types.assignee_coercion(assignee).cloned() {
            let temp = self.new_temp(ty.clone(), target.span);
            self.assign(
                Place::local(temp),
                Rvalue::PointerCoercion(value, ty),
                target.span,
            );
            value = self.place_operand(Place::local(temp));
        }
        let target_ty = self.ty(target).clone();
        let place = self.as_place_mut(target, Mutability::Mut);
        if place.has_index() && self.typeck.items.needs_drop(&target_ty) {
            // `(a[i], ..) = ..`: the index is checked before the value moves, as for
            // `a[i] = v`.
            self.push_stmt(StatementKind::PlaceMention(place.clone()), target.span);
        }
        self.assign(place, Rvalue::Use(value), target.span);
    }

    /// Branches on the condition of an `if` or `while` or on a match guard as rustc's
    /// `then_else_break` does (decision 73): `&&`, `||` and `!` become control flow
    /// rather than a `bool` temporary, each other operand is evaluated in a temporary scope
    /// of its own and switched on. Returns the blocks reached when the condition holds and
    /// when it does not, both left open, with the storage of the last operand's
    /// temporaries ended at their start.
    fn branch_on(&mut self, cond: &Expr, span: Span) -> (BasicBlock, BasicBlock) {
        match &strip_parens(cond).kind {
            ExprKind::Binary(op @ (BinOp::And | BinOp::Or), lhs, rhs) => {
                let (lhs_true, lhs_false) = self.branch_on(lhs, span);
                let (decided, rest) = if *op == BinOp::And {
                    (lhs_false, lhs_true)
                } else {
                    (lhs_true, lhs_false)
                };
                self.enter(rest);
                let (rhs_true, rhs_false) = self.branch_on(rhs, span);
                let join = self.new_block();
                let other = if *op == BinOp::And {
                    rhs_false
                } else {
                    rhs_true
                };
                // Entered (again) before they end: an inner join was never entered, and every
                // block takes the cleanup scopes open where it is entered.
                for bb in [decided, other] {
                    self.enter(bb);
                    self.terminate(TerminatorKind::Goto { target: join }, span);
                }
                if *op == BinOp::And {
                    (rhs_true, join)
                } else {
                    (join, rhs_false)
                }
            }
            ExprKind::Unary(UnOp::Not, inner) if self.ty(inner).is_bool() => {
                let (holds, fails) = self.branch_on(inner, span);
                (fails, holds)
            }
            _ => {
                let (discr, temps) = self.condition(cond, span);
                let holds = self.new_block();
                let fails = self.new_block();
                self.switch_int(discr, vec![(0, fails)], holds, span);
                self.enter(holds);
                self.storage_dead(&temps, span);
                self.enter(fails);
                self.storage_dead(&temps, span);
                (holds, fails)
            }
        }
    }

    /// `a && b` / `a || b`: the right operand only runs when the left one does not decide
    /// the result.
    fn lower_logical(&mut self, dest: &Place, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) {
        // Each operand is a temporary scope of its own. The left one is branched on as a
        // condition (rustc's `then_else_break`), so `a && b || c` does not first collect
        // `a && b` in a temporary.
        let (lhs_true, lhs_false) = self.branch_on(lhs, span);
        let (rhs_block, short_block, short_value) = match op {
            // `false && _` is false; `true || _` is true.
            BinOp::And => (lhs_true, lhs_false, false),
            BinOp::Or => (lhs_false, lhs_true, true),
            _ => unreachable!("not a logical operator"),
        };
        self.enter(rhs_block);
        let saved = self.push_temp_scope();
        let rhs = self.as_operand(rhs);
        self.assign(dest.clone(), Rvalue::Use(rhs), span);
        self.close_temp_scope(saved, span);
        let rhs_end = self.current;
        self.enter(short_block);
        self.assign(
            dest.clone(),
            Rvalue::Use(Self::bool_constant(short_value, span)),
            span,
        );
        let short_end = self.current;
        let join = self.new_block();
        self.terminate_block(rhs_end, TerminatorKind::Goto { target: join }, span);
        self.terminate_block(short_end, TerminatorKind::Goto { target: join }, span);
        self.enter(join);
    }

    /// `if cond { then } else { els }`. `bool` is `0`/`1`, so `0` selects the else
    /// branch. Without `else`, the value is `()`.
    fn lower_if(
        &mut self,
        dest: Option<ValueDest>,
        cond: &Expr,
        then: &Block,
        els: Option<&Expr>,
        span: Span,
    ) {
        if let Some(operands) = cond.let_chain() {
            self.lower_if_chain(dest, &operands, then, els, span);
            return;
        }
        if let ExprKind::Let(pat, scrutinee) = &strip_parens(cond).kind {
            self.lower_if_let(dest, pat, scrutinee, then, els, span);
            return;
        }
        // The condition and each branch are temporary scopes of their own: a
        // condition's temporaries are dropped before the branch runs.
        let (then_block, else_block) = self.branch_on(cond, span);
        self.enter(then_block);
        self.in_temp_scope(then.span, |this| match dest {
            Some(dest) => this.lower_block_into(dest.place, then, dest.extending),
            None => this.lower_block_discard(then),
        });
        let then_end = self.current;
        self.enter(else_block);
        match (els, dest) {
            (Some(els), Some(dest)) => {
                self.in_temp_scope(els.span, |this| {
                    this.value_into(dest.place, els, dest.extending);
                });
            }
            (Some(els), None) => self.in_temp_scope(els.span, |this| this.lower_expr_discard(els)),
            (None, Some(dest)) => {
                self.assign(dest.place.clone(), Self::unit_rvalue(span), span);
            }
            (None, None) => {}
        }
        let else_end = self.current;
        let join = self.new_block();
        self.terminate_block(then_end, TerminatorKind::Goto { target: join }, span);
        self.terminate_block(else_end, TerminatorKind::Goto { target: join }, span);
        self.enter(join);
    }

    /// `while cond { body }`. The condition's temporaries die at the start of both
    /// successors, so each iteration starts from the same storage state.
    fn lower_while(&mut self, cond: &Expr, body: &Block, span: Span) {
        if let Some(operands) = cond.let_chain() {
            self.lower_while_chain(&operands, body, span);
            return;
        }
        if let ExprKind::Let(pat, scrutinee) = &strip_parens(cond).kind {
            self.lower_while_let(pat, scrutinee, body, span);
            return;
        }
        let header = self.new_block();
        self.terminate(TerminatorKind::Goto { target: header }, span);
        self.enter(header);
        let (body_block, false_block) = self.branch_on(cond, span);
        self.loops.push(LoopScope {
            header,
            exit: None,
            dest: None,
            scope_depth: self.scopes.len(),
        });
        self.enter(body_block);
        self.in_temp_scope(body.span, |this| this.lower_block_discard(body));
        self.terminate(TerminatorKind::Goto { target: header }, span);
        let scope = self.loops.pop().expect("loop scope pushed above");
        self.enter(false_block);
        let exit = scope.exit.unwrap_or_else(|| self.new_block());
        self.terminate(TerminatorKind::Goto { target: exit }, span);
        self.enter(exit);
    }

    /// `loop { body }`. Only `break` leaves it; without one nothing after the loop is
    /// reachable.
    fn lower_loop(&mut self, dest: Option<Place>, body: &Block, span: Span) {
        let header = self.new_block();
        self.terminate(TerminatorKind::Goto { target: header }, span);
        self.enter(header);
        self.loops.push(LoopScope {
            header,
            exit: None,
            dest,
            scope_depth: self.scopes.len(),
        });
        self.in_temp_scope(body.span, |this| this.lower_block_discard(body));
        self.terminate(TerminatorKind::Goto { target: header }, span);
        let scope = self.loops.pop().expect("loop scope pushed above");
        let exit = scope.exit.unwrap_or_else(|| self.new_block());
        self.enter(exit);
    }

    fn lower_break(&mut self, value: Option<&Expr>, span: Span) {
        let depth = self.loops.len() - 1;
        let dest = self.loops[depth].dest.clone();
        match (value, dest) {
            (Some(value), Some(dest)) => self.expr_into_dest(&dest, value),
            (Some(value), None) => self.lower_expr_discard(value),
            (None, Some(dest)) => {
                if dest.ty(&self.body).is_unit() {
                    self.assign(dest, Self::unit_rvalue(span), span);
                }
            }
            (None, None) => {}
        }
        let exit = if let Some(bb) = self.loops[depth].exit {
            bb
        } else {
            let bb = self.new_block();
            self.loops[depth].exit = Some(bb);
            bb
        };
        self.exit_to(self.loops[depth].scope_depth, exit, span);
    }

    fn lower_continue(&mut self, span: Span) {
        let scope = self.loops.last().expect("`continue` inside a loop");
        let (header, depth) = (scope.header, scope.scope_depth);
        self.exit_to(depth, header, span);
    }

    /// `e?` into `dest` (decision 93), as rustc's `match Try::branch(e) { Continue(v) =>
    /// v, Break(r) => return FromResidual::from_residual(r) }` behaves: `e` is evaluated
    /// into a temporary of the statement, its discriminant is tested, and either the
    /// `Some`/`Ok` value moves into `dest`, or the function returns `None`/`Err(err)` (the
    /// error moved out of the temporary; the type checker allows only an error of the
    /// function's own type, so `From::from` is the identity), dropping every live local
    /// on the way as `return` does.
    fn lower_try(&mut self, dest: &Place, expr: &Expr, operand: &Expr) {
        let span = self.span_of(expr);
        let question = Span::new(span.file, operand.span.end, span.end);
        let kind = self.types.try_exprs[&expr.id];
        let temp = self.as_temp(operand);
        let Ty::Adt(adt, args) = self.body.local_decl(temp).ty.clone() else {
            unreachable!("the operand of `?` is an `Option` or a `Result`")
        };
        let (continue_index, break_index) = self.resolve.try_variants(kind);
        let discr = self.new_temp(Ty::Prim(PrimTy::Isize), question);
        self.assign(
            Place::local(discr),
            Rvalue::Discriminant(Place::local(temp)),
            question,
        );
        let value_bb = self.new_block();
        let break_bb = self.new_block();
        #[allow(clippy::cast_sign_loss)]
        let continue_discr = self.typeck.items.adt(adt).variant(continue_index).discr as u128;
        self.switch_int(
            Operand::Copy(Place::local(discr)),
            vec![(continue_discr, value_bb)],
            break_bb,
            question,
        );
        // The early return: `None`, or `Err` with the error, at the function's own type.
        self.enter(break_bb);
        let ret = Place::local(Local::RETURN);
        let Ty::Adt(ret_adt, ret_args) = ret.ty(&self.body) else {
            unreachable!("a function using `?` returns an `Option` or a `Result`")
        };
        let mut operands = Vec::new();
        if kind == CoreItem::Result {
            let err_ty = self
                .typeck
                .items
                .field_tys(&Ty::Adt(adt, args), Some(break_index))
                .swap_remove(0);
            let err = Place::local(temp).downcast(break_index).field(0, err_ty);
            operands.push(self.place_operand(err));
        }
        self.assign(
            ret,
            Rvalue::Aggregate(
                AggregateKind::Variant(ret_adt, break_index, ret_args),
                operands,
            ),
            question,
        );
        let target = self.return_block();
        self.leave_to(0, target, question);
        // The value.
        self.enter(value_bb);
        let value_ty = self.ty(expr).clone();
        let value = Place::local(temp)
            .downcast(continue_index)
            .field(0, value_ty);
        let op = self.place_operand(value);
        self.assign(dest.clone(), Rvalue::Use(op), span);
    }

    fn lower_return(&mut self, value: Option<&Expr>, span: Span) {
        let ret = Place::local(Local::RETURN);
        match value {
            Some(value) => self.expr_into_dest(&ret, value),
            None => self.assign(ret, Self::unit_rvalue(span), span),
        }
        let target = self.return_block();
        self.exit_to(0, target, span);
    }

    // ----- blocks and statements -----

    /// Lowers a block, writing its tail into `dest`. A block without a tail yields `()`
    /// when the destination is a unit place; a diverging one yields nothing. The tail of
    /// a block in an extending position is in one too ([`Self::extending_into`]).
    fn lower_block_into(&mut self, dest: &Place, block: &Block, extending: bool) {
        self.push_scope();
        for stmt in &block.stmts {
            self.lower_stmt(stmt);
        }
        match &block.tail {
            Some(tail) => {
                self.in_tail_scope(tail, |this| this.value_into(dest, tail, extending));
            }
            None => {
                if dest.ty(&self.body).is_unit() {
                    self.assign(dest.clone(), Self::unit_rvalue(block.span), block.span);
                }
            }
        }
        self.pop_scope(block.span);
    }

    /// Lowers a block whose value is not needed.
    fn lower_block_discard(&mut self, block: &Block) {
        self.push_scope();
        for stmt in &block.stmts {
            self.lower_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.in_tail_scope(tail, |this| this.lower_expr_discard(tail));
        }
        self.pop_scope(block.span);
    }

    /// `let (a, (b, _)) = init;`. A place initialiser is destructured where it is, as in
    /// rustc: each binding copies or moves its own element out of it, and `_` touches
    /// nothing (`let (a, _) = t;` leaves `t.1` usable). Any other initialiser is
    /// evaluated into a temporary first; the referent of `&<rvalue>` lives as long as the
    /// bindings (temporary lifetime extension, as for `let r = &<rvalue>;`), and so does
    /// the initialiser's own temporary when the pattern has a `ref`/`ref mut` binding
    /// (rustc's extending patterns: `let S { ref mut a, .. } = F();` borrows a value that
    /// lives to the end of the block). Without an initialiser the bindings are only
    /// declared.
    fn lower_let_tuple(&mut self, pat: &Pat, init: Option<&Expr>) {
        let Some(init) = init else {
            self.declare_pattern_locals(pat);
            return;
        };
        let extending = is_extending_pattern(pat);
        // Extended temporaries go before the bindings, in the order they are made.
        let block_scope = self.temp_scope - 1;
        self.extend_at = Some((block_scope, self.scopes[block_scope].len()));
        let place = if is_place_expr(self.resolve, init) {
            let place = self.as_place_mut(init, Mutability::from_bool(matches::binds_ref_mut(pat)));
            if place.has_index() {
                // `let ref x = a[i];`: the index is checked at the initialiser, as in
                // rustc, not where the pattern binds the first part of it.
                self.push_stmt(
                    StatementKind::PlaceMention(place.clone()),
                    self.span_of(init),
                );
            }
            place
        } else if let Some(temp) = self.extended_ref(init) {
            Place::local(temp)
        } else if matches!(strip_parens(init).kind, ExprKind::Tuple(_))
            || self.has_extended_borrow(init)
        {
            // `let (a, b) = (&F(), 1);`, `let (a, b) = { (&F(), 1) };`
            let ty = self.ty(init).clone();
            let temp = if extending {
                self.new_temp_in_block_scope(ty, init.span)
            } else {
                self.new_temp(ty, init.span)
            };
            self.extending_into(&Place::local(temp), init);
            self.temp_complete(temp);
            Place::local(temp)
        } else if extending {
            let ty = self.ty(init).clone();
            let temp = self.new_temp_in_block_scope(ty, init.span);
            if !writes_once(init) {
                self.open_pending(temp);
            }
            self.expr_into_dest(&Place::local(temp), init);
            self.temp_complete(temp);
            Place::local(temp)
        } else {
            Place::local(self.as_temp(init))
        };
        self.extend_at = None;
        if self.ty(init).is_uninhabited() {
            // `let (a, b) = return;`: nothing to bind; the code after it is unreachable.
            self.declare_pattern_locals(pat);
            return;
        }
        self.bind_pat(pat, place, &matches::Choice::new(), BindMode::Declare);
    }

    /// Lowers one statement. Temporaries it creates die when it ends.
    fn lower_stmt(&mut self, stmt: &Stmt) {
        let saved = self.push_temp_scope();
        // A `let` inside an initialiser (`let x = { let y = &F(); .. };`) has its own.
        let outer_extend_at = self.extend_at.take();
        match &stmt.kind {
            StmtKind::Let(l) if l.els.is_some() || has_or(&l.pat) => match &l.init {
                Some(init) => self.lower_let_else(&l.pat, init, l.els.as_deref()),
                None => self.declare_pattern_locals(&l.pat),
            },
            StmtKind::Let(l) => match &l.pat.kind {
                PatKind::Ident {
                    by_ref: false,
                    sub: None,
                    ..
                } if self.resolve.local_of_pat(l.pat.id).is_some() => {
                    let id = self
                        .resolve
                        .local_of_pat(l.pat.id)
                        .expect("identifier patterns introduce locals");
                    let block_scope = self.scopes.len() - 2;
                    let at = self.scopes[block_scope].len();
                    let local = self.declare_user_local(id, l.pat.span);
                    self.extend_at = Some((block_scope, at));
                    if l.init.as_ref().is_none_or(|init| !writes_once(init)) {
                        self.open_pending(local);
                    }
                    if let Some(init) = &l.init {
                        self.extending_into(&Place::local(local), init);
                    }
                    self.extend_at = None;
                }
                // `let _ = place;` only mentions the place, as in rustc (#37): no read, move
                // or initialisation check, a liveness use of its local (`let _ = p;` leaves
                // a non-Copy `p` usable). Any other initialiser is evaluated into a
                // temporary that dies with the statement.
                PatKind::Wild => {
                    if let Some(init) = &l.init {
                        if is_place_expr(self.resolve, init) {
                            let place = self.as_place(init);
                            self.push_stmt(StatementKind::PlaceMention(place), init.span);
                        } else {
                            self.as_temp(init);
                        }
                    }
                }
                _ => self.lower_let_tuple(&l.pat, l.init.as_ref()),
            },
            StmtKind::Expr { expr, .. } => self.lower_expr_discard(expr),
            StmtKind::Empty => {}
        }
        self.close_temp_scope(saved, stmt.span);
        self.extend_at = outer_extend_at;
    }
}

/// Whether a `let` pattern extends the temporary holding its initialiser to the end of the
/// enclosing block (rustc's *extending pattern*): a `ref`/`ref mut` binding, or a tuple,
/// tuple-struct, struct or or-pattern with an extending subpattern. A binding under a
/// default binding mode does not count, as in rustc (it borrows through a reference, not
/// from the initialiser's temporary).
fn is_extending_pattern(pat: &Pat) -> bool {
    match &pat.kind {
        PatKind::Ident { by_ref, .. } => *by_ref,
        PatKind::Tuple(elems) | PatKind::TupleStruct(_, elems) | PatKind::Or(elems) => {
            elems.iter().any(is_extending_pattern)
        }
        PatKind::Struct { fields, .. } => fields.iter().any(|f| is_extending_pattern(&f.pat)),
        PatKind::Wild
        | PatKind::Rest
        | PatKind::Path(_)
        | PatKind::Lit(_)
        | PatKind::Range(..)
        // `let &(ref a, _) = &t;` borrows through the reference, as in rustc.
        | PatKind::Ref { .. }
        | PatKind::Assignee(_) => false,
    }
}

/// Whether `expr` denotes a place (a local, a field of a place, a dereference) rather than
/// a value to evaluate.
fn is_place_expr(resolve: &ResolveResults, expr: &Expr) -> bool {
    match &strip_parens(expr).kind {
        ExprKind::Path(_) => matches!(resolve.res(strip_parens(expr).id), Res::Local(_)),
        ExprKind::Field(..) | ExprKind::Deref(_) | ExprKind::Index(..) => true,
        _ => false,
    }
}

/// The span after the parameters' end, for the storage markers of their bindings.
fn param_span(f: &FnDecl) -> Span {
    f.params.last().map_or(f.name.span, |p| p.span)
}

/// Whether lowering `expr` into a destination writes it exactly once, as its last step
/// (or never, when it diverges first): then the cleanup scope of a destination with drop
/// glue opens after that write. Branching expressions write it on each branch.
fn writes_once(expr: &Expr) -> bool {
    match &strip_parens(expr).kind {
        ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::While { .. }
        | ExprKind::Loop { .. }
        | ExprKind::Break(_)
        | ExprKind::Continue
        | ExprKind::Return(_)
        | ExprKind::Binary(BinOp::And | BinOp::Or, ..) => false,
        ExprKind::Block(block) => block.tail.as_deref().is_none_or(writes_once),
        _ => true,
    }
}

fn strip_parens(mut e: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    e
}

/// Whether `rvalue` moves any of its operands.
fn rvalue_moves(rvalue: &Rvalue) -> bool {
    match rvalue {
        Rvalue::Use(op) | Rvalue::Repeat(op, _) => matches!(op, Operand::Move(_)),
        Rvalue::Aggregate(_, ops) => ops.iter().any(|op| matches!(op, Operand::Move(_))),
        _ => false,
    }
}
