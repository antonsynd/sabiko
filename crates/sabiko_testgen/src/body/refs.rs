//! References to references (#11): `&&T`, `&&mut T`, `&mut &T` and `&mut &mut T` locals
//! and parameters, `&&str` and `&&[T]` among them.
//!
//! A double reference local borrows a reference local or parameter `r`: `let rr = &r;`,
//! or `let m = &mut r;` for a `let mut r = ..;` declared for the purpose (or a `mut`
//! `&str` local). It is a reference local of the scope model with a region like any other
//! (`bind_ref`): its loan is on `r` (shared or mutable), and it never outlives `r`'s
//! region. Through it, the places of the referent are read as `**rr`, by auto-deref
//! (`rr.X`, `rr[0]`, `rr.len()`) or with one or two explicit dereferences (`(*rr).X`,
//! `(**rr).X`) and, through a `&mut &mut T`, written (`**m = v`, `m.X += v`); they are
//! borrowed (`&**rr`, `&mut **m`), matched by value (`match **rr`) or through both
//! references with default binding modes (`match rr`, every binding a reference, a
//! top-level binding the double reference itself), and they are method receivers
//! (`rr.Get()`, `m.Bump(m.X)` as a two-phase borrow). `(*rr)` is a `&T` wherever one is
//! wanted (`(*ss).len()`, `(*ss) == "a"`). A `&mut &T` retargets `r` (`*m = &y;`, `*m =
//! "lit";`) and a `&mut &mut T` too (`*m = &mut y;`): `r` then also borrows `y` for the
//! rest of its region, so `y` must live in `r`'s block or an outer one, and `m` dies at
//! once, so that `r` is read again right after (`PrintLine(*r);`).
//!
//! Parameters of the types `&&T`, `&&mut T` and `&mut &mut T` take `&r`, `&mut r`, a
//! double reference local, or `&&x`, `& &mut x`, `&mut &mut x` of a place (a borrow of a
//! temporary that lives to the end of the call's statement; never of a literal, which
//! rustc promotes to a constant, #26). A function with a double reference parameter never
//! returns a reference unless it is a `&self` method: `fn F(rr: &&i32) -> &i32` has two
//! elided input lifetimes (rustc's E0106). `&&str` operands are compared (`ss == &s`), and
//! `&&p` ones of primitives (`rr < &r`).
//!
//! Not generated: TODO(#92) double references in tuples and arrays (struct fields and
//! enum payloads wait for lifetimes), as for single ones. TODO(#58): three levels (`&&&T`), `&mut &T`
//! parameters (a callee can only retarget them to what the caller lent it), retargets of
//! a reference local declared outside the loop the retarget is in (the new loan would be
//! held at the top of the next iteration, which the scope model does not follow), and
//! string patterns on a `&&str` scrutinee. Returned double references are not in the
//! subset at all: no elision gives `fn F(rr: &&i32) -> &&i32` or `-> &i32` a lifetime.

use super::matches::{Mode, Scrut};
use super::{Access, BodyGen, Expr, Place, Ranges};
use crate::program::str_literal;
use crate::ty::{Ty, ValTy};

impl BodyGen<'_> {
    /// The live double reference locals (and parameters) satisfying `pred` on their type.
    fn ref_ref_vars(&self, pred: impl Fn(bool, bool, ValTy) -> bool) -> Vec<usize> {
        (0..self.vars.len())
            .filter(|&v| {
                let var = &self.vars[v];
                !var.hidden
                    && !var.dead
                    && matches!(var.ty, Ty::RefRef { outer, mutable, inner }
                        if pred(outer, mutable, inner))
            })
            .collect()
    }

    /// Whether reference local `v` may be used (copied, reborrowed, borrowed) as a shared
    /// reference, or (`mutable`) a mutable one, now.
    fn ref_free(&self, v: usize, mutable: bool) -> bool {
        let var = &self.vars[v];
        !var.moved && var.mut_locks == 0 && (!mutable || var.shared_locks == 0)
    }

    /// Picks one of `vars` that is free for `mutable` use (or, with the violation knob, one
    /// that is not).
    fn pick_free(&mut self, vars: Vec<usize>, mutable: bool) -> Option<usize> {
        let (ok, bad): (Vec<usize>, Vec<usize>) =
            vars.into_iter().partition(|&v| self.ref_free(v, mutable));
        let pool = self.choose_pool(ok, bad)?;
        Some(*self.rng.pick(&pool))
    }

    /// `(*rr)`: a `&inner` read out of a double reference local whose inner reference is
    /// shared (a copy), for a `&inner` (or, coerced, a slice) wanted.
    pub(super) fn deref_ref_ref(&mut self, inner: ValTy) -> Option<Expr> {
        let prog = self.prog;
        let vars = self.ref_ref_vars(|_, m, t| !m && prog.ref_coerces(t, inner));
        let v = self.pick_free(vars, false)?;
        Some(Expr {
            text: format!("(*{})", self.vars[v].name),
            roots: vec![(v, false)],
        })
    }

    /// A double reference local as the receiver of a method on `inner` (`rr.Get()`,
    /// `m.Bump(..)`: auto-deref, then autoref); `&mut self` only through `&mut &mut`.
    pub(super) fn ref_ref_receiver(&mut self, mutable: bool, inner: ValTy) -> Option<Expr> {
        let vars = self.ref_ref_vars(|o, m, t| t == inner && (!mutable || (o && m)));
        let v = self.pick_free(vars, mutable)?;
        Some(Expr {
            text: self.vars[v].name.clone(),
            roots: vec![(v, mutable)],
        })
    }

    /// `ss` for a `&&str` local, as the base of `.len()` or a range (auto-deref).
    pub(super) fn ref_ref_str_base(&mut self) -> Option<Expr> {
        let vars = self.ref_ref_vars(|_, _, t| t == ValTy::Str);
        let v = self.pick_free(vars, false)?;
        Some(Expr {
            text: self.vars[v].name.clone(),
            roots: vec![(v, false)],
        })
    }

    /// An expression of type `&&inner` (`outer == false`) or `&mut &inner`, with the inner
    /// reference `&mut` when `mutable`: a double reference local of that type (passed
    /// bare: copied, or reborrowed where a `&mut &mut` is expected), a borrow of a
    /// reference local (`&r`, `&mut r`) or of a borrowed place (`&&x`, `& &mut x.A`, `&mut
    /// &mut a[1]`). The types are exact: `&&[T; N]` does not coerce to `&&[T]`.
    pub(super) fn ref_ref_expr(
        &mut self,
        outer: bool,
        mutable: bool,
        inner: ValTy,
    ) -> Option<Expr> {
        let first = self.rng.weighted(&[30, 40, 30]);
        for kind in (0..3).map(|k| (first + k) % 3) {
            let found = match kind {
                0 => {
                    let vars =
                        self.ref_ref_vars(|o, m, t| o == outer && m == mutable && t == inner);
                    self.pick_free(vars, outer).map(|v| Expr {
                        text: self.vars[v].name.clone(),
                        roots: vec![(v, outer)],
                    })
                }
                1 => {
                    let want = Ty::Ref { mutable, inner };
                    let vars: Vec<usize> = (0..self.vars.len())
                        .filter(|&v| {
                            let var = &self.vars[v];
                            !var.hidden && !var.dead && var.ty == want && (!outer || var.mutable)
                        })
                        .collect();
                    self.pick_free(vars, outer).map(|v| Expr {
                        text: format!("&{}{}", if outer { "mut " } else { "" }, self.vars[v].name),
                        roots: vec![(v, outer)],
                    })
                }
                _ if inner == ValTy::Str => {
                    // `&&*s`: a string place is only ever behind a `&str` local.
                    self.place_of(inner, Access::Borrow(false))
                        .map(|place| Expr {
                            text: format!("&{}&{}", if outer { "mut " } else { "" }, place.text),
                            roots: vec![(place.root, false)],
                        })
                }
                _ => self
                    .place_of(inner, Access::Borrow(mutable))
                    .map(|place| Expr {
                        text: format!(
                            "&{}&{}{}",
                            if outer { "mut " } else { "" },
                            if mutable { "mut " } else { "" },
                            place.text
                        ),
                        roots: vec![(place.root, mutable)],
                    }),
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// A `&&inner` comparison operand: a `&&inner` local, `&r` of a `&inner` local, or
    /// `&&x` of a place.
    pub(super) fn ref_ref_operand(&mut self, inner: ValTy) -> Option<Expr> {
        self.ref_ref_expr(false, false, inner)
    }

    /// `(a op b)` between two `&&inner` operands (which compares the referents), the left
    /// one's loans held while the right one is generated. `None` when no operand is in
    /// reach.
    pub(super) fn ref_ref_compare(&mut self, inner: ValTy, op: &str) -> Option<Expr> {
        let a = self.ref_ref_operand(inner)?;
        self.lock(&a.roots);
        let b = self.ref_ref_operand(inner);
        self.unlock(&a.roots);
        let b = b?;
        Some(Expr::value(format!("({} {op} {})", a.text, b.text)))
    }

    /// `match rr`: a double reference local as a scrutinee, matched with default binding
    /// modes through both references (bindings are `&mut` only through `&mut &mut`); a
    /// top-level binding binds (copies or moves) the double reference itself.
    pub(super) fn ref_ref_scrut(&mut self, ty: ValTy) -> Option<Scrut> {
        let vars = self.ref_ref_vars(|_, _, t| t == ty);
        if vars.is_empty() {
            return None;
        }
        let v = *self.rng.pick(&vars);
        let Ty::RefRef { outer, mutable, .. } = self.vars[v].ty else {
            unreachable!("a double reference")
        };
        let binds_mut = outer && mutable;
        if !self.ref_free(v, binds_mut) && !self.violate() {
            return None;
        }
        Some(Scrut {
            text: self.vars[v].name.clone(),
            ty,
            mode: Mode::Ref(binds_mut),
            place_root: Some(v),
            roots: vec![v],
            guard_lock: vec![(v, binds_mut)],
            can_move: false,
            can_ref: false,
            can_ref_mut: false,
            strict: None,
            top: Some(self.vars[v].ty),
        })
    }

    // ----- statements ----------------------------------------------------------------------

    /// `let mut r = <reference>;` for a double reference to borrow mutably: a `&T` or `&mut
    /// T` (a borrow, a reference local reborrowed, a call) or a `&str`. `None` (and nothing
    /// written) when no reference of the chosen type is in reach.
    fn fresh_mut_ref(&mut self, depth: u32) -> Option<usize> {
        let places = self.candidates();
        let str_wanted = places.is_empty() || self.rng.chance(20);
        let name = self.fresh_name();
        if str_wanted {
            let e = self.str_expr(depth.saturating_sub(1));
            let annotation = if self.rng.chance(50) { ": &str" } else { "" };
            self.line(&format!("let mut {name}{annotation} = {};", e.text));
            return Some(self.bind_str(name, true, &e));
        }
        let inner = self.rng.pick(&places).ty;
        if inner == ValTy::Str {
            return None;
        }
        let mutable = self.rng.chance(50);
        let mut e = self.ref_expr(mutable, inner, depth)?;
        // As in `stmt_let_ref`: a bare reference local is reborrowed explicitly.
        if let [(root, _)] = e.roots[..] {
            if e.text == self.vars[root].name && self.vars[root].ty.is_ref() {
                e.text = format!("&{}*{}", if mutable { "mut " } else { "" }, e.text);
            }
        }
        let ty = Ty::Ref { mutable, inner };
        let annotation = if !self.prog.is_sized(inner) || self.rng.chance(40) {
            format!(": {}", self.prog.ty_name(ty))
        } else {
            String::new()
        };
        self.line(&format!("let mut {name}{annotation} = {};", e.text));
        let v = self.bind_ref(name, ty, &e);
        self.vars[v].mutable = true;
        Some(v)
    }

    /// `let rr = &r;` or `let m = &mut r;` for a reference local or parameter `r` (a fresh
    /// `let mut` one now and then), then, through a `&mut`, now and then a retarget of `r`
    /// or a write through both references.
    pub(super) fn stmt_let_ref_ref(&mut self, depth: u32) -> bool {
        if self.rng.chance(15) {
            return self.stmt_let_ref_ref_of_place();
        }
        let existing: Vec<usize> = (0..self.vars.len())
            .filter(|&v| {
                let var = &self.vars[v];
                matches!(var.ty, Ty::Ref { .. }) && !var.hidden && !var.dead && !var.moved
            })
            .collect();
        let fresh = existing.is_empty() || self.rng.chance(45);
        let r = if fresh {
            match self.fresh_mut_ref(depth) {
                Some(r) => r,
                None => return false,
            }
        } else {
            *self.rng.pick(&existing)
        };
        let Ty::Ref { mutable, inner } = self.vars[r].ty else {
            unreachable!("a reference")
        };
        // A `&str` derived from `s` (`&s[1..]`) carries no lock in the scope model, and
        // needs none: a loan through a shared reference does not conflict with borrowing
        // the reference itself (#56, seed 5001324), so `&mut s` is fine while it lives.
        let outer = self.vars[r].mutable && self.rng.chance(if fresh { 70 } else { 50 });
        if !self.ref_free(r, outer) && !self.violate() {
            // A fresh local is free; it stays declared.
            return fresh;
        }
        let name = self.fresh_name();
        let ty = Ty::RefRef {
            outer,
            mutable,
            inner,
        };
        let annotation = if self.rng.chance(35) {
            format!(": {}", self.prog.ty_name(ty))
        } else {
            String::new()
        };
        let text = format!("&{}{}", if outer { "mut " } else { "" }, self.vars[r].name);
        self.line(&format!("let {name}{annotation} = {text};"));
        let e = Expr {
            text,
            roots: vec![(r, outer)],
        };
        let v = self.bind_ref(name, ty, &e);
        if outer && self.rng.chance(65) {
            self.through_mut_ref_ref(v, r, depth);
        } else if self.rng.chance(40) {
            self.read_through(v);
        }
        true
    }

    /// `let rr = &&x;` (`& &mut x`, `&mut &mut x`; the borrowed temporary lives as long
    /// as `rr`), or a copy or reborrow of a double reference local (`let r2 = rr;`, `let
    /// m2 = &mut *m;`): a double reference of a place's type.
    fn stmt_let_ref_ref_of_place(&mut self) -> bool {
        let prog = self.prog;
        let places: Vec<ValTy> = self
            .candidates()
            .into_iter()
            .map(|p| p.ty)
            .filter(|&t| prog.is_sized(t) || t == ValTy::Str)
            .collect();
        if places.is_empty() {
            return false;
        }
        let inner = *self.rng.pick(&places);
        let (outer, mutable) = match self.rng.weighted(&[50, 20, 10, 20]) {
            _ if inner == ValTy::Str => (false, false),
            0 => (false, false),
            1 => (false, true),
            2 => (true, false),
            _ => (true, true),
        };
        let Some(mut e) = self.ref_ref_expr(outer, mutable, inner) else {
            return false;
        };
        // `let m2 = m;` would move a `&mut &T` local: it is reborrowed explicitly.
        if let [(root, _)] = e.roots[..] {
            if outer && e.text == self.vars[root].name {
                e.text = format!("&mut *{}", e.text);
            }
        }
        let ty = Ty::RefRef {
            outer,
            mutable,
            inner,
        };
        let name = self.fresh_name();
        let annotation = if self.rng.chance(35) {
            format!(": {}", self.prog.ty_name(ty))
        } else {
            String::new()
        };
        self.line(&format!("let {name}{annotation} = {};", e.text));
        let v = self.bind_ref(name, ty, &e);
        if self.rng.chance(50) {
            self.read_through(v);
        }
        true
    }

    /// Prints the referent through the double reference `v` (`PrintLine(**rr);`,
    /// `PrintLine(rr.X);`, `PrintLine(ss.len());`).
    fn read_through(&mut self, v: usize) {
        let Ty::RefRef { inner, .. } = self.vars[v].ty else {
            return;
        };
        if matches!(inner, ValTy::Enum(_)) {
            return;
        }
        let line = self.show_line(&format!("**{}", self.vars[v].name), inner);
        self.line(&line);
    }

    /// After `let m = &mut r;`: retargets `r` (`*m = &y;`, `*m = "lit";`, `*m = &mut y;`)
    /// and reads `r` again, or writes through both references (`**m = v;`).
    fn through_mut_ref_ref(&mut self, m: usize, r: usize, depth: u32) {
        let Ty::RefRef { mutable, inner, .. } = self.vars[m].ty else {
            return;
        };
        let m_name = self.vars[m].name.clone();
        if mutable && self.prog.is_sized(inner) && self.rng.chance(70) {
            let e = self.typed_expr(inner, depth.saturating_sub(1));
            if self.vars[m].moved {
                // The value moved `m` (`match m { x => .. }` binds it whole; seed 18301027):
                // no write through it after that.
                let name = self.fresh_name();
                let ty = self.prog.val_name(inner);
                self.line(&format!("let {name}: {ty} = {};", e.text));
                self.declare(name, Ty::Val(inner), false, false);
            } else {
                self.line(&format!("**{m_name} = {};", e.text));
            }
            return;
        }
        // A loan stored into `r` inside a loop that `r` was declared outside of would still
        // be held at the top of the next iteration, which the scope model does not follow.
        let target = if self.vars[r].loop_depth != self.loop_depth() {
            None
        } else if inner == ValTy::Str {
            Some(Expr::value(str_literal(self.rng).0))
        } else {
            self.retarget_place(r, mutable, inner).map(|place| Expr {
                text: format!("&{}{}", if mutable { "mut " } else { "" }, place.text),
                roots: vec![(place.root, mutable)],
            })
        };
        let Some(target) = target else {
            self.read_through(m);
            return;
        };
        self.line(&format!("*{m_name} = {};", target.text));
        // `r` now also borrows the new target, for the rest of its region; `m` is done.
        self.lock(&target.roots);
        self.vars[r].borrows.extend(target.roots.iter().copied());
        self.kill(m);
        if self.rng.chance(60) {
            let r_name = self.vars[r].name.clone();
            let line = match inner {
                ValTy::Str => format!("PrintLine({r_name});"),
                ValTy::Enum(_) => return,
                _ => self.show_line(&format!("*{r_name}"), inner),
            };
            self.line(&line);
        }
    }

    /// A place of type `inner` that reference local `r` may be retargeted to: a local's
    /// own place (not through a reference, whose loan `r` would have to outlive), declared
    /// in `r`'s block or an outer one, borrowable shared (or, `mutable`, mutably) now.
    fn retarget_place(&mut self, r: usize, mutable: bool, inner: ValTy) -> Option<Place> {
        let block = self.vars[r].block;
        let access = Access::Borrow(mutable);
        let ok: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| {
                p.ty == inner
                    && p.via_ref.is_none()
                    && p.root != r
                    && self.vars[p.root].block <= block
                    && self.vars[p.root].ty.as_val().is_some()
                    && self.permitted(p, access)
            })
            .collect();
        if ok.is_empty() {
            return None;
        }
        let mut place = self.rng.pick(&ok).clone();
        self.finish_place(&mut place, Ranges::No);
        Some(place)
    }
}
