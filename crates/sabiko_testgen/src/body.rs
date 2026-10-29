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
    float_literal, int_literal, random_literal, random_prim, unique_name, FnSig, Program, RetTy,
    LOCAL_NAMES,
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
    /// Locks held until the end of the current statement (see `deref_call` and `call`); a
    /// variable with one is excluded from every candidate list, violation knob or not.
    stmt_locks: u32,
    /// Read in the current statement (see `apply`): no `&mut` borrow or write of it may
    /// follow in the same statement, violation knob or not.
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
    /// Variables whose `stmt_locks` must be released when the statement ends.
    stmt_locked: Vec<usize>,
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
            stmt_locked: Vec::new(),
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
                stmt_locks: 0,
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
            stmt_locks: 0,
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
        self.stmt_locked.retain(|&v| v < block.first_var);
        self.vars.truncate(block.first_var);
        self.indent -= 1;
    }

    // ----- places --------------------------------------------------------------------

    fn candidates(&self) -> Vec<Place> {
        let mut out = Vec::new();
        for (i, var) in self.vars.iter().enumerate() {
            if var.hidden || var.dead || var.stmt_locks > 0 {
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

    /// Accesses that are never generated, violation knob or not: see the TODO in `call`.
    fn hard_blocked(&self, place: &Place, _access: Access) -> bool {
        self.vars[place.root].stmt_locks > 0
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
            .filter(|p| p.ty == ty && !self.hard_blocked(p, access))
            .collect();
        let (ok, bad): (Vec<Place>, Vec<Place>) =
            typed.into_iter().partition(|p| self.permitted(p, access));
        let pool = self.choose_pool(ok, bad)?;
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
            .filter(|p| filter(p.ty) && !self.hard_blocked(p, access))
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

    /// A call to function `callee`. Reference arguments are chosen first and their loans
    /// held while the remaining arguments are generated, which forbids `F(&mut x, x.A)`
    /// (an error in both languages) and, conservatively, `F(x.A, &mut x)` (fine in both).
    fn call(&mut self, callee: usize, depth: u32) -> Option<Expr> {
        let prog = self.prog;
        let sig = &prog.fns[callee];
        let mut args: Vec<Option<Expr>> = vec![None; sig.params.len()];
        let mut held: Vec<(usize, bool)> = Vec::new();
        let mut result_roots = Vec::new();
        let mut ok = true;
        for (i, p) in sig.params.iter().enumerate() {
            if let Ty::Ref { mutable, inner } = p.ty {
                let Some(e) = self.ref_expr(mutable, inner, depth.saturating_sub(1)) else {
                    ok = false;
                    break;
                };
                self.lock(&e.roots);
                held.extend(e.roots.iter().copied());
                result_roots.extend(e.roots.iter().copied());
                if let [(root, _)] = e.roots[..] {
                    if e.text == self.vars[root].name {
                        // TODO(gen-bugs/README.md #4): a bare `&mut` local passed as an
                        // argument is an implicit reborrow, which rustc treats as a
                        // two-phase borrow (later arguments may read through it) and
                        // Sabiko does not. Until that is fixed the local is unusable for
                        // the rest of the statement, whatever the violation knob says.
                        self.hold_for_stmt(root);
                    }
                }
                args[i] = Some(e);
            }
        }
        if ok {
            for (i, p) in sig.params.iter().enumerate() {
                if let Ty::Val(v) = p.ty {
                    args[i] = Some(self.val_expr(v, depth.saturating_sub(1)));
                }
            }
        }
        self.unlock(&held);
        if !ok {
            return None;
        }
        let texts: Vec<String> = args.into_iter().map(|a| a.expect("filled").text).collect();
        let roots = if matches!(sig.ret, RetTy::Ref { .. }) {
            result_roots
        } else {
            Vec::new()
        };
        Some(Expr {
            text: format!("{}({})", sig.name, texts.join(", ")),
            roots,
        })
    }

    fn val_expr(&mut self, ty: ValTy, depth: u32) -> Expr {
        match ty {
            ValTy::Prim(p) => self.prim_expr(p, depth),
            ValTy::Struct(s) => self.struct_expr(s, depth),
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
            if v.hidden || v.dead || v.stmt_locks > 0 || t != inner || (mutable && !m) {
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
            self.release_stmt_locks();
        }
        if !done {
            let e = self.prim_leaf(Prim::Int(IntTy::I32));
            self.line(&format!("PrintLine({});", e.text));
        }
        self.release_stmt_locks();
        let block = self.cur_block();
        self.blocks[block].stmt_index += 1;
    }

    fn release_stmt_locks(&mut self) {
        for v in std::mem::take(&mut self.stmt_locked) {
            self.vars[v].stmt_locks -= 1;
        }
    }

    fn hold_for_stmt(&mut self, root: usize) {
        self.vars[root].stmt_locks += 1;
        self.stmt_locked.push(root);
    }

    #[allow(clippy::too_many_lines)]
    fn try_stmt(&mut self) -> bool {
        let depth = self.knobs.max_expr_depth;
        let nesting_ok = self.blocks.len() <= self.knobs.max_block_depth && self.remaining() > 6;
        let in_loop = !self.loops.is_empty();
        let has_fns = self.fn_index > 0;
        let returns = self.sig.ret != RetTy::Unit;
        let weights = [
            18,                             // 0 let
            10,                             // 1 let ref
            12,                             // 2 assign
            10,                             // 3 compound
            16,                             // 4 print
            6,                              // 5 show struct
            if has_fns { 10 } else { 0 },   // 6 call statement
            if nesting_ok { 9 } else { 0 }, // 7 if
            if nesting_ok { 6 } else { 0 }, // 8 while
            if nesting_ok { 4 } else { 0 }, // 9 loop
            if nesting_ok { 4 } else { 0 }, // 10 let = loop { break v }
            if in_loop { 7 } else { 0 },    // 11 guarded break/continue
            if returns { 3 } else { 1 },    // 12 guarded return
            if has_fns { 5 } else { 0 },    // 13 write through a returned reference
            1,                              // 14 guarded panic
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
        if !self.hard_blocked(&place, Access::Write)
            && (violation || self.permitted(&place, Access::Write))
        {
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
        if !self.hard_blocked(&place, Access::Write)
            && (violation || (!moved && self.permitted(&place, Access::Write)))
        {
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
        let Some((place, _)) =
            self.any_place(Access::Borrow(false), |t| matches!(t, ValTy::Struct(_)))
        else {
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

#[cfg(test)]
mod tests {
    use super::{BodyGen, Ty, ValTy};
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
        let mut reinitialised = 0;
        for seed in 0..400 {
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
            let block = seed % 4;
            match block {
                0 => gen.stmt_while(2),
                1 => gen.stmt_loop(false),
                2 => gen.stmt_loop(true),
                _ => gen.stmt_if(2),
            };
            let reinit = |lines: &[String]| {
                lines
                    .iter()
                    .any(|l| l.trim_start().starts_with(&format!("{name} = ")))
            };
            reinitialised += usize::from(reinit(&gen.lines));
            // An `if` with an `else` whose branches both re-initialise at their top level
            // (not in a nested block) is definite.
            let top_level = |lines: &[String]| {
                lines
                    .iter()
                    .any(|l| l.starts_with(&format!("        {name} = ")))
            };
            let both_branches = gen
                .lines
                .iter()
                .position(|l| l == "    } else {")
                .is_some_and(|e| top_level(&gen.lines[..e]) && top_level(&gen.lines[e..]));
            assert_eq!(
                gen.vars[v].moved,
                !both_branches,
                "seed {seed}, block kind {block}:\n{}",
                gen.lines.join("\n")
            );
        }
        // The property is only exercised when a block re-initialises the local.
        assert!(
            reinitialised >= 5,
            "only {reinitialised} re-initialisations"
        );
    }
}
