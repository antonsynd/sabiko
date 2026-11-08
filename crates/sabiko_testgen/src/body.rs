//! Function bodies: statements and expressions generated against a [`Program`] skeleton.
//!
//! The generator keeps a model of the scope precise enough to produce programs the borrow
//! checker accepts: which locals are moved, which are borrowed by a live reference local
//! (a *region*: the few statements after `let r = &x;` during which `r` is used and `x`
//! is locked), and the loop depth each local was declared at (a non-`Copy` local declared
//! outside a loop is never moved inside it). Every check against that model can be
//! skipped with the `violation_percent` knob, which is how the default knobs produce the
//! occasional use-after-move or conflicting borrow for the verdict comparison; with
//! `borrow_clean()` the checks are never skipped.
//!
//! Enums and pattern matching live in the `matches` module, arrays, slices, indexing and
//! range indexing in the `arrays` module, strings in the `strings` module, references to
//! references in the `refs` module.

mod arrays;
mod matches;
mod refs;
mod strings;

use arrays::{has_range, Ranges};

use std::fmt::Write as _;

use crate::program::{
    float_literal, int_literal, random_literal, random_prim, unique_name, FnSig, Program, Receiver,
    RetTy, LOCAL_NAMES,
};
use crate::rng::Rng;
use crate::ty::{IntTy, Prim, Ty, ValTy};
use crate::Knobs;

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // four independent flags of the scope model
struct Var {
    name: String,
    ty: Ty,
    mutable: bool,
    moved: bool,
    /// Loop nesting depth at declaration.
    loop_depth: usize,
    /// Index of the block that declared it.
    block: usize,
    shared_locks: u32,
    mut_locks: u32,
    /// Loop counters are never touched by generated statements.
    hidden: bool,
    /// A reference local whose region ended: never used again.
    dead: bool,
    /// What a reference local borrows: `(variable, mutable loan)`.
    borrows: Vec<(usize, bool)>,
    /// A reference local dies when its block's statement counter reaches this.
    expires: Option<usize>,
}

#[derive(Debug)]
struct Block {
    stmt_index: usize,
    first_var: usize,
}

#[derive(Debug, Clone, Copy)]
enum LoopKind {
    Stmt,
    Value(ValTy),
    /// `while i < s.len() { ..; i += 1usize; }`: a `continue` would skip the increment.
    Index,
}

/// What a statement wants to do with a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// A by-value use: a copy, or a move for a non-`Copy` type.
    Read,
    Write,
    Borrow(bool),
}

/// A place expression and where it lives.
#[derive(Debug, Clone)]
struct Place {
    text: String,
    ty: ValTy,
    root: usize,
    /// `Some(mutable)` when the place is reached through a reference local.
    via_ref: Option<bool>,
    /// The whole root variable (or the whole referent), not a field of it.
    whole: bool,
    /// Inside a struct with `impl Drop` or an array or slice element, so never moved
    /// (E0509, E0508).
    no_move: bool,
    /// The lengths of the arrays (`None`: slices) its index segments index, in order.
    lens: Vec<Option<usize>>,
}

/// A generated expression and the variables a reference-typed result borrows.
#[derive(Debug, Clone)]
struct Expr {
    text: String,
    roots: Vec<(usize, bool)>,
}

impl Expr {
    fn value(text: String) -> Self {
        Expr {
            text,
            roots: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct BodyGen<'a> {
    prog: &'a Program,
    knobs: &'a Knobs,
    rng: &'a mut Rng,
    fn_index: usize,
    sig: &'a FnSig,
    vars: Vec<Var>,
    blocks: Vec<Block>,
    loops: Vec<LoopKind>,
    lines: Vec<String>,
    indent: usize,
    used_names: Vec<String>,
    budget: usize,
    /// While a call's value arguments are generated: the variable its receiver (or a
    /// reborrowed `&mut` argument) borrows, which places then prefer, so that the shapes
    /// two-phase borrows allow (`v.Add(v.Get())`, `Use(m, m.X)`) come up.
    prefer_root: Option<usize>,
    /// Set by `stmt_method_call`: the next call of a method uses method syntax on a place.
    force_method: bool,
    /// While positive, no check is skipped: code that may be unreachable (a match arm after
    /// the first, an `else` after a pattern that may be irrefutable) holds no violation.
    no_violations: u32,
    /// While positive, index expressions are leaves (no calls): in guards.
    simple: u32,
    /// While positive, the bounds or indices of a range place are being generated: no
    /// further range goes into them (which keeps the recursion shallow).
    range_depth: u32,
    /// While positive, nothing is moved: the value arguments of a call whose reference
    /// arguments (generated first, evaluated later) may read what they would move.
    no_moves: u32,
}

const INDENT: &str = "    ";

impl<'a> BodyGen<'a> {
    pub fn new(
        prog: &'a Program,
        knobs: &'a Knobs,
        rng: &'a mut Rng,
        fn_index: usize,
        budget: usize,
    ) -> Self {
        let sig = &prog.fns[fn_index];
        let mut gen = BodyGen {
            prog,
            knobs,
            rng,
            fn_index,
            sig,
            vars: Vec::new(),
            blocks: vec![Block {
                stmt_index: 0,
                first_var: 0,
            }],
            loops: Vec::new(),
            lines: Vec::new(),
            indent: 1,
            used_names: Vec::new(),
            budget,
            prefer_root: None,
            force_method: false,
            no_violations: 0,
            simple: 0,
            range_depth: 0,
            no_moves: 0,
        };
        for p in &sig.params {
            gen.used_names.push(p.name.clone());
            gen.vars.push(Var {
                name: p.name.clone(),
                ty: p.ty,
                mutable: p.mutable,
                moved: false,
                loop_depth: 0,
                block: 0,
                shared_locks: 0,
                mut_locks: 0,
                hidden: false,
                dead: false,
                borrows: Vec::new(),
                expires: None,
            });
        }
        gen
    }

    /// Generates the body and returns its lines (without the signature and closing brace).
    pub fn generate(mut self) -> Vec<String> {
        let n = self.rng.range(2, 5);
        for _ in 0..n {
            if self.lines.len() >= self.budget {
                break;
            }
            self.stmt();
        }
        self.expire_refs();
        self.final_prints();
        self.kill_all_refs();
        match self.sig.ret {
            RetTy::Unit => {}
            RetTy::Val(v) => {
                let e = self.val_expr(v, self.knobs.max_expr_depth);
                self.line(&e.text);
            }
            RetTy::Ref { mutable, inner } => {
                let text = self
                    .ref_tail(mutable, inner)
                    .expect("ref parameter present");
                self.line(&text);
            }
        }
        self.lines
    }

    // ----- output ------------------------------------------------------------------

    fn line(&mut self, text: &str) {
        let mut l = INDENT.repeat(self.indent);
        l.push_str(text);
        self.lines.push(l);
    }

    fn remaining(&self) -> usize {
        self.budget.saturating_sub(self.lines.len())
    }

    // ----- scope model -------------------------------------------------------------

    fn clean(&self) -> bool {
        self.knobs.violation_percent == 0
    }

    /// True when a check may be skipped: never in borrow-clean mode.
    fn violate(&mut self) -> bool {
        !self.clean()
            && self.no_violations == 0
            && self.rng.chance(u32::from(self.knobs.violation_percent))
    }

    /// Runs `f` with every check enforced (see `no_violations`).
    fn strictly<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.no_violations += 1;
        let out = f(self);
        self.no_violations -= 1;
        out
    }

    /// Runs `f` with index expressions made of leaves (see `simple`): for a place that is
    /// written more than once or evaluated repeatedly, whose index must not move anything.
    fn simply<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.simple += 1;
        let out = f(self);
        self.simple -= 1;
        out
    }

    fn fresh_name(&mut self) -> String {
        unique_name(self.rng, LOCAL_NAMES, &mut self.used_names)
    }

    fn loop_depth(&self) -> usize {
        self.loops.len()
    }

    fn cur_block(&self) -> usize {
        self.blocks.len() - 1
    }

    fn declare(&mut self, name: String, ty: Ty, mutable: bool, hidden: bool) -> usize {
        self.vars.push(Var {
            name,
            ty,
            mutable,
            moved: false,
            loop_depth: self.loop_depth(),
            block: self.cur_block(),
            shared_locks: 0,
            mut_locks: 0,
            hidden,
            dead: false,
            borrows: Vec::new(),
            expires: None,
        });
        self.vars.len() - 1
    }

    fn lock(&mut self, roots: &[(usize, bool)]) {
        for &(v, mutable) in roots {
            if mutable {
                self.vars[v].mut_locks += 1;
            } else {
                self.vars[v].shared_locks += 1;
            }
        }
    }

    fn unlock(&mut self, roots: &[(usize, bool)]) {
        for &(v, mutable) in roots {
            if mutable {
                self.vars[v].mut_locks = self.vars[v].mut_locks.saturating_sub(1);
            } else {
                self.vars[v].shared_locks = self.vars[v].shared_locks.saturating_sub(1);
            }
        }
    }

    fn kill(&mut self, v: usize) {
        if self.vars[v].dead {
            return;
        }
        self.vars[v].dead = true;
        let borrows = std::mem::take(&mut self.vars[v].borrows);
        self.unlock(&borrows);
    }

    /// Ends the regions of the reference locals of the current block whose time is up.
    fn expire_refs(&mut self) {
        let block = self.cur_block();
        let index = self.blocks[block].stmt_index;
        for v in 0..self.vars.len() {
            if self.vars[v].block == block && self.vars[v].expires.is_some_and(|e| index >= e) {
                self.kill(v);
            }
        }
    }

    /// Ends every region: used before a tail expression, after which nothing is live.
    fn kill_all_refs(&mut self) {
        for v in 0..self.vars.len() {
            if self.vars[v].expires.is_some() {
                self.kill(v);
            }
        }
    }

    /// Binds a reference local to `expr` and opens its region.
    fn bind_ref(&mut self, name: String, ty: Ty, expr: &Expr) -> usize {
        let v = self.declare(name, ty, false, false);
        let span = if self.clean() {
            self.rng.range(1, 3)
        } else {
            self.rng.range(1, 6)
        };
        let block = self.cur_block();
        let mut expires = self.blocks[block].stmt_index + usize::try_from(span).expect("small") + 1;
        for &(root, _) in &expr.roots {
            // A reference derived from another reference local must not outlive it.
            if self.vars[root].block == block {
                if let Some(e) = self.vars[root].expires {
                    expires = expires.min(e);
                }
            }
        }
        self.vars[v].expires = Some(expires);
        self.vars[v].borrows.clone_from(&expr.roots);
        self.lock(&expr.roots);
        v
    }

    fn open_block(&mut self) {
        self.blocks.push(Block {
            stmt_index: 0,
            first_var: self.vars.len(),
        });
        self.indent += 1;
    }

    fn close_block(&mut self) {
        let block = self.blocks.pop().expect("block");
        for v in block.first_var..self.vars.len() {
            self.kill(v);
        }
        self.vars.truncate(block.first_var);
        self.indent -= 1;
    }

    // ----- places --------------------------------------------------------------------

    fn candidates(&self) -> Vec<Place> {
        let mut out = Vec::new();
        for (i, var) in self.vars.iter().enumerate() {
            if var.hidden || var.dead {
                continue;
            }
            match var.ty {
                Ty::Val(v) => {
                    for (path, ty) in self.prog.sub_places(v) {
                        out.push(Place {
                            text: format!("{}{path}", var.name),
                            ty,
                            root: i,
                            via_ref: None,
                            whole: path.is_empty(),
                            no_move: self.prog.moves_out_of_drop(v, &path),
                            lens: self.prog.path_lens(v, &path),
                        });
                    }
                }
                Ty::Ref { mutable, inner } => {
                    for (path, ty) in self.prog.sub_places(inner) {
                        let text = if path.is_empty() {
                            format!("*{}", var.name)
                        } else {
                            format!("{}{path}", var.name)
                        };
                        out.push(Place {
                            text,
                            ty,
                            root: i,
                            via_ref: Some(mutable),
                            whole: path.is_empty(),
                            no_move: self.prog.moves_out_of_drop(inner, &path),
                            lens: self.prog.path_lens(inner, &path),
                        });
                    }
                }
                Ty::RefRef {
                    outer,
                    mutable,
                    inner,
                } => {
                    // Writable only through `&mut &mut`. A part is reached by auto-deref
                    // (`rr.X`, `rr[0]`) or through one or two explicit dereferences
                    // (`(*rr).X`, `(**rr).X`), chosen by the place so that the text is
                    // stable between calls.
                    for (k, (path, ty)) in self.prog.sub_places(inner).into_iter().enumerate() {
                        let text = if path.is_empty() {
                            format!("**{}", var.name)
                        } else {
                            match (i + k) % 4 {
                                0 => format!("(*{}){path}", var.name),
                                1 => format!("(**{}){path}", var.name),
                                _ => format!("{}{path}", var.name),
                            }
                        };
                        out.push(Place {
                            text,
                            ty,
                            root: i,
                            via_ref: Some(outer && mutable),
                            whole: path.is_empty(),
                            no_move: self.prog.moves_out_of_drop(inner, &path),
                            lens: self.prog.path_lens(inner, &path),
                        });
                    }
                }
            }
        }
        out
    }

    /// The strict rules: whether `access` to `place` is accepted by the borrow checker
    /// according to the scope model.
    fn permitted(&self, place: &Place, access: Access) -> bool {
        if !self.usable(place, access) {
            return false;
        }
        let root = &self.vars[place.root];
        let copy = self.prog.is_copy(Ty::Val(place.ty));
        let writable = match place.via_ref {
            Some(mutable) => mutable,
            None => root.mutable,
        };
        if root.moved {
            // Only a whole re-initialisation of a `mut` local is allowed.
            return access == Access::Write && place.whole && place.via_ref.is_none() && writable;
        }
        match access {
            Access::Read => {
                if root.mut_locks > 0 {
                    return false;
                }
                if copy {
                    return true;
                }
                // A move.
                self.no_moves == 0
                    && place.via_ref.is_none()
                    && !place.no_move
                    && root.shared_locks == 0
                    && root.loop_depth == self.loop_depth()
            }
            Access::Write | Access::Borrow(true) => {
                root.mut_locks == 0 && root.shared_locks == 0 && writable
            }
            Access::Borrow(false) => root.mut_locks == 0,
        }
    }

    /// Whether `access` to `place` type-checks at all: an unsized place (a slice, `*s`) is
    /// only borrowed, never read or written as a whole. A place that is not usable is in
    /// neither pool of `choose_pool`, as a violation must be a borrow error.
    fn usable(&self, place: &Place, access: Access) -> bool {
        matches!(access, Access::Borrow(_)) || self.prog.is_sized(place.ty)
    }

    /// Records the effect of an access: a move, or a re-initialisation.
    fn apply(&mut self, place: &Place, access: Access) {
        let copy = self.prog.is_copy(Ty::Val(place.ty));
        match access {
            Access::Read if !copy => self.vars[place.root].moved = true,
            Access::Write if place.whole && place.via_ref.is_none() => {
                self.vars[place.root].moved = false;
            }
            _ => {}
        }
    }

    /// Picks the permitted pool, or (with the violation knob, or when nothing is permitted
    /// and a violation is rolled) the forbidden one. `None` when nothing may be used.
    fn choose_pool<T>(&mut self, ok: Vec<T>, bad: Vec<T>) -> Option<Vec<T>> {
        if ok.is_empty() {
            if bad.is_empty() || !self.violate() {
                return None;
            }
            return Some(bad);
        }
        if !bad.is_empty() && self.violate() {
            return Some(bad);
        }
        Some(ok)
    }

    /// A random place of type `ty` that `access` is allowed on (or, with the violation
    /// knob, one it is not), with the access recorded.
    fn place_of(&mut self, ty: ValTy, access: Access) -> Option<Place> {
        self.place_where(|t| t == ty, access)
    }

    /// [`BodyGen::place_of`] for a place whose type satisfies `filter`. Its indices are
    /// finished ([`BodyGen::finish_place`]) before the access is recorded.
    fn place_where(&mut self, filter: impl Fn(ValTy) -> bool, access: Access) -> Option<Place> {
        let typed: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| filter(p.ty) && self.usable(p, access))
            .collect();
        let (ok, bad): (Vec<Place>, Vec<Place>) =
            typed.into_iter().partition(|p| self.permitted(p, access));
        let mut pool = self.choose_pool(ok, bad)?;
        if let Some(root) = self.prefer_root {
            if pool.iter().any(|p| p.root == root) && self.rng.chance(60) {
                pool.retain(|p| p.root == root);
            }
        }
        let mut place = self.rng.pick(&pool).clone();
        self.finish_place(&mut place, Ranges::of(access));
        self.apply(&place, access);
        Some(place)
    }

    /// A random place of any type with `access` allowed, chosen among `filter`ed types,
    /// its indices finished. The access is not recorded. The flag says whether the place
    /// was taken from the forbidden pool (a deliberate violation).
    fn any_place(
        &mut self,
        access: Access,
        filter: impl Fn(ValTy) -> bool,
    ) -> Option<(Place, bool)> {
        self.any_place_with(access, filter, Ranges::of(access))
    }

    /// [`BodyGen::any_place`] with the range indices made for `ranges` (a place borrowed
    /// shared now but written later is finished for [`Ranges::Mut`]).
    fn any_place_with(
        &mut self,
        access: Access,
        filter: impl Fn(ValTy) -> bool,
        ranges: Ranges,
    ) -> Option<(Place, bool)> {
        let (mut place, violation) = self.pick_place(access, filter)?;
        self.finish_place(&mut place, ranges);
        Some((place, violation))
    }

    /// [`BodyGen::any_place`] without finishing the place: for the left-hand side of an
    /// assignment, whose index expressions are evaluated after the right-hand side (and
    /// must see what it moved).
    fn pick_place(
        &mut self,
        access: Access,
        filter: impl Fn(ValTy) -> bool,
    ) -> Option<(Place, bool)> {
        let typed: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| filter(p.ty) && self.usable(p, access))
            .collect();
        let (ok, bad): (Vec<Place>, Vec<Place>) =
            typed.into_iter().partition(|p| self.permitted(p, access));
        let pool = self.choose_pool(ok, bad)?;
        let place = self.rng.pick(&pool).clone();
        let violation = !self.permitted(&place, access);
        Some((place, violation))
    }

    // ----- expressions -------------------------------------------------------------

    fn callables(&self, pred: impl Fn(RetTy) -> bool) -> Vec<usize> {
        (0..self.fn_index)
            .filter(|&i| pred(self.prog.fns[i].ret))
            .collect()
    }

    /// A call to function `callee`: `F(args)`, `Type::F(args)` for an associated function,
    /// or for a method also `receiver.F(args)`. Reference arguments (a method's `&self` or
    /// `&mut self` receiver included) are chosen first and their loans held while the
    /// value arguments are generated, which forbids `F(&mut x, x.A)` (an error in both
    /// languages) and, conservatively, `F(x.A, &mut x)` (fine in both).
    ///
    /// A two-phase borrow (#15) is only held as shared while the value arguments are
    /// generated, so they may read its place (`v.Add(v.Get())`, `Use(m, m.X)`) but not
    /// write, move or borrow it mutably: an autoref'd `&mut self` receiver and a bare
    /// `&mut` local passed where a `&mut` is expected (its implicit reborrow). An explicit
    /// `&mut x` argument, and a receiver returned by a call, keep the mutable lock. The
    /// other reference arguments are chosen while the two-phase loans are still held as
    /// mutable: a shared borrow of the place passed to the call would conflict when the
    /// call activates the borrow (`v.AddFrom(&v)`).
    fn call(&mut self, callee: usize, depth: u32) -> Option<Expr> {
        let prog = self.prog;
        let sig = &prog.fns[callee];
        let receiver = sig.owner.and_then(|o| o.receiver);
        let forced = std::mem::take(&mut self.force_method);
        let method_syntax = receiver.is_some() && (forced || self.rng.chance(75));
        let returns_ref = matches!(sig.ret, RetTy::Ref { .. });
        let mut args: Vec<Option<Expr>> = vec![None; sig.params.len()];
        let mut held: Vec<(usize, bool)> = Vec::new();
        let mut two_phase: Vec<usize> = Vec::new();
        let mut result_roots = Vec::new();
        let mut ok = true;
        for (i, p) in sig.params.iter().enumerate() {
            let found = match p.ty {
                Ty::Val(_) => continue,
                // Never a method's receiver; its loans are ordinary ones.
                Ty::RefRef {
                    outer,
                    mutable,
                    inner,
                } => self.ref_ref_expr(outer, mutable, inner).map(|e| (e, false)),
                Ty::Ref { mutable, inner } if i == 0 && method_syntax => {
                    let temporary = !returns_ref && !forced;
                    self.receiver_ref(mutable, inner, depth.saturating_sub(1), temporary)
                }
                Ty::Ref { mutable, inner } => self.ref_arg(mutable, inner, depth),
            };
            let Some((e, is_two_phase)) = found else {
                ok = false;
                break;
            };
            self.lock(&e.roots);
            held.extend(e.roots.iter().copied());
            // A `&self`/`&mut self` method's result borrows `self` only.
            if i == 0 || !matches!(receiver, Some(Receiver::Ref { .. })) {
                result_roots.extend(e.roots.iter().copied());
            }
            if is_two_phase {
                two_phase.extend(e.roots.iter().map(|&(root, _)| root));
            }
            args[i] = Some(e);
        }
        if ok {
            for &root in &two_phase {
                self.unlock(&[(root, true)]);
                self.lock(&[(root, false)]);
                if let Some(h) = held.iter_mut().find(|h| **h == (root, true)) {
                    h.1 = false;
                }
            }
            let receiver_root = args
                .first()
                .and_then(Option::as_ref)
                .filter(|_| method_syntax)
                .and_then(|e| e.roots.first().map(|&(root, _)| root));
            let saved = self.prefer_root;
            self.prefer_root = two_phase.first().copied().or(receiver_root).or(saved);
            // The reference arguments were generated first but are evaluated in order: one
            // after a value argument may read (in an index or a range bound) what the value
            // argument would move (`F(d, &s[d.K..])`, E0382), so then nothing is moved.
            let ref_after_value = sig
                .params
                .iter()
                .enumerate()
                .any(|(j, p)| p.ty.is_ref() && sig.params[..j].iter().any(|q| !q.ty.is_ref()));
            self.no_moves += u32::from(ref_after_value);
            for (i, p) in sig.params.iter().enumerate() {
                if let Ty::Val(v) = p.ty {
                    let mut e = self.typed_expr(v, depth.saturating_sub(1));
                    if i == 0 && method_syntax {
                        e.text = receiver_text(&e.text);
                    }
                    args[i] = Some(e);
                }
            }
            self.no_moves -= u32::from(ref_after_value);
            self.prefer_root = saved;
        }
        self.unlock(&held);
        if !ok {
            return None;
        }
        let mut texts: Vec<String> = args.into_iter().map(|a| a.expect("filled").text).collect();
        let roots = if returns_ref {
            result_roots
        } else {
            Vec::new()
        };
        let text = match sig.owner {
            Some(_) if method_syntax => {
                let recv = texts.remove(0);
                format!("{recv}.{}({})", sig.name, texts.join(", "))
            }
            Some(owner) => format!(
                "{}::{}({})",
                prog.val_name(owner.ty),
                sig.name,
                texts.join(", ")
            ),
            None => format!("{}({})", sig.name, texts.join(", ")),
        };
        Some(Expr { text, roots })
    }

    /// A reference argument for a `&inner` / `&mut inner` parameter, with whether its
    /// borrow is two-phase: a bare `&mut` local of the parameter's type is reborrowed
    /// implicitly, two-phase; one coerced from `&mut [T; N]` to `&mut [T]` is not (rustc's
    /// E0503 for `F(m, m[0])`).
    fn ref_arg(&mut self, mutable: bool, inner: ValTy, depth: u32) -> Option<(Expr, bool)> {
        let e = self.ref_expr(mutable, inner, depth.saturating_sub(1))?;
        let bare = matches!(e.roots[..], [(root, true)]
            if e.text == self.vars[root].name
                && self.vars[root].ty == Ty::Ref { mutable: true, inner });
        Some((e, mutable && bare))
    }

    /// The receiver of a `&self` (or, `mutable`, `&mut self`) method call on a value of
    /// type `inner`, as written before `.Name(..)`, with whether its borrow is two-phase:
    /// a place, autoref'd (`p.X`, `(*r)`); a reference local, reborrowed (`r`); a call
    /// returning a reference (`p.Left()`); or, when the method's result does not borrow
    /// it (`temporary`), a struct literal or value call as a temporary.
    fn receiver_ref(
        &mut self,
        mutable: bool,
        inner: ValTy,
        depth: u32,
        temporary: bool,
    ) -> Option<(Expr, bool)> {
        let fns = self.callables(
            |r| matches!(r, RetTy::Ref { mutable: m, inner: t } if t == inner && (m || !mutable)),
        );
        let first = self.rng.weighted(&[
            50,
            25,
            if fns.is_empty() || depth == 0 { 0 } else { 25 },
            if temporary && matches!(inner, ValTy::Struct(_) | ValTy::Enum(_)) {
                8
            } else {
                0
            },
        ]);
        for kind in (0..4).map(|k| (first + k) % 4) {
            let found = match kind {
                0 => self.place_of(inner, Access::Borrow(mutable)).map(|place| {
                    let e = Expr {
                        text: receiver_text(&place.text),
                        roots: vec![(place.root, mutable)],
                    };
                    // An element of a range place (`a[1..][0].Bump(..)`) is reached
                    // through an `IndexMut` call that borrows `a` at once: no two-phase
                    // borrow (rustc's E0503 for `a[1..][0].Bump(a[0].X)`).
                    let two_phase = mutable && !has_range(&place.text);
                    (e, two_phase)
                }),
                1 => {
                    // A double reference local (`rr.Get()`, `m.Bump(..)`) now and then.
                    let double = if self.rng.chance(35) {
                        self.ref_ref_receiver(mutable, inner)
                    } else {
                        None
                    };
                    double
                        .or_else(|| self.ref_var(mutable, inner))
                        .map(|e| (e, mutable))
                }
                2 if !fns.is_empty() && depth > 0 => {
                    let callee = *self.rng.pick(&fns);
                    // The call's own loans are ordinary ones (`a.BMut().Add(a.N)`).
                    self.call(callee, depth).map(|e| (e, false))
                }
                3 if temporary => match inner {
                    ValTy::Struct(s) => {
                        let e = if self.rng.chance(60) {
                            self.struct_literal(s, depth)
                        } else {
                            self.value_call(inner, depth)
                                .unwrap_or_else(|| self.struct_literal(s, depth))
                        };
                        Some((Expr::value(format!("({})", e.text)), false))
                    }
                    ValTy::Enum(e) => {
                        let e = self.variant_literal(e, depth);
                        Some((Expr::value(format!("({})", e.text)), false))
                    }
                    _ => None,
                },
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn val_expr(&mut self, ty: ValTy, depth: u32) -> Expr {
        if self.prog.generics && depth > 0 && self.rng.chance(8) {
            return self.generic_value(ty, depth - 1);
        }
        if !matches!(ty, ValTy::Prim(_)) && depth >= 2 && self.rng.chance(4) {
            return self.inline_match(ty, depth);
        }
        match ty {
            ValTy::Prim(p) => self.prim_expr(p, depth),
            ValTy::Struct(s) => self.struct_expr(s, depth),
            ValTy::Tuple(t) => self.tuple_expr(t, depth),
            ValTy::Enum(e) => self.enum_expr(e, depth),
            ValTy::Array(a) => self.array_expr(a, depth, false),
            ValTy::Str => unreachable!("`str` is unsized"),
        }
    }

    /// A value of type `ty` passed through a generic helper (#114): `GenId(e)` (now and
    /// then with a turbofish), or `GenPick(c, a, b)`, which evaluates both values like a
    /// tuple literal does. The helpers' type parameter is inferred from the context, as the
    /// wrapped expressions' literals are.
    fn generic_value(&mut self, ty: ValTy, depth: u32) -> Expr {
        if self.rng.chance(60) {
            let e = self.val_expr(ty, depth);
            let turbofish = if self.rng.chance(30) {
                format!("::<{}>", self.prog.val_name(ty))
            } else {
                String::new()
            };
            Expr {
                text: format!("GenId{turbofish}({})", e.text),
                roots: e.roots,
            }
        } else {
            let c = self.prim_expr(Prim::Bool, depth);
            let a = self.val_expr(ty, depth);
            let b = self.val_expr(ty, depth);
            let mut roots = a.roots;
            roots.extend(b.roots);
            Expr {
                text: format!("GenPick({}, {}, {})", c.text, a.text, b.text),
                roots,
            }
        }
    }

    fn literal(&mut self, p: Prim) -> Expr {
        Expr::value(random_literal(self.rng, p).0)
    }

    fn prim_leaf(&mut self, p: Prim) -> Expr {
        let prog = self.prog;
        let consts: Vec<&str> = prog
            .consts
            .iter()
            .filter(|c| c.ty == p)
            .map(|c| c.name.as_str())
            .collect();
        let lengths = p == Prim::Int(IntTy::Usize) && !prog.arrays.is_empty();
        match self.rng.weighted(&[
            35,
            45,
            if consts.is_empty() { 0 } else { 20 },
            if lengths { 25 } else { 0 },
        ]) {
            0 => self.literal(p),
            1 => match self.place_of(ValTy::Prim(p), Access::Read) {
                Some(place) => Expr::value(place.text),
                None => self.literal(p),
            },
            2 => Expr::value((*self.rng.pick(&consts)).to_owned()),
            _ => self.len_expr().unwrap_or_else(|| self.literal(p)),
        }
    }

    /// `*F(..)` for a function returning a reference to a `Copy` value of type `ty`.
    fn deref_call(&mut self, ty: ValTy, depth: u32) -> Option<Expr> {
        if !self.prog.is_copy(Ty::Val(ty)) {
            return None;
        }
        let fns = self.callables(|r| matches!(r, RetTy::Ref { inner, .. } if inner == ty));
        if fns.is_empty() {
            return None;
        }
        let callee = *self.rng.pick(&fns);
        let call = self.call(callee, depth)?;
        Some(Expr::value(format!("(*{})", call.text)))
    }

    fn value_call(&mut self, ty: ValTy, depth: u32) -> Option<Expr> {
        let fns = self.callables(|r| r == RetTy::Val(ty));
        if fns.is_empty() {
            return None;
        }
        let callee = *self.rng.pick(&fns);
        self.call(callee, depth)
    }

    /// `(if c { a } else { b })`; moves in either branch count afterwards.
    fn if_expr(&mut self, ty: ValTy, depth: u32) -> Expr {
        let c = self.prim_expr(Prim::Bool, depth.saturating_sub(1));
        let before: Vec<bool> = self.vars.iter().map(|v| v.moved).collect();
        let a = self.val_expr(ty, depth.saturating_sub(1));
        let after_a: Vec<bool> = self.vars.iter().map(|v| v.moved).collect();
        for (v, &m) in self.vars.iter_mut().zip(&before) {
            v.moved = m;
        }
        let b = self.val_expr(ty, depth.saturating_sub(1));
        for (v, &m) in self.vars.iter_mut().zip(&after_a) {
            v.moved |= m;
        }
        Expr::value(format!(
            "(if {} {{ {} }} else {{ {} }})",
            c.text, a.text, b.text
        ))
    }

    fn cast_to(&mut self, target: Prim, depth: u32) -> Expr {
        let d = depth.saturating_sub(1);
        match target {
            Prim::Int(_) => {
                let fieldless: Vec<usize> = (0..self.prog.enums.len())
                    .filter(|&e| self.prog.enums[e].fieldless())
                    .collect();
                let source = match self.rng.weighted(&[
                    60,
                    10,
                    10,
                    10,
                    10,
                    if fieldless.is_empty() { 0 } else { 12 },
                    if self.prog.arrays.is_empty() { 0 } else { 10 },
                ]) {
                    0 => Prim::Int(*self.rng.pick(&IntTy::ALL)),
                    1 => Prim::F32,
                    2 => Prim::F64,
                    3 => Prim::Bool,
                    4 => Prim::Char,
                    6 => {
                        // The length of an array or slice (`(a.len() as i32)`).
                        let Some(e) = self.len_expr() else {
                            return self.prim_leaf(target);
                        };
                        return Expr::value(format!("({} as {})", e.text, target.name()));
                    }
                    _ => {
                        // A fieldless enum's discriminant (`E::A as u8`, which wraps).
                        let e = *self.rng.pick(&fieldless);
                        let e = self.enum_expr(e, d);
                        return Expr::value(format!("({} as {})", e.text, target.name()));
                    }
                };
                let e = self.prim_expr(source, d);
                Expr::value(format!("({} as {})", e.text, target.name()))
            }
            Prim::F32 | Prim::F64 => {
                let source = match self.rng.weighted(&[60, 20, 20]) {
                    0 => Prim::Int(*self.rng.pick(&IntTy::ALL)),
                    1 => Prim::F32,
                    _ => Prim::F64,
                };
                let e = self.prim_expr(source, d);
                Expr::value(format!("({} as {})", e.text, target.name()))
            }
            Prim::Char => {
                // Only `u8` casts to `char`; keep the result printable ASCII.
                let e = self.prim_expr(Prim::Int(IntTy::U8), d);
                Expr::value(format!("((({} % 95u8) + 32u8) as char)", e.text))
            }
            Prim::Bool => self.prim_leaf(Prim::Bool),
        }
    }

    fn arith(&mut self, p: Prim, depth: u32) -> Expr {
        let sub = depth.saturating_sub(1);
        let op = *self.rng.pick(&["+", "-", "*", "/", "%"]);
        let lhs = self.prim_expr(p, sub);
        let rhs = if matches!(op, "/" | "%") && self.rng.chance(60) {
            // A divisor that is usually not zero, so most programs run to the end.
            match p {
                Prim::Int(int) => {
                    let value = self.rng.range(1, 12);
                    let value = if int.signed() && self.rng.chance(25) {
                        -value
                    } else {
                        value
                    };
                    Expr::value(int_literal(int, i128::from(value)))
                }
                _ => Expr::value(float_literal(self.rng, p).0),
            }
        } else {
            self.prim_expr(p, sub)
        };
        Expr::value(format!("({} {op} {})", lhs.text, rhs.text))
    }

    fn shift(&mut self, i: IntTy, depth: u32) -> Expr {
        let d = depth.saturating_sub(1);
        let op = *self.rng.pick(&["<<", ">>"]);
        let a = self.prim_expr(Prim::Int(i), d);
        let amount_ty = *self.rng.pick(&IntTy::ALL);
        let b = if self.rng.chance(88) {
            let max = i64::from(i.bits()) - 1;
            Expr::value(int_literal(amount_ty, i128::from(self.rng.range(0, max))))
        } else {
            self.prim_expr(Prim::Int(amount_ty), d)
        };
        Expr::value(format!("({} {op} {})", a.text, b.text))
    }

    fn compare(&mut self, depth: u32) -> Expr {
        let d = depth.saturating_sub(1);
        let p = random_prim(self.rng);
        let op = *self.rng.pick(&["==", "!=", "<", "<=", ">", ">="]);
        if self.rng.chance(14) {
            return self.str_compare(op, d);
        }
        if self.rng.chance(20) {
            if let Some(e) = self.ref_compare(p, op, d) {
                return e;
            }
        }
        if self.rng.chance(6) {
            // Between `&&p` operands (#11): `(rr < &r)`, `(&&x == rr)`.
            if let Some(e) = self.ref_ref_compare(ValTy::Prim(p), op) {
                return e;
            }
        }
        let a = self.prim_expr(p, d);
        let b = self.prim_expr(p, d);
        Expr::value(format!("({} {op} {})", a.text, b.text))
    }

    /// `(a op b)` between two `&p` operands, which compares the referents: borrows of
    /// places, reference locals (`&` or `&mut`, only reborrowed), calls returning
    /// references and borrows of temporaries. The left operand's loans are held while the
    /// right one is generated, as for call arguments: both languages keep them alive until
    /// the comparison. `None` when no reference to a `p` is in reach.
    fn ref_compare(&mut self, p: Prim, op: &str, depth: u32) -> Option<Expr> {
        let mut a = self.ref_compare_operand(p, depth)?;
        if !matches!(op, "==" | "!=") && !a.text.starts_with('&') {
            // A reference local or call may be `&mut`, and `PartialOrd` has no impl for
            // `&mut A` against `&B` (a `&mut` right operand is coerced to `&`).
            a.text = format!("&*{}", a.text);
        }
        self.lock(&a.roots);
        let b = self.ref_compare_operand(p, depth);
        self.unlock(&a.roots);
        let b = b?;
        Some(Expr::value(format!("({} {op} {})", a.text, b.text)))
    }

    fn ref_compare_operand(&mut self, p: Prim, depth: u32) -> Option<Expr> {
        // A borrowed temporary must not be a place (`&(x)` borrows `x`): a literal or an
        // `if` expression.
        match self.rng.weighted(&[70, 15, 15]) {
            0 => self.ref_expr(false, ValTy::Prim(p), depth),
            1 => Some(Expr::value(format!("&{}", self.literal(p).text))),
            _ => {
                let e = self.if_expr(ValTy::Prim(p), depth);
                Some(Expr::value(format!("&{}", e.text)))
            }
        }
    }

    fn prim_expr(&mut self, p: Prim, depth: u32) -> Expr {
        if depth == 0 || self.rng.chance(15) {
            return self.prim_leaf(p);
        }
        if depth >= 2 && self.rng.chance(4) {
            return self.inline_match(ValTy::Prim(p), depth);
        }
        if depth >= 2 && self.rng.chance(2) {
            return self.str_inline_match(ValTy::Prim(p), depth);
        }
        let has_calls = !self
            .callables(|r| r == RetTy::Val(ValTy::Prim(p)))
            .is_empty();
        let has_ref_calls = !self
            .callables(|r| matches!(r, RetTy::Ref { inner, .. } if inner == ValTy::Prim(p)))
            .is_empty();
        let call_w = if has_calls { 12 } else { 0 };
        let deref_w = if has_ref_calls { 8 } else { 0 };
        let sub = depth.saturating_sub(1);
        let choice = match p {
            Prim::Int(int) => {
                let neg_w = if int.signed() { 6 } else { 0 };
                match self
                    .rng
                    .weighted(&[36, 10, 8, neg_w, 5, 14, call_w, deref_w, 6])
                {
                    0 => return self.arith(p, depth),
                    1 => {
                        let op = *self.rng.pick(&["&", "|", "^"]);
                        let lhs = self.prim_expr(p, sub);
                        let rhs = self.prim_expr(p, sub);
                        return Expr::value(format!("({} {op} {})", lhs.text, rhs.text));
                    }
                    2 => return self.shift(int, depth),
                    3 => {
                        let operand = self.prim_expr(p, sub);
                        // `--128i8` and `-(-128i8)` are out-of-range literals to rustc (only
                        // the outer negation counts), so a negative literal is negated
                        // through a cast: `(-(-128i8 as i8))`, which overflows at run time.
                        if operand.text.starts_with('-') {
                            return Expr::value(format!("(-({} as {}))", operand.text, p.name()));
                        }
                        return Expr::value(format!("(-{})", operand.text));
                    }
                    4 => {
                        let operand = self.prim_expr(p, sub);
                        return Expr::value(format!("(!{})", operand.text));
                    }
                    5 => return self.cast_to(p, depth),
                    n => n,
                }
            }
            Prim::F32 | Prim::F64 => match self.rng.weighted(&[45, 10, 20, call_w, deref_w, 8]) {
                0 => return self.arith(p, depth),
                1 => {
                    let operand = self.prim_expr(p, sub);
                    return Expr::value(format!("(-{})", operand.text));
                }
                2 => return self.cast_to(p, depth),
                n => n + 3,
            },
            Prim::Bool => match self.rng.weighted(&[45, 20, 10, call_w, deref_w, 6]) {
                0 => return self.compare(depth),
                1 => {
                    let op = *self.rng.pick(&["&&", "||"]);
                    let lhs = self.prim_expr(p, sub);
                    let rhs = self.prim_expr(p, sub);
                    return Expr::value(format!("({} {op} {})", lhs.text, rhs.text));
                }
                2 => {
                    let operand = self.prim_expr(p, sub);
                    return Expr::value(format!("(!{})", operand.text));
                }
                n => n + 3,
            },
            Prim::Char => match self.rng.weighted(&[40, call_w, deref_w, 10, 30]) {
                0 => return self.cast_to(p, depth),
                1 => 6,
                2 => 7,
                3 => 8,
                _ => return self.prim_leaf(p),
            },
        };
        match choice {
            6 => self
                .value_call(ValTy::Prim(p), depth)
                .unwrap_or_else(|| self.prim_leaf(p)),
            7 => self
                .deref_call(ValTy::Prim(p), depth)
                .unwrap_or_else(|| self.prim_leaf(p)),
            _ => self.if_expr(ValTy::Prim(p), depth),
        }
    }

    fn struct_literal(&mut self, s: usize, depth: u32) -> Expr {
        let prog = self.prog;
        let def = &prog.structs[s];
        let mut fields = Vec::new();
        for f in &def.fields {
            let e = self.typed_expr(f.ty, depth.saturating_sub(1));
            fields.push(format!("{}: {}", f.name, e.text));
        }
        Expr::value(format!("{} {{ {} }}", def.name, fields.join(", ")))
    }

    fn struct_expr(&mut self, s: usize, depth: u32) -> Expr {
        let has_calls = !self
            .callables(|r| r == RetTy::Val(ValTy::Struct(s)))
            .is_empty();
        let choice = if depth == 0 {
            self.rng.weighted(&[50, 50, 0, 0, 0])
        } else {
            self.rng
                .weighted(&[35, 30, if has_calls { 15 } else { 0 }, 10, 6])
        };
        match choice {
            0 => self.struct_literal(s, depth),
            1 => match self.place_of(ValTy::Struct(s), Access::Read) {
                Some(place) => Expr::value(place.text),
                None => self.struct_literal(s, depth),
            },
            2 => self
                .value_call(ValTy::Struct(s), depth)
                .unwrap_or_else(|| self.struct_literal(s, depth)),
            3 => self.if_expr(ValTy::Struct(s), depth),
            _ => self
                .deref_call(ValTy::Struct(s), depth)
                .unwrap_or_else(|| self.struct_literal(s, depth)),
        }
    }

    /// `(a, b)`, or `(a,)` for one element; elements are evaluated left to right.
    fn tuple_literal(&mut self, t: usize, depth: u32) -> Expr {
        let prog = self.prog;
        let elems: Vec<String> = prog.tuples[t]
            .iter()
            .map(|e| self.val_expr(*e, depth.saturating_sub(1)).text)
            .collect();
        if elems.len() == 1 {
            Expr::value(format!("({},)", elems[0]))
        } else {
            Expr::value(format!("({})", elems.join(", ")))
        }
    }

    fn tuple_expr(&mut self, t: usize, depth: u32) -> Expr {
        let ty = ValTy::Tuple(t);
        let has_calls = !self.callables(|r| r == RetTy::Val(ty)).is_empty();
        let choice = if depth == 0 {
            self.rng.weighted(&[50, 50, 0, 0, 0])
        } else {
            self.rng
                .weighted(&[40, 30, if has_calls { 15 } else { 0 }, 10, 5])
        };
        match choice {
            0 => self.tuple_literal(t, depth),
            1 => match self.place_of(ty, Access::Read) {
                Some(place) => Expr::value(place.text),
                None => self.tuple_literal(t, depth),
            },
            2 => self
                .value_call(ty, depth)
                .unwrap_or_else(|| self.tuple_literal(t, depth)),
            3 => self.if_expr(ty, depth),
            _ => self
                .deref_call(ty, depth)
                .unwrap_or_else(|| self.tuple_literal(t, depth)),
        }
    }

    /// A reference local usable as `&inner` (or `&mut inner` when `mutable`), also by an
    /// unsizing coercion (a `&[T; N]` local for a `&[T]`) where the expected type is known.
    fn ref_var(&mut self, mutable: bool, inner: ValTy) -> Option<Expr> {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for (i, v) in self.vars.iter().enumerate() {
            let Ty::Ref {
                mutable: m,
                inner: t,
            } = v.ty
            else {
                continue;
            };
            if v.hidden || v.dead || !self.prog.ref_coerces(t, inner) || (mutable && !m) {
                continue;
            }
            let free = !v.moved && v.mut_locks == 0 && (!mutable || v.shared_locks == 0);
            if free {
                ok.push(i);
            } else {
                bad.push(i);
            }
        }
        if !mutable && self.rng.chance(if ok.is_empty() { 60 } else { 15 }) {
            // `(*rr)`, a `&T` copied out of a `&&T` or `&mut &T`.
            if let Some(e) = self.deref_ref_ref(inner) {
                return Some(e);
            }
        }
        let pool = self.choose_pool(ok, bad)?;
        let v = *self.rng.pick(&pool);
        Some(Expr {
            text: self.vars[v].name.clone(),
            roots: vec![(v, mutable)],
        })
    }

    /// An expression of type `&inner` / `&mut inner`: a borrow of a place, a reference
    /// local, a call returning such a reference, or (shared) an element of an array of
    /// them (`[&a, r][i]`, #43). `None` when nothing in scope fits. For
    /// a slice `inner`, the expression may have a reference to an array type instead, which
    /// coerces where the expected type is known: every caller writes it there (a call
    /// argument, an annotated `let`).
    fn ref_expr(&mut self, mutable: bool, inner: ValTy, depth: u32) -> Option<Expr> {
        let prog = self.prog;
        // `GenFirst(r)` (#114): the shared reference `r`, with its loans. Not for arrays,
        // whose references may coerce to slices where `T` would have to be unsized.
        if prog.generics
            && !mutable
            && inner != ValTy::Str
            && prog.array_of(inner).is_none()
            && depth > 0
            && self.rng.chance(25)
        {
            let r = self.ref_expr(false, inner, depth - 1)?;
            let arg = if r.text.starts_with('&') {
                r.text
            } else {
                format!("&*{}", r.text)
            };
            return Some(Expr {
                text: format!("GenFirst({arg})"),
                roots: r.roots,
            });
        }
        if inner == ValTy::Str {
            // Strings are never borrowed mutably in the subset.
            return (!mutable).then(|| self.str_expr(depth));
        }
        let fns = self.callables(|r| {
            matches!(r, RetTy::Ref { mutable: m, inner: t }
                if prog.ref_coerces(t, inner) && (m || !mutable))
        });
        // A slice may also be a range of an array or slice (`&a[1..]`, `&mut s[..2]`).
        let slice = prog.array_of(inner).is_some_and(|(_, len)| len.is_none());
        // An element of an array of references (#43): `[&a, &*r][i]`. Never of slices,
        // whose elements could be references to arrays of different lengths, nor of
        // mutable references, which cannot be moved out of an array. A shared reference
        // wanted may be given as a `&mut` local (`r`) or a call returning one, so every
        // element but a fresh shared borrow is reborrowed shared, giving the elements one
        // type (seed 7000161: `[tmp, &*tmp]` mixed `&mut f64` and `&f64`).
        if !mutable && !slice && depth > 0 && self.range_depth == 0 && self.rng.chance(6) {
            let element = |g: &mut Self| {
                g.ref_expr(false, inner, depth - 1).map(|mut e| {
                    if !e.text.starts_with('&') {
                        e.text = format!("&*{}", e.text);
                    }
                    e
                })
            };
            if let Some(e) = self.ref_array_element(element) {
                return Some(e);
            }
        }
        let order = match self.rng.weighted(&[
            55,
            25,
            if fns.is_empty() || depth == 0 { 0 } else { 20 },
            if slice { 30 } else { 0 },
        ]) {
            0 => [0, 1, 2, 3],
            1 => [1, 0, 2, 3],
            2 => [2, 0, 1, 3],
            _ => [3, 0, 1, 2],
        };
        for kind in order {
            let found = match kind {
                0 => self
                    .place_where(|t| prog.ref_coerces(t, inner), Access::Borrow(mutable))
                    .map(|place| Expr {
                        text: format!("&{}{}", if mutable { "mut " } else { "" }, place.text),
                        roots: vec![(place.root, mutable)],
                    }),
                1 => self.ref_var(mutable, inner),
                3 if slice => self.range_borrow(mutable, inner),
                3 => None,
                _ => {
                    if fns.is_empty() || depth == 0 {
                        None
                    } else {
                        let callee = *self.rng.pick(&fns);
                        let returns_mut =
                            matches!(prog.fns[callee].ret, RetTy::Ref { mutable: true, .. });
                        // A `&mut` result wanted as `&` is reborrowed explicitly: an
                        // unannotated `let` (or a match on it) would keep the `&mut` type
                        // (#77, seed 45000063 then wrote `let m: &&Phase = &delta;`).
                        self.call(callee, depth).map(|mut e| {
                            if returns_mut && !mutable {
                                e.text = format!("&*{}", e.text);
                            }
                            e
                        })
                    }
                }
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// The tail of a function returning a reference: a reference into its single reference
    /// parameter.
    fn ref_tail(&mut self, mutable: bool, inner: ValTy) -> Option<String> {
        let p = self.vars.iter().position(|v| v.ty.is_ref())?;
        let Ty::Ref {
            mutable: pm,
            inner: pinner,
        } = self.vars[p].ty
        else {
            return None;
        };
        let name = self.vars[p].name.clone();
        // A reference to an array returned as a slice coerces at the return.
        let mut options: Vec<String> = self
            .prog
            .sub_places(pinner)
            .into_iter()
            .filter(|(_, t)| self.prog.ref_coerces(*t, inner))
            .map(|(path, _)| {
                if path.is_empty() {
                    if pm == mutable {
                        name.clone()
                    } else {
                        format!("&*{name}")
                    }
                } else {
                    // A slice element at an index that is in bounds unless it is empty.
                    let path = path.replace("[?]", "[0]");
                    format!("&{}{name}{path}", if mutable { "mut " } else { "" })
                }
            })
            .collect();
        if let Some(range) = self.range_tail(p, mutable, inner) {
            options.push(range);
        }
        if inner == ValTy::Str {
            // A literal is `'static`: it may be returned whatever the parameter.
            options.push(crate::program::str_literal(self.rng).0);
        }
        if options.is_empty() {
            return None;
        }
        if options.len() >= 2 && self.rng.chance(40) {
            let c = self.prim_expr(Prim::Bool, 1);
            let a = self.rng.pick(&options).clone();
            let b = self.rng.pick(&options).clone();
            return Some(format!("if {} {{ {a} }} else {{ {b} }}", c.text));
        }
        Some(self.rng.pick(&options).clone())
    }

    // ----- statements ----------------------------------------------------------------

    fn stmt(&mut self) {
        self.expire_refs();
        let mut done = false;
        for _ in 0..8 {
            if self.try_stmt() {
                done = true;
                break;
            }
        }
        if !done {
            let e = self.prim_leaf(Prim::Int(IntTy::I32));
            self.line(&format!("PrintLine({});", e.text));
        }
        let block = self.cur_block();
        self.blocks[block].stmt_index += 1;
    }

    #[allow(clippy::too_many_lines)]
    fn try_stmt(&mut self) -> bool {
        let depth = self.knobs.max_expr_depth;
        let nesting_ok = self.blocks.len() <= self.knobs.max_block_depth && self.remaining() > 6;
        let in_loop = !self.loops.is_empty();
        let has_fns = self.fn_index > 0;
        let returns = self.sig.ret != RetTy::Unit;
        let has_enums = !self.prog.enums.is_empty();
        let has_arrays = !self.prog.arrays.is_empty();
        let weights = [
            18,                                               // 0 let
            10,                                               // 1 let ref
            12,                                               // 2 assign
            10,                                               // 3 compound
            16,                                               // 4 print
            6,                                                // 5 show struct
            if has_fns { 10 } else { 0 },                     // 6 call statement
            if nesting_ok { 9 } else { 0 },                   // 7 if
            if nesting_ok { 6 } else { 0 },                   // 8 while
            if nesting_ok { 4 } else { 0 },                   // 9 loop
            if nesting_ok { 4 } else { 0 },                   // 10 let = loop { break v }
            if in_loop { 7 } else { 0 },                      // 11 guarded break/continue
            if returns { 3 } else { 1 },                      // 12 guarded return
            if has_fns { 5 } else { 0 }, // 13 write through a returned reference
            1,                           // 14 guarded panic
            if self.prog.tuples.is_empty() { 0 } else { 10 }, // 15 destructuring let
            if self.has_methods() { 14 } else { 0 }, // 16 method call
            if nesting_ok { 8 } else { 0 }, // 17 match statement
            if nesting_ok { 5 } else { 0 }, // 18 let = match
            if nesting_ok { 9 } else { 0 }, // 19 if let (half of them let chains)
            if nesting_ok && has_enums { 3 } else { 0 }, // 20 while let
            if self.remaining() > 4 { 4 } else { 0 }, // 21 let-else
            4,                           // 22 destructuring let
            if nesting_ok && has_arrays { 7 } else { 0 }, // 23 while i < s.len()
            7,                           // 24 let of a `&str`
            if nesting_ok { 5 } else { 0 }, // 25 match on a `&str`
            if self.remaining() > 4 { 4 } else { 0 }, // 26 `&str` if let / let-else
            if nesting_ok { 5 } else { 0 }, // 27 loop over a `&str`
            if has_arrays { 7 } else { 0 }, // 28 let m = &mut a[i..j]
            8,                           // 29 let rr = &r; let m = &mut r;
            6,                           // 30 destructuring assignment
            5,                           // 31 let r = { &e }; if c { &a } else { &b }
            5,                           // 32 `[&T; N]`, `(&T, U)`, `[&mut T; N]` locals
        ];
        match self.rng.weighted(&weights) {
            0 => self.stmt_let(depth),
            1 => self.stmt_let_ref(),
            2 => self.stmt_assign(depth),
            3 => self.stmt_compound(depth),
            4 => self.stmt_print(depth),
            5 => self.stmt_show(),
            6 => self.stmt_call(depth),
            7 => self.stmt_if(depth),
            8 => self.stmt_while(depth),
            9 => self.stmt_loop(false),
            10 => self.stmt_loop(true),
            11 => self.stmt_jump(depth),
            12 => self.stmt_return(depth),
            13 => self.stmt_write_through_call(depth),
            15 => {
                if self.rng.chance(25) {
                    self.stmt_let_tuple_ref()
                } else {
                    self.stmt_let_tuple(depth)
                }
            }
            16 => self.stmt_method_call(depth),
            17 => self.stmt_match(depth),
            18 => self.stmt_let_match(depth),
            19 => self.stmt_if_let(depth),
            20 => self.stmt_while_let(depth),
            21 => self.stmt_let_else(depth),
            22 => self.stmt_let_destructure(depth),
            23 => self.stmt_index_loop(),
            24 => self.stmt_let_str(depth),
            25 => self.stmt_str_match(depth),
            26 => {
                if self.rng.chance(50) {
                    self.stmt_str_if_let(depth)
                } else {
                    self.stmt_str_let_else(depth)
                }
            }
            27 => self.stmt_str_loop(depth),
            28 => self.stmt_range_ref(depth),
            29 => self.stmt_let_ref_ref(depth),
            30 => self.stmt_assign_destructure(depth),
            31 => self.stmt_let_extended(depth),
            32 => self.stmt_ref_aggregate(depth),
            _ => {
                let c = self.prim_expr(Prim::Bool, 1);
                self.line(&format!("if {} {{ Panic(\"generated panic\"); }}", c.text));
                true
            }
        }
    }

    fn random_val_ty(&mut self) -> ValTy {
        let prog = self.prog;
        let sized: Vec<usize> = (0..prog.arrays.len())
            .filter(|&a| prog.arrays[a].len.is_some())
            .collect();
        if !self.prog.structs.is_empty() && self.rng.chance(25) {
            ValTy::Struct(self.rng.index(self.prog.structs.len()))
        } else if !self.prog.enums.is_empty() && self.rng.chance(25) {
            ValTy::Enum(self.rng.index(self.prog.enums.len()))
        } else if !self.prog.tuples.is_empty() && self.rng.chance(20) {
            ValTy::Tuple(self.rng.index(self.prog.tuples.len()))
        } else if !sized.is_empty() && self.rng.chance(30) {
            ValTy::Array(*self.rng.pick(&sized))
        } else if self.rng.chance(30) {
            // The type of something already in scope, so the new local can interact with it.
            let places: Vec<ValTy> = self
                .candidates()
                .into_iter()
                .map(|p| p.ty)
                .filter(|&t| prog.is_sized(t))
                .collect();
            if places.is_empty() {
                ValTy::Prim(random_prim(self.rng))
            } else {
                *self.rng.pick(&places)
            }
        } else {
            ValTy::Prim(random_prim(self.rng))
        }
    }

    fn stmt_let(&mut self, depth: u32) -> bool {
        let ty = self.random_val_ty();
        let annotated = self.rng.chance(60);
        let e = if annotated {
            self.typed_expr(ty, depth)
        } else {
            self.val_expr(ty, depth)
        };
        let name = self.fresh_name();
        let mutable = self.rng.chance(50);
        let annotation = if annotated {
            format!(": {}", self.prog.val_name(ty))
        } else {
            String::new()
        };
        self.line(&format!(
            "let {}{name}{annotation} = {};",
            if mutable { "mut " } else { "" },
            e.text
        ));
        self.declare(name, Ty::Val(ty), mutable, false);
        true
    }

    /// `let (a, (b, _), mut c): T = e;`: destructures a tuple value. `_` binds (and moves)
    /// nothing; the model still counts a moved initialiser place as moved as a whole,
    /// which is conservative.
    fn stmt_let_tuple(&mut self, depth: u32) -> bool {
        let t = self.rng.index(self.prog.tuples.len());
        let e = self.val_expr(ValTy::Tuple(t), depth);
        let mut binds = Vec::new();
        let pat = self.tuple_pattern(t, 0, &mut binds);
        let annotation = if self.rng.chance(40) {
            format!(": {}", self.prog.val_name(ValTy::Tuple(t)))
        } else {
            String::new()
        };
        self.line(&format!("let {pat}{annotation} = {};", e.text));
        for (name, ty, mutable) in binds {
            self.declare(name, Ty::Val(ty), mutable, false);
        }
        true
    }

    /// A pattern for tuple type `t`: per element `_`, a nested pattern (for a tuple element,
    /// at most two levels deep) or a fresh binding, `mut` now and then. The bindings are
    /// appended to `binds` as `(name, type, mutable)`.
    fn tuple_pattern(
        &mut self,
        t: usize,
        depth: u32,
        binds: &mut Vec<(String, ValTy, bool)>,
    ) -> String {
        let prog = self.prog;
        let mut parts = Vec::new();
        for &elem in &prog.tuples[t] {
            let part = match elem {
                ValTy::Tuple(inner) if depth < 2 && self.rng.chance(40) => {
                    self.tuple_pattern(inner, depth + 1, binds)
                }
                _ if self.rng.chance(20) => "_".to_owned(),
                _ => {
                    let name = self.fresh_name();
                    let mutable = self.rng.chance(30);
                    binds.push((name.clone(), elem, mutable));
                    format!("{}{name}", if mutable { "mut " } else { "" })
                }
            };
            parts.push(part);
        }
        if parts.len() == 1 {
            format!("({},)", parts[0])
        } else {
            format!("({})", parts.join(", "))
        }
    }

    /// `let (a, _, b) = &t;` (or `&mut t`, a reference local, a call returning a reference):
    /// default binding modes bind references to the elements, each a reference
    /// local with its own region over the borrowed roots.
    fn stmt_let_tuple_ref(&mut self) -> bool {
        let t = self.rng.index(self.prog.tuples.len());
        let mutable = self.rng.chance(50);
        let Some(mut e) = self.ref_expr(mutable, ValTy::Tuple(t), self.knobs.max_expr_depth) else {
            return false;
        };
        // As in `stmt_let_ref`: a bare reference local is reborrowed explicitly.
        if let [(root, _)] = e.roots[..] {
            if e.text == self.vars[root].name && matches!(self.vars[root].ty, Ty::Ref { .. }) {
                e.text = format!("&{}*{}", if mutable { "mut " } else { "" }, e.text);
            }
        }
        let prog = self.prog;
        let mut parts = Vec::new();
        let mut binds = Vec::new();
        for &elem in &prog.tuples[t] {
            if self.rng.chance(25) {
                parts.push("_".to_owned());
            } else {
                let name = self.fresh_name();
                parts.push(name.clone());
                binds.push((name, elem));
            }
        }
        let pat = if parts.len() == 1 {
            format!("({},)", parts[0])
        } else {
            format!("({})", parts.join(", "))
        };
        self.line(&format!("let {pat} = {};", e.text));
        for (name, inner) in binds {
            self.bind_ref(name, Ty::Ref { mutable, inner }, &e);
        }
        true
    }

    fn stmt_let_ref(&mut self) -> bool {
        let places = self.candidates();
        if places.is_empty() {
            return false;
        }
        let mut inner = self.rng.pick(&places).ty;
        if inner == ValTy::Str {
            return self.stmt_let_str(self.knobs.max_expr_depth);
        }
        // An array borrowed as a slice of its elements (`let s: &[T] = &a;`).
        if let Some((elem, Some(_))) = self.prog.array_of(inner) {
            if let Some(slice) = self.prog.slice_of(elem) {
                if self.rng.chance(45) {
                    inner = slice;
                }
            }
        }
        let mutable = self.rng.chance(50);
        let Some(mut e) = self.ref_expr(mutable, inner, self.knobs.max_expr_depth) else {
            return false;
        };
        let name = self.fresh_name();
        let ty = Ty::Ref { mutable, inner };
        // `let q = r;` with `r: &mut T` moves `r` (only an annotated `let` reborrows), so a
        // bare reference local is always reborrowed explicitly.
        if let [(root, _)] = e.roots[..] {
            if e.text == self.vars[root].name && matches!(self.vars[root].ty, Ty::Ref { .. }) {
                e.text = format!("&{}*{}", if mutable { "mut " } else { "" }, e.text);
            }
        }
        // A slice reference is always annotated: the expression may be a reference to an
        // array, which only the annotation coerces.
        let annotation = if !self.prog.is_sized(inner) || self.rng.chance(40) {
            format!(": {}", self.prog.ty_name(ty))
        } else {
            String::new()
        };
        self.line(&format!("let {name}{annotation} = {};", e.text));
        self.bind_ref(name, ty, &e);
        true
    }

    fn stmt_assign(&mut self, depth: u32) -> bool {
        let Some((mut place, violation)) = self.pick_place(Access::Write, |_| true) else {
            return false;
        };
        // Rust evaluates the right-hand side first (then an index on the left, which must
        // not read what the right-hand side moved).
        let e = self.typed_expr(place.ty, depth);
        self.finish_place(&mut place, Ranges::Mut);
        if violation || self.permitted(&place, Access::Write) {
            self.apply(&place, Access::Write);
            self.line(&format!("{} = {};", place.text, e.text));
        } else {
            let name = self.fresh_name();
            let ty = self.prog.val_name(place.ty);
            self.line(&format!("let {name}: {ty} = {};", e.text));
            self.declare(name, Ty::Val(place.ty), false, false);
        }
        true
    }

    /// `(p, q) = (e, f);`, a destructuring assignment (#50): both values are evaluated
    /// before either place, then each place is evaluated and assigned in turn (rustc's
    /// `{ let (x, y) = (e, f); p = x; q = y; }`). A place the rules forbid by its turn is
    /// `_` instead, which drops its value.
    fn stmt_assign_destructure(&mut self, depth: u32) -> bool {
        let Some((mut first, first_violation)) = self.pick_place(Access::Write, |_| true) else {
            return false;
        };
        // A different second place when there is one (`(a, a) = ..` is valid but dull).
        let mut second = None;
        for _ in 0..4 {
            second = self.pick_place(Access::Write, |_| true);
            if second.as_ref().is_some_and(|(p, _)| p.text != first.text) {
                break;
            }
        }
        let Some((mut second, second_violation)) = second else {
            return false;
        };
        let mut e = self.typed_expr(first.ty, depth);
        let mut f = self.typed_expr(second.ty, depth);
        let mut targets = Vec::new();
        for (place, violation, value) in [
            (&mut first, first_violation, &mut e),
            (&mut second, second_violation, &mut f),
        ] {
            self.finish_place(place, Ranges::Mut);
            if violation || self.permitted(place, Access::Write) {
                self.apply(place, Access::Write);
                targets.push(place.text.clone());
            } else {
                targets.push("_".to_owned());
                // `[]` took its type from the place; `_` gives it none (rustc's E0282,
                // seed 1002197, #78). Any empty array does: it is dropped at once.
                if value.text == "[]" {
                    "[0u8; 0]".clone_into(&mut value.text);
                }
            }
        }
        self.line(&format!(
            "({}, {}) = ({}, {});",
            targets[0], targets[1], e.text, f.text
        ));
        true
    }

    fn stmt_compound(&mut self, depth: u32) -> bool {
        let Some((mut place, violation)) = self.pick_place(
            Access::Write,
            |t| matches!(t, ValTy::Prim(p) if p.is_numeric()),
        ) else {
            return false;
        };
        let ValTy::Prim(p) = place.ty else {
            return false;
        };
        let op = if p.is_int() {
            *self
                .rng
                .pick(&["+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>="])
        } else {
            *self.rng.pick(&["+=", "-=", "*=", "/="])
        };
        let e = match (op, p) {
            ("<<=" | ">>=", Prim::Int(i)) => {
                let amount_ty = *self.rng.pick(&IntTy::ALL);
                let max = i64::from(i.bits()) - 1;
                Expr::value(int_literal(amount_ty, i128::from(self.rng.range(0, max))))
            }
            ("/=" | "%=", Prim::Int(i)) if self.rng.chance(60) => {
                Expr::value(int_literal(i, i128::from(self.rng.range(1, 12))))
            }
            _ => self.prim_expr(p, depth),
        };
        // The right-hand side of a primitive compound assignment is evaluated first too.
        self.finish_place(&mut place, Ranges::Mut);
        let moved = self.vars[place.root].moved;
        if violation || (!moved && self.permitted(&place, Access::Write)) {
            self.line(&format!("{} {op} {};", place.text, e.text));
        } else {
            let name = self.fresh_name();
            self.line(&format!("let {name} = {};", e.text));
            self.declare(name, Ty::Val(place.ty), false, false);
        }
        true
    }

    fn stmt_print(&mut self, depth: u32) -> bool {
        if self.rng.chance(6) {
            let word = *self.rng.pick(&["step", "done", "value", "check", "mark"]);
            self.line(&format!("PrintLine(\"{word}\");"));
            return true;
        }
        if self.rng.chance(12) {
            let e = self.str_expr(depth.saturating_sub(1));
            let f = if self.rng.chance(75) {
                "PrintLine"
            } else {
                "Print"
            };
            self.line(&format!("{f}({});", e.text));
            return true;
        }
        let p = random_prim(self.rng);
        let e = self.prim_expr(p, depth);
        let f = if self.rng.chance(80) {
            "PrintLine"
        } else {
            "Print"
        };
        self.line(&format!("{f}({});", e.text));
        true
    }

    /// Prints every primitive leaf of a struct, tuple or array in scope on one line (a
    /// slice's length), or an enum through a `match` on a reference to it. The place is
    /// written once per leaf, so its index expressions are leaves.
    fn stmt_show(&mut self) -> bool {
        let Some((place, _)) = self.simply(|gen| {
            gen.any_place(Access::Borrow(false), |t| {
                matches!(
                    t,
                    ValTy::Struct(_)
                        | ValTy::Tuple(_)
                        | ValTy::Enum(_)
                        | ValTy::Array(_)
                        | ValTy::Str
                )
            })
        }) else {
            return false;
        };
        if let ValTy::Enum(e) = place.ty {
            if self.remaining() < 6 {
                return false;
            }
            self.show_enum(&format!("&{}", place.text), e);
            return true;
        }
        let line = self.show_line(&place.text, place.ty);
        self.line(&line);
        true
    }

    /// Prints the primitive leaves of `base` (prints the type's name when it has none: a
    /// struct of enums), or a slice's length.
    fn show_line(&self, base: &str, ty: ValTy) -> String {
        if ty == ValTy::Str {
            // The `&str` itself: `s` for `*s`, `*ss` for `**ss` (only a `&str` prints).
            let text = base.strip_prefix('*').unwrap_or(base);
            return format!("PrintLine({text});");
        }
        if !self.prog.is_sized(ty) {
            return format!("PrintLine({}.len());", arrays::len_receiver(base));
        }
        let leaves = self.prog.leaves(ty);
        if leaves.is_empty() {
            return format!("PrintLine(\"{}\");", self.prog.val_name(ty));
        }
        let mut out = String::new();
        let last = leaves.len() - 1;
        for (i, (path, _)) in leaves.iter().enumerate() {
            let text = if path.is_empty() {
                base.to_owned()
            } else if base.starts_with('*') {
                // `r.X` for `*r`, `rr.X` for `**rr`: auto-deref.
                format!("{}{path}", base.trim_start_matches('*'))
            } else {
                format!("{base}{path}")
            };
            if i == last {
                let _ = write!(out, "PrintLine({text});");
            } else {
                let _ = write!(out, "Print({text}); Print(' '); ");
            }
        }
        out
    }

    /// The methods (functions with a receiver) this body may call.
    fn methods(&self) -> Vec<usize> {
        (0..self.fn_index)
            .filter(|&i| self.prog.fns[i].owner.is_some_and(|o| o.receiver.is_some()))
            .collect()
    }

    fn has_methods(&self) -> bool {
        !self.methods().is_empty()
    }

    /// `p.Name(args);`: a method called with method syntax on a place (or reference local)
    /// in scope, its arguments preferring to read that place, as in `v.Add(v.Get())`.
    fn stmt_method_call(&mut self, depth: u32) -> bool {
        let methods = self.methods();
        if methods.is_empty() {
            return false;
        }
        let callee = *self.rng.pick(&methods);
        let owner = self.prog.fns[callee].owner.expect("a method");
        let ty = owner.ty;
        let in_scope = self.candidates().iter().any(|p| p.ty == ty);
        if !in_scope || self.rng.chance(30) {
            // A fresh local to call it on.
            let e = self.val_expr(ty, self.knobs.max_expr_depth.saturating_sub(1));
            let name = self.fresh_name();
            self.line(&format!("let mut {name} = {};", e.text));
            self.declare(name, Ty::Val(ty), true, false);
        }
        self.force_method = true;
        let e = self.call(callee, depth);
        self.force_method = false;
        match e {
            Some(e) => {
                self.line(&format!("{};", e.text));
                true
            }
            None => false,
        }
    }

    fn stmt_call(&mut self, depth: u32) -> bool {
        let callee = self.rng.index(self.fn_index);
        match self.call(callee, depth) {
            Some(e) => {
                self.line(&format!("{};", e.text));
                true
            }
            None => false,
        }
    }

    fn snapshot_moved(&self) -> Vec<bool> {
        self.vars.iter().map(|v| v.moved).collect()
    }

    fn restore_moved(&mut self, snapshot: &[bool]) {
        for (v, &m) in self.vars.iter_mut().zip(snapshot) {
            v.moved = m;
        }
    }

    /// Joins the current state with another path's: a local moved on either is
    /// maybe-moved, which the borrow checker treats as moved. A re-initialisation inside an
    /// `if` without `else` or a loop body is only definite on some paths (the condition
    /// may be false, the loop may run zero times or `break` before it), so the state from
    /// before the construct is joined in afterwards.
    fn join_moved(&mut self, other: &[bool]) {
        for (v, &m) in self.vars.iter_mut().zip(other) {
            v.moved |= m;
        }
    }

    fn block_body(&mut self, max_stmts: i64) {
        self.open_block();
        let n = self.rng.range(1, max_stmts);
        for _ in 0..n {
            if self.remaining() <= 2 {
                break;
            }
            self.stmt();
        }
        self.close_block();
    }

    fn stmt_if(&mut self, depth: u32) -> bool {
        let c = self.prim_expr(Prim::Bool, depth);
        let before = self.snapshot_moved();
        self.line(&format!("if {} {{", c.text));
        self.block_body(3);
        let after_then = self.snapshot_moved();
        if self.rng.chance(60) {
            self.restore_moved(&before);
            self.line("} else {");
            self.block_body(3);
            self.join_moved(&after_then);
        } else {
            self.join_moved(&before);
        }
        self.line("}");
        true
    }

    fn counter(&mut self) -> String {
        let name = self.fresh_name();
        self.line(&format!("let mut {name}: i32 = 0i32;"));
        self.declare(name.clone(), Ty::prim(Prim::Int(IntTy::I32)), true, true);
        name
    }

    fn stmt_while(&mut self, depth: u32) -> bool {
        let i = self.counter();
        let bound = self.rng.range(1, 4);
        let before = self.snapshot_moved();
        self.loops.push(LoopKind::Stmt);
        let extra = if self.rng.chance(30) {
            let c = self.prim_expr(Prim::Bool, depth.saturating_sub(1));
            format!(" && {}", c.text)
        } else {
            String::new()
        };
        self.line(&format!("while ({i} < {bound}i32){extra} {{"));
        self.open_block();
        self.line(&format!("{i} += 1i32;"));
        let n = self.rng.range(1, 3);
        for _ in 0..n {
            if self.remaining() <= 2 {
                break;
            }
            self.stmt();
        }
        self.close_block();
        self.loops.pop();
        self.join_moved(&before);
        self.line("}");
        true
    }

    /// `loop { k += 1; if k > n { break [value]; } ... }`, as a statement or (with
    /// `value`) as the initialiser of a new local.
    fn stmt_loop(&mut self, value: bool) -> bool {
        let k = self.counter();
        let bound = self.rng.range(1, 4);
        let result = value.then(|| self.random_val_ty());
        let kind = match result {
            Some(ty) => LoopKind::Value(ty),
            None => LoopKind::Stmt,
        };
        let name = result.map(|_| self.fresh_name());
        match (&name, result) {
            (Some(name), Some(ty)) => {
                self.line(&format!("let {name}: {} = loop {{", self.prog.val_name(ty)));
            }
            _ => self.line("loop {"),
        }
        let before = self.snapshot_moved();
        self.loops.push(kind);
        self.open_block();
        self.line(&format!("{k} += 1i32;"));
        match result {
            Some(ty) => {
                let v = self.val_expr(ty, self.knobs.max_expr_depth.saturating_sub(1));
                self.line(&format!("if {k} > {bound}i32 {{ break {}; }}", v.text));
            }
            None => self.line(&format!("if {k} > {bound}i32 {{ break; }}")),
        }
        let n = self.rng.range(1, 3);
        for _ in 0..n {
            if self.remaining() <= 2 {
                break;
            }
            self.stmt();
        }
        self.close_block();
        self.loops.pop();
        self.join_moved(&before);
        match (name, result) {
            (Some(name), Some(ty)) => {
                self.line("};");
                self.declare(name, Ty::Val(ty), false, false);
            }
            _ => self.line("}"),
        }
        true
    }

    fn stmt_jump(&mut self, depth: u32) -> bool {
        let Some(&kind) = self.loops.last() else {
            return false;
        };
        let c = self.prim_expr(Prim::Bool, depth.saturating_sub(1));
        let jump = match kind {
            LoopKind::Stmt => {
                if self.rng.chance(50) {
                    "continue;".to_owned()
                } else {
                    "break;".to_owned()
                }
            }
            LoopKind::Value(ty) => {
                if self.rng.chance(40) {
                    "continue;".to_owned()
                } else {
                    // The branch diverges, so its moves do not reach the code after it;
                    // keeping them recorded is conservative and simpler.
                    let v = self.val_expr(ty, depth.saturating_sub(1));
                    format!("break {};", v.text)
                }
            }
            LoopKind::Index => "break;".to_owned(),
        };
        self.line(&format!("if {} {{ {jump} }}", c.text));
        true
    }

    fn stmt_return(&mut self, depth: u32) -> bool {
        let c = self.prim_expr(Prim::Bool, depth.saturating_sub(1));
        let Some(ret) = self.return_text(depth) else {
            return false;
        };
        self.line(&format!("if {} {{ {ret} }}", c.text));
        true
    }

    /// `return;` or `return value;` for the function's return type; `None` when a
    /// reference into the reference parameter cannot be returned now.
    fn return_text(&mut self, depth: u32) -> Option<String> {
        let value = match self.sig.ret {
            RetTy::Unit => String::new(),
            RetTy::Val(v) => {
                let e = self.val_expr(v, depth.saturating_sub(1));
                format!(" {}", e.text)
            }
            RetTy::Ref { mutable, inner } => {
                let p = self.vars.iter().position(|v| v.ty.is_ref());
                let free = p.is_some_and(|p| {
                    let v = &self.vars[p];
                    v.mut_locks == 0 && (!mutable || v.shared_locks == 0)
                });
                if !free && !self.violate() {
                    return None;
                }
                format!(" {}", self.ref_tail(mutable, inner)?)
            }
        };
        Some(format!("return{value};"))
    }

    /// `*F(&mut x) = e;`, `F(&mut x).Field op= e;` for a function returning `&mut T`.
    fn stmt_write_through_call(&mut self, depth: u32) -> bool {
        let fns = self.callables(|r| matches!(r, RetTy::Ref { mutable: true, .. }));
        if fns.is_empty() {
            return false;
        }
        let callee = *self.rng.pick(&fns);
        let RetTy::Ref { inner, .. } = self.prog.fns[callee].ret else {
            return false;
        };
        // A slice is only written element by element (`F(&mut a)[0] = e;`, an index that is
        // in bounds unless the slice is empty).
        let subs: Vec<(String, ValTy)> = self
            .prog
            .sub_places(inner)
            .into_iter()
            .filter(|(_, t)| self.prog.is_sized(*t))
            .map(|(path, t)| (path.replace("[?]", "[0]"), t))
            .collect();
        if subs.is_empty() {
            return false;
        }
        let (path, ty) = self.rng.pick(&subs).clone();
        // Right-hand side first, as Rust evaluates it.
        let compound = matches!(ty, ValTy::Prim(p) if p.is_numeric()) && self.rng.chance(40);
        let rhs = if compound {
            let ValTy::Prim(p) = ty else { unreachable!() };
            if p.is_int() && self.rng.chance(50) {
                let Prim::Int(i) = p else { unreachable!() };
                Expr::value(int_literal(i, i128::from(self.rng.range(1, 9))))
            } else {
                self.prim_expr(p, depth.saturating_sub(1))
            }
        } else {
            self.val_expr(ty, depth.saturating_sub(1))
        };
        let Some(call) = self.call(callee, depth) else {
            return false;
        };
        let op = if compound {
            if matches!(ty, ValTy::Prim(p) if p.is_int()) {
                *self.rng.pick(&["+=", "-=", "*=", "|=", "^="])
            } else {
                *self.rng.pick(&["+=", "-=", "*="])
            }
        } else {
            "="
        };
        let target = if path.is_empty() {
            format!("*{}", call.text)
        } else {
            format!("{}{path}", call.text)
        };
        self.line(&format!("{target} {op} {};", rhs.text));
        true
    }

    /// Prints a few of the live locals so that the computation is observable.
    fn final_prints(&mut self) {
        let mut printed = 0;
        let candidates: Vec<(String, ValTy, &str)> = self
            .vars
            .iter()
            .filter(|v| !v.hidden && !v.dead && !v.moved && v.mut_locks == 0)
            .map(|v| match v.ty {
                Ty::Val(t) => (v.name.clone(), t, ""),
                Ty::Ref { inner, .. } => (v.name.clone(), inner, "*"),
                Ty::RefRef { inner, .. } => (v.name.clone(), inner, "**"),
            })
            .collect();
        for (name, ty, derefs) in candidates {
            if printed >= 3 || self.remaining() == 0 {
                break;
            }
            if self.rng.chance(30) {
                continue;
            }
            if let ValTy::Enum(e) = ty {
                self.show_enum(&format!("&{derefs}{name}"), e);
            } else {
                let base = format!("{derefs}{name}");
                let line = self.show_line(&base, ty);
                self.line(&line);
            }
            printed += 1;
        }
        // `Main` always shows that it ran to the end (seed 110 printed nothing at all).
        if printed == 0 && self.sig.name == "Main" {
            self.line("PrintLine(\"end\");");
        }
    }
}

/// `text` as a method receiver: a plain path of names and indices (`p`, `p.A.X`, `a[1].X`,
/// `a[(k % 2usize)]`) as it is, anything else parenthesised (`(*r)`, `(t.0)`, `(F(x))`),
/// so that `*r.Get()` cannot read as `*(r.Get())` and a tuple index cannot run into the
/// method name.
fn receiver_text(text: &str) -> String {
    // Index expressions do not matter: `a[i].Get()` calls `Get` on the element.
    let mut names = String::new();
    let mut level = 0;
    for c in text.chars() {
        match c {
            '[' => level += 1,
            ']' => level -= 1,
            _ if level == 0 => names.push(c),
            _ => {}
        }
    }
    let plain = names.split('.').all(|segment| {
        segment
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    if plain {
        text.to_owned()
    } else {
        format!("({text})")
    }
}

#[cfg(test)]
mod tests {
    use super::{receiver_text, BodyGen, Ty, ValTy, INDENT};
    use crate::program::gen_program;
    use crate::rng::Rng;
    use crate::Knobs;

    /// Seed 906 (borrow-clean) moved a struct local, re-initialised it inside a `loop`
    /// after the loop's first `break`, and used it after the loop: rustc rejects that
    /// (E0382), since the local is maybe-moved there. A re-initialisation in a loop body or
    /// an `if` without `else` must not count as definite afterwards.
    #[test]
    fn reinitialisation_in_a_block_is_not_definite_afterwards() {
        let knobs = Knobs::borrow_clean();
        let (mut reinitialised, mut definite) = (0, 0);
        // 12000 seeds: with tuples (#6) and arrays (#9, whose elements are places too) in
        // the generator, a struct local re-initialised in both branches of an `if`/`else`
        // is rarer than in the 400 this test began with.
        for seed in 0..12000 {
            let mut rng = Rng::new(seed);
            let prog = gen_program(&mut rng, &knobs);
            let Some(s) = prog.structs.iter().position(|s| !s.copy) else {
                continue;
            };
            let mut gen = BodyGen::new(&prog, &knobs, &mut rng, 0, 1000);
            let name = "moved".to_owned();
            gen.used_names.push(name.clone());
            let v = gen.declare(name.clone(), Ty::Val(ValTy::Struct(s)), true, false);
            gen.vars[v].moved = true;
            // The block's own lines are at the current indentation, the statements of its
            // body (or branches) one level deeper; anything deeper is in a nested block.
            let outer = INDENT.repeat(gen.indent);
            let body_reinit = format!("{outer}{INDENT}{name} = ");
            let block = seed % 4;
            match block {
                0 => gen.stmt_while(2),
                1 => gen.stmt_loop(false),
                2 => gen.stmt_loop(true),
                _ => gen.stmt_if(2),
            };
            let reinit_at =
                |lines: &[String], prefix: &str| lines.iter().any(|l| l.starts_with(prefix));
            reinitialised += usize::from(reinit_at(&gen.lines, &body_reinit));
            // An `if` with an `else` whose branches both re-initialise at their top level
            // is definite, unless a branch uses the local again after its re-initialisation
            // (and may move it): the text does not tell, so such a block is skipped.
            let else_line = gen
                .lines
                .iter()
                .position(|l| *l == format!("{outer}}} else {{"));
            let is_name_char = |c: char| c.is_alphanumeric() || c == '_';
            let mentioned_after_reinit = |lines: &[String]| {
                lines
                    .iter()
                    .position(|l| l.starts_with(&body_reinit))
                    .is_some_and(|r| {
                        lines[r + 1..].iter().any(|l| {
                            l.match_indices(name.as_str()).any(|(at, _)| {
                                !l[..at].ends_with(is_name_char)
                                    && !l[at + name.len()..].starts_with(is_name_char)
                            })
                        })
                    })
            };
            // A re-initialisation in a nested `if`/`else` may make a branch without one at
            // its top level definite too (seed 1743), and so may a destructuring assignment
            // `(x, moved) = (..);` at any level (seed 775): the text does not tell either,
            // so such a block is skipped.
            let mentions = |text: &str| {
                text.match_indices(name.as_str()).any(|(at, _)| {
                    !text[..at].ends_with(is_name_char)
                        && !text[at + name.len()..].starts_with(is_name_char)
                })
            };
            let unknown_branch = |lines: &[String]| {
                lines.iter().any(|l| {
                    let l = l.trim_start();
                    l.starts_with('(') && l.split(") = (").next().is_some_and(mentions)
                }) || (!reinit_at(lines, &body_reinit)
                    && lines
                        .iter()
                        .any(|l| l.trim_start().starts_with(&format!("{name} = "))))
            };
            if block == 3
                && else_line.is_some_and(|e| {
                    mentioned_after_reinit(&gen.lines[..e])
                        || mentioned_after_reinit(&gen.lines[e..])
                        || unknown_branch(&gen.lines[..e])
                        || unknown_branch(&gen.lines[e..])
                })
            {
                continue;
            }
            let both_branches = block == 3
                && else_line.is_some_and(|e| {
                    reinit_at(&gen.lines[..e], &body_reinit)
                        && reinit_at(&gen.lines[e..], &body_reinit)
                });
            definite += usize::from(both_branches);
            assert_eq!(
                gen.vars[v].moved,
                !both_branches,
                "seed {seed}, block kind {block}:\n{}",
                gen.lines.join("\n")
            );
        }
        // The property is only exercised when a block re-initialises the local, and the
        // exception only when some `if`/`else` does so in both branches; if either count
        // drops to zero, the line matching above no longer fits the generator's layout.
        assert!(
            reinitialised >= 5,
            "only {reinitialised} blocks re-initialise the local"
        );
        assert!(
            definite > 0,
            "no `if`/`else` re-initialises in both branches"
        );
    }

    #[test]
    fn receivers_are_parenthesised_unless_plain_paths() {
        assert_eq!(receiver_text("p"), "p");
        assert_eq!(receiver_text("self.A.X"), "self.A.X");
        assert_eq!(receiver_text("*self"), "(*self)");
        assert_eq!(receiver_text("t.0"), "(t.0)");
        assert_eq!(receiver_text("F(x)"), "(F(x))");
        assert_eq!(receiver_text("a[1].X"), "a[1].X");
        assert_eq!(
            receiver_text("a[(F(x).0 % 2usize)]"),
            "a[(F(x).0 % 2usize)]"
        );
        assert_eq!(receiver_text("t.0[1]"), "(t.0[1])");
    }
}
