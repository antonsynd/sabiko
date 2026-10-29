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
        !self.clean() && self.rng.chance(u32::from(self.knobs.violation_percent))
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
                place.via_ref.is_none()
                    && root.shared_locks == 0
                    && root.loop_depth == self.loop_depth()
            }
            Access::Write | Access::Borrow(true) => {
                root.mut_locks == 0 && root.shared_locks == 0 && writable
            }
            Access::Borrow(false) => root.mut_locks == 0,
        }
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
        let typed: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| p.ty == ty)
            .collect();
        let (ok, bad): (Vec<Place>, Vec<Place>) =
            typed.into_iter().partition(|p| self.permitted(p, access));
        let mut pool = self.choose_pool(ok, bad)?;
        if let Some(root) = self.prefer_root {
            if pool.iter().any(|p| p.root == root) && self.rng.chance(60) {
                pool.retain(|p| p.root == root);
            }
        }
        let place = self.rng.pick(&pool).clone();
        self.apply(&place, access);
        Some(place)
    }

    /// A random place of any type with `access` allowed, chosen among `filter`ed types.
    /// The access is not recorded. The flag says whether the place was taken from the
    /// forbidden pool (a deliberate violation).
    fn any_place(
        &mut self,
        access: Access,
        filter: impl Fn(ValTy) -> bool,
    ) -> Option<(Place, bool)> {
        let typed: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| filter(p.ty))
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
            let Ty::Ref { mutable, inner } = p.ty else {
                continue;
            };
            let found = if i == 0 && method_syntax {
                let temporary = !returns_ref && !forced;
                self.receiver_ref(mutable, inner, depth.saturating_sub(1), temporary)
            } else {
                self.ref_expr(mutable, inner, depth.saturating_sub(1))
                    .map(|e| {
                        // A bare `&mut` local is reborrowed implicitly, two-phase.
                        let bare = matches!(e.roots[..], [(root, true)]
                            if e.text == self.vars[root].name
                                && matches!(self.vars[root].ty, Ty::Ref { mutable: true, .. }));
                        (e, mutable && bare)
                    })
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
            for (i, p) in sig.params.iter().enumerate() {
                if let Ty::Val(v) = p.ty {
                    let mut e = self.val_expr(v, depth.saturating_sub(1));
                    if i == 0 && method_syntax {
                        e.text = receiver_text(&e.text);
                    }
                    args[i] = Some(e);
                }
            }
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
                prog.structs[owner.strukt].name,
                sig.name,
                texts.join(", ")
            ),
            None => format!("{}({})", sig.name, texts.join(", ")),
        };
        Some(Expr { text, roots })
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
            if temporary && matches!(inner, ValTy::Struct(_)) {
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
                    (e, mutable)
                }),
                1 => self.ref_var(mutable, inner).map(|e| (e, mutable)),
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
        match ty {
            ValTy::Prim(p) => self.prim_expr(p, depth),
            ValTy::Struct(s) => self.struct_expr(s, depth),
            ValTy::Tuple(t) => self.tuple_expr(t, depth),
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
        match self
            .rng
            .weighted(&[35, 45, if consts.is_empty() { 0 } else { 20 }])
        {
            0 => self.literal(p),
            1 => match self.place_of(ValTy::Prim(p), Access::Read) {
                Some(place) => Expr::value(place.text),
                None => self.literal(p),
            },
            _ => Expr::value((*self.rng.pick(&consts)).to_owned()),
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
                let source = match self.rng.weighted(&[60, 10, 10, 10, 10]) {
                    0 => Prim::Int(*self.rng.pick(&IntTy::ALL)),
                    1 => Prim::F32,
                    2 => Prim::F64,
                    3 => Prim::Bool,
                    _ => Prim::Char,
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
        if self.rng.chance(20) {
            if let Some(e) = self.ref_compare(p, op, d) {
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
            let e = self.val_expr(f.ty, depth.saturating_sub(1));
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

    /// A reference local usable as `&inner` (or `&mut inner` when `mutable`).
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
            if v.hidden || v.dead || t != inner || (mutable && !m) {
                continue;
            }
            let free = !v.moved && v.mut_locks == 0 && (!mutable || v.shared_locks == 0);
            if free {
                ok.push(i);
            } else {
                bad.push(i);
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
    /// local, or a call returning such a reference. `None` when nothing in scope fits.
    fn ref_expr(&mut self, mutable: bool, inner: ValTy, depth: u32) -> Option<Expr> {
        let fns = self.callables(
            |r| matches!(r, RetTy::Ref { mutable: m, inner: t } if t == inner && (m || !mutable)),
        );
        let order =
            match self
                .rng
                .weighted(&[55, 25, if fns.is_empty() || depth == 0 { 0 } else { 20 }])
            {
                0 => [0, 1, 2],
                1 => [1, 0, 2],
                _ => [2, 0, 1],
            };
        for kind in order {
            let found = match kind {
                0 => self
                    .place_of(inner, Access::Borrow(mutable))
                    .map(|place| Expr {
                        text: format!("&{}{}", if mutable { "mut " } else { "" }, place.text),
                        roots: vec![(place.root, mutable)],
                    }),
                1 => self.ref_var(mutable, inner),
                _ => {
                    if fns.is_empty() || depth == 0 {
                        None
                    } else {
                        let callee = *self.rng.pick(&fns);
                        self.call(callee, depth)
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
        let options: Vec<String> = self
            .prog
            .sub_places(pinner)
            .into_iter()
            .filter(|(_, t)| *t == inner)
            .map(|(path, _)| {
                if path.is_empty() {
                    if pm == mutable {
                        name.clone()
                    } else {
                        format!("&*{name}")
                    }
                } else {
                    format!("&{}{name}{path}", if mutable { "mut " } else { "" })
                }
            })
            .collect();
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
            _ => {
                let c = self.prim_expr(Prim::Bool, 1);
                self.line(&format!("if {} {{ Panic(\"generated panic\"); }}", c.text));
                true
            }
        }
    }

    fn random_val_ty(&mut self) -> ValTy {
        if !self.prog.structs.is_empty() && self.rng.chance(25) {
            ValTy::Struct(self.rng.index(self.prog.structs.len()))
        } else if !self.prog.tuples.is_empty() && self.rng.chance(20) {
            ValTy::Tuple(self.rng.index(self.prog.tuples.len()))
        } else if self.rng.chance(30) {
            // The type of something already in scope, so the new local can interact with it.
            let places = self.candidates();
            if places.is_empty() {
                ValTy::Prim(random_prim(self.rng))
            } else {
                self.rng.pick(&places).ty
            }
        } else {
            ValTy::Prim(random_prim(self.rng))
        }
    }

    fn stmt_let(&mut self, depth: u32) -> bool {
        let ty = self.random_val_ty();
        let e = self.val_expr(ty, depth);
        let name = self.fresh_name();
        let mutable = self.rng.chance(50);
        let annotation = if self.rng.chance(60) {
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
    /// Rust 2021's default binding modes bind references to the elements, each a reference
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
        let inner = self.rng.pick(&places).ty;
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
        let annotation = if self.rng.chance(40) {
            format!(": {}", self.prog.ty_name(ty))
        } else {
            String::new()
        };
        self.line(&format!("let {name}{annotation} = {};", e.text));
        self.bind_ref(name, ty, &e);
        true
    }

    fn stmt_assign(&mut self, depth: u32) -> bool {
        let Some((place, violation)) = self.any_place(Access::Write, |_| true) else {
            return false;
        };
        // Rust evaluates the right-hand side first.
        let e = self.val_expr(place.ty, depth);
        if violation || self.permitted(&place, Access::Write) {
            self.apply(&place, Access::Write);
            self.line(&format!("{} = {};", place.text, e.text));
        } else {
            let name = self.fresh_name();
            self.line(&format!("let {name} = {};", e.text));
            self.declare(name, Ty::Val(place.ty), false, false);
        }
        true
    }

    fn stmt_compound(&mut self, depth: u32) -> bool {
        let Some((place, violation)) = self.any_place(
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

    /// Prints every primitive leaf of a struct in scope on one line.
    fn stmt_show(&mut self) -> bool {
        let Some((place, _)) = self.any_place(Access::Borrow(false), |t| {
            matches!(t, ValTy::Struct(_) | ValTy::Tuple(_))
        }) else {
            return false;
        };
        let line = self.show_line(&place.text, place.ty);
        self.line(&line);
        true
    }

    fn show_line(&self, base: &str, ty: ValTy) -> String {
        let leaves = self.prog.leaves(ty);
        let mut out = String::new();
        let last = leaves.len() - 1;
        for (i, (path, _)) in leaves.iter().enumerate() {
            let text = if path.is_empty() {
                base.to_owned()
            } else if let Some(stripped) = base.strip_prefix('*') {
                format!("{stripped}{path}")
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
        let ty = ValTy::Struct(owner.strukt);
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
        };
        self.line(&format!("if {} {{ {jump} }}", c.text));
        true
    }

    fn stmt_return(&mut self, depth: u32) -> bool {
        let c = self.prim_expr(Prim::Bool, depth.saturating_sub(1));
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
                    return false;
                }
                match self.ref_tail(mutable, inner) {
                    Some(t) => format!(" {t}"),
                    None => return false,
                }
            }
        };
        self.line(&format!("if {} {{ return{value}; }}", c.text));
        true
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
        let subs: Vec<(String, ValTy)> = self.prog.sub_places(inner).into_iter().collect();
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
        let candidates: Vec<(String, ValTy, bool)> = self
            .vars
            .iter()
            .filter(|v| !v.hidden && !v.dead && !v.moved && v.mut_locks == 0)
            .map(|v| match v.ty {
                Ty::Val(t) => (v.name.clone(), t, false),
                Ty::Ref { inner, .. } => (v.name.clone(), inner, true),
            })
            .collect();
        for (name, ty, is_ref) in candidates {
            if printed >= 3 || self.remaining() == 0 {
                break;
            }
            if self.rng.chance(30) {
                continue;
            }
            let base = if is_ref { format!("*{name}") } else { name };
            let line = self.show_line(&base, ty);
            self.line(&line);
            printed += 1;
        }
    }
}

/// `text` as a method receiver: a plain path of names (`p`, `p.A.X`) as it is, anything
/// else parenthesised (`(*r)`, `(t.0)`, `(F(x))`), so that `*r.Get()` cannot read as
/// `*(r.Get())` and a tuple index cannot run into the method name.
fn receiver_text(text: &str) -> String {
    let plain = text.split('.').all(|segment| {
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
        // 1600 seeds: with tuples in the generator (#6) a struct local re-initialised in
        // both branches of an `if`/`else` is rarer than in the 400 this test began with.
        for seed in 0..1600 {
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
            // is definite.
            let both_branches = block == 3
                && gen
                    .lines
                    .iter()
                    .position(|l| *l == format!("{outer}}} else {{"))
                    .is_some_and(|e| {
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
    }
}
