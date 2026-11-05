//! Arrays and slices (#9): array literals and `[x; N]` repeats, element places with
//! constant and computed indices, `.len()`, and `while i < s.len()` loops.
//!
//! Element places come from [`Program::sub_places`](crate::program::Program::sub_places):
//! constant indices into arrays (`a[1].X`, never out of bounds) and the placeholder `[?]`
//! for a slice's elements. [`BodyGen::finish_place`] replaces every placeholder, and now and
//! then a constant index, with an index expression: usually in bounds (`(e % 3usize)`,
//! `(s.len() - 1usize)`), sometimes not (a `usize` local, a cast, `e + 1usize`, the length
//! itself), so that rustc's `index out of bounds: the len is N but the index is M` panic
//! and the overflow panics of index arithmetic are compared at their locations. While an
//! index expression is generated, the place's root is held as shared, so the index may
//! read the array (`a[a[0] as usize]`) but not borrow it mutably, write or move it.
//!
//! The scope model treats an element like a field, except that nothing is ever moved out
//! of one (rustc's E0508; the violation knob does it now and then). Unsized slices only
//! appear behind references: borrowed (`&*s`), indexed and measured, never read or written
//! as a whole. A reference to an array coerces to a slice of its elements where the
//! expected type is known (call arguments, annotated `let`s, return values); such an
//! implicit reborrow of a `&mut [T; N]` local is not two-phase (`F(m, m[0])` is E0503 in
//! rustc when `F` takes `&mut [T]`), see `BodyGen::call`.

use super::{Access, BodyGen, Expr, LoopKind, Place};
use crate::program::{int_literal, ConstValue, RetTy};
use crate::ty::{IntTy, Prim, Ty, ValTy};

const USIZE: Prim = Prim::Int(IntTy::Usize);

impl BodyGen<'_> {
    /// An expression of the array type `a` (sized): a literal, a repeat, a place, a call,
    /// an `if` or a dereferenced call. `typed` when the expected type is known (an
    /// annotated `let`, an argument, a field), where an empty array may be written `[]`.
    pub(super) fn array_expr(&mut self, a: usize, depth: u32, typed: bool) -> Expr {
        let ty = ValTy::Array(a);
        let has_calls = !self.callables(|r| r == RetTy::Val(ty)).is_empty();
        let copy = self.prog.is_copy(Ty::Val(ty));
        let choice = if depth == 0 {
            self.rng.weighted(&[40, 30, 30, 0, 0, 0])
        } else {
            self.rng.weighted(&[
                32,
                22,
                24,
                if has_calls { 12 } else { 0 },
                8,
                if copy { 5 } else { 0 },
            ])
        };
        match choice {
            0 => self.array_literal(a, depth, typed),
            1 => self.array_repeat(a, depth, typed),
            2 => match self.place_of(ty, Access::Read) {
                Some(place) => Expr::value(place.text),
                None => self.array_literal(a, depth, typed),
            },
            3 => self
                .value_call(ty, depth)
                .unwrap_or_else(|| self.array_literal(a, depth, typed)),
            4 => self.if_expr(ty, depth),
            _ => self
                .deref_call(ty, depth)
                .unwrap_or_else(|| self.array_literal(a, depth, typed)),
        }
    }

    /// `[a, b, c]`, elements evaluated left to right; an empty one is `[]` where the type
    /// is known, else a repeat of length zero.
    pub(super) fn array_literal(&mut self, a: usize, depth: u32, typed: bool) -> Expr {
        let def = &self.prog.arrays[a];
        let (elem, len) = (def.elem, def.len.expect("an array literal has a length"));
        if len == 0 {
            if typed && self.rng.chance(60) {
                return Expr::value("[]".to_owned());
            }
            return self.array_repeat(a, depth, typed);
        }
        let elems: Vec<String> = (0..len)
            .map(|_| self.typed_expr(elem, depth.saturating_sub(1)).text)
            .collect();
        Expr::value(format!("[{}]", elems.join(", ")))
    }

    /// `[x; N]`: only for a `Copy` element or a length of zero or one (rustc's E0277
    /// otherwise). Zero drops `x` at once. The length is spelled as the type spells it, as
    /// a literal or as a constant of that value.
    fn array_repeat(&mut self, a: usize, depth: u32, typed: bool) -> Expr {
        let def = &self.prog.arrays[a];
        let (elem, len) = (def.elem, def.len.expect("a repeat has a length"));
        if len >= 2 && !self.prog.is_copy(Ty::Val(elem)) {
            return self.array_literal(a, depth, typed);
        }
        let x = self.val_expr(elem, depth.saturating_sub(1));
        let n = self.len_spelling(a);
        Expr::value(format!("[{}; {n}]", x.text))
    }

    /// A spelling of the length of array type `a`: the type's own, a literal, or a `usize`
    /// constant of that value.
    fn len_spelling(&mut self, a: usize) -> String {
        let prog = self.prog;
        let def = &prog.arrays[a];
        let len = def.len.expect("sized");
        let consts: Vec<&str> = prog
            .consts
            .iter()
            .filter(|c| {
                c.ty == USIZE && c.value == ConstValue::Int(i128::try_from(len).expect("small"))
            })
            .map(|c| c.name.as_str())
            .collect();
        match self
            .rng
            .weighted(&[40, 35, 10, if consts.is_empty() { 0 } else { 25 }])
        {
            0 => def.len_text.clone(),
            1 => len.to_string(),
            2 => format!("{len}usize"),
            _ => (*self.rng.pick(&consts)).to_owned(),
        }
    }

    /// `val_expr` where the expected type is known.
    pub(super) fn typed_expr(&mut self, ty: ValTy, depth: u32) -> Expr {
        match ty {
            ValTy::Array(a) if self.prog.arrays[a].len == Some(0) => {
                self.array_expr(a, depth, true)
            }
            _ => self.val_expr(ty, depth),
        }
    }

    /// Replaces the placeholder of every slice index in `place` (`s[?]`), and now and then
    /// a constant array index, with an index expression (made of leaves only, without
    /// calls, while `simple` is set, as in guards). The root is held as shared meanwhile.
    pub(super) fn finish_place(&mut self, place: &mut Place) {
        if !place.text.contains('[') {
            return;
        }
        let lock = [(place.root, false)];
        self.lock(&lock);
        let text = place.text.clone();
        let mut out = String::new();
        let mut lens = place.lens.iter().copied();
        let mut rest = text.as_str();
        while let Some(open) = rest.find('[') {
            out.push_str(&rest[..open]);
            let close = open + rest[open..].find(']').expect("a closing bracket");
            let inner = &rest[open + 1..close];
            let len = lens.next().expect("a length per index");
            let index = if inner == "?" || self.rng.chance(20) {
                self.index_expr(&out, len)
            } else {
                inner.to_owned()
            };
            out.push('[');
            out.push_str(&index);
            out.push(']');
            rest = &rest[close + 1..];
        }
        out.push_str(rest);
        self.unlock(&lock);
        place.text = out;
    }

    /// An index into `base` (an array of `len` elements, or a slice): usually in bounds,
    /// sometimes out of bounds or overflowing on the way.
    fn index_expr(&mut self, base: &str, len: Option<usize>) -> String {
        let len_text = match len {
            Some(n) => int_literal(IntTy::Usize, i128::try_from(n).expect("small")),
            None => format!("{}.len()", len_receiver(base)),
        };
        let nonempty = len.is_some_and(|n| n > 0);
        match self.rng.weighted(&[
            if nonempty { 24 } else { 0 },
            30,
            8,
            if nonempty { 4 } else { 12 },
            5,
            4,
            6,
        ]) {
            0 => {
                let k = self.rng.index(len.expect("an array"));
                if self.rng.chance(20) {
                    format!("{k}usize")
                } else {
                    k.to_string()
                }
            }
            1 => {
                let e = self.index_operand();
                format!("({e} % {len_text})")
            }
            // Any `usize` value: a place, a constant or a literal, often out of bounds.
            2 => self.prim_leaf(USIZE).text,
            // The last element, or an overflow for an empty one.
            3 => format!("({len_text} - 1usize)"),
            4 => {
                // A cast from another integer type: a negative value wraps to a huge index.
                let from = *self.rng.pick(&IntTy::ALL);
                let e = self.prim_leaf(Prim::Int(from)).text;
                if self.rng.chance(60) {
                    format!("(({e} as usize) % {len_text})")
                } else {
                    format!("({e} as usize)")
                }
            }
            // Surely out of bounds.
            5 => {
                if self.rng.chance(50) {
                    len_text
                } else {
                    format!("({len_text} + 1usize)")
                }
            }
            // Arithmetic that may overflow or leave the bounds.
            _ => {
                let e = self.index_operand();
                let op = *self.rng.pick(&["+", "-"]);
                format!("({e} {op} 1usize)")
            }
        }
    }

    fn index_operand(&mut self) -> String {
        if self.simple > 0 {
            self.prim_leaf(USIZE).text
        } else {
            self.prim_expr(USIZE, 1).text
        }
    }

    /// `x.len()` for a place `x` of array or slice type that may be borrowed (or, with the
    /// violation knob, one that may not).
    pub(super) fn len_expr(&mut self) -> Option<Expr> {
        let prog = self.prog;
        let (place, _) = self.any_place(Access::Borrow(false), |t| prog.array_of(t).is_some())?;
        Some(Expr::value(format!("{}.len()", len_receiver(&place.text))))
    }

    /// `let mut i: usize = 0usize; while i < x.len() { ..x[i].. ; i += 1usize; }` over an
    /// array or slice place `x`: the body starts with a read, write or compound assignment
    /// of `x[i]` and ends with the increment (so no `continue` may skip it).
    pub(super) fn stmt_index_loop(&mut self) -> bool {
        let prog = self.prog;
        let sized: Vec<usize> = (0..prog.arrays.len())
            .filter(|&a| prog.arrays[a].len.is_some())
            .collect();
        let in_scope = self
            .candidates()
            .iter()
            .any(|p| prog.array_of(p.ty).is_some());
        if !sized.is_empty() && (!in_scope || self.rng.chance(35)) {
            // A fresh array to walk.
            let ty = ValTy::Array(*self.rng.pick(&sized));
            let e = self.typed_expr(ty, self.knobs.max_expr_depth.saturating_sub(1));
            let name = self.fresh_name();
            self.line(&format!(
                "let mut {name}: {} = {};",
                prog.val_name(ty),
                e.text
            ));
            self.declare(name, Ty::Val(ty), true, false);
        }
        // The condition runs on every iteration and the body writes the place again, so its
        // index expressions are leaves.
        let found =
            self.simply(|gen| gen.any_place(Access::Borrow(false), |t| prog.array_of(t).is_some()));
        let Some((place, _)) = found else {
            return false;
        };
        let (elem, _) = prog.array_of(place.ty).expect("an array");
        let base = len_receiver(&place.text);
        let i = self.fresh_name();
        self.line(&format!("let mut {i}: usize = 0usize;"));
        self.declare(i.clone(), Ty::prim(USIZE), true, true);
        let before = self.snapshot_moved();
        self.loops.push(LoopKind::Index);
        self.line(&format!("while {i} < {base}.len() {{"));
        self.open_block();
        self.index_loop_element(&place, &format!("{base}[{i}]"), elem);
        let n = self.rng.range(0, 2);
        for _ in 0..n {
            if self.remaining() <= 3 {
                break;
            }
            self.stmt();
        }
        self.line(&format!("{i} += 1usize;"));
        self.close_block();
        self.loops.pop();
        self.join_moved(&before);
        self.line("}");
        true
    }

    /// The first statement of an index loop over `place`: a print, a write or a compound
    /// assignment of `element` (`x[i]`, of type `elem`), or a copy of it into a new local.
    fn index_loop_element(&mut self, place: &Place, element: &str, elem: ValTy) {
        let prog = self.prog;
        let element_place = Place {
            text: element.to_owned(),
            ty: elem,
            whole: false,
            no_move: true,
            lens: Vec::new(),
            ..place.clone()
        };
        let writable = self.permitted(&element_place, Access::Write);
        let numeric: Vec<(String, Prim)> = prog
            .leaves(elem)
            .into_iter()
            .filter(|(_, p)| p.is_numeric())
            .collect();
        let copy = prog.is_copy(Ty::Val(elem));
        match self.rng.weighted(&[
            40,
            if writable { 25 } else { 0 },
            if writable && !numeric.is_empty() {
                25
            } else {
                0
            },
            if copy { 15 } else { 0 },
        ]) {
            0 => {
                if let ValTy::Enum(e) = elem {
                    if self.remaining() > 6 {
                        self.show_enum(&format!("&{element}"), e);
                    }
                } else {
                    let line = self.show_line(element, elem);
                    self.line(&line);
                }
            }
            1 => {
                let e = self.val_expr(elem, self.knobs.max_expr_depth.saturating_sub(1));
                self.line(&format!("{element} = {};", e.text));
            }
            2 => {
                let (path, p) = self.rng.pick(&numeric).clone();
                let op = if p.is_int() {
                    *self.rng.pick(&["+=", "-=", "*=", "^=", "|="])
                } else {
                    *self.rng.pick(&["+=", "-=", "*="])
                };
                let e = self.prim_expr(p, 1);
                self.line(&format!("{element}{path} {op} {};", e.text));
            }
            _ => {
                let name = self.fresh_name();
                self.line(&format!("let {name} = {element};"));
                self.declare(name, Ty::Val(elem), false, false);
            }
        }
    }
}

/// The receiver of `.len()` for a place: `*s` is written `s` (auto-deref).
pub(super) fn len_receiver(text: &str) -> String {
    match text.strip_prefix('*') {
        Some(name) => name.to_owned(),
        None => text.to_owned(),
    }
}
