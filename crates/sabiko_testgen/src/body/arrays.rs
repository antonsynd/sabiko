//! Arrays and slices (#9): array literals and `[x; N]` repeats, element places with
//! constant and computed indices, `.len()`, and `while i < s.len()` loops. Range indexing
//! (#10): `a[lo..hi]` and the other five forms as range places (`&a[1..]`, `&mut s[..k]`,
//! `a[1..][i] = v`, `a[..2].len()`), with constant bounds and computed ones, sometimes out
//! of range or overflowing.
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
//!
//! A range place is an `Index::index` call on its base, or an `IndexMut::index_mut` call
//! where it is written or borrowed mutably. The mutable call borrows the base before its
//! bounds and any later index are evaluated, with no two-phase borrow, so those must not
//! read the base (`&mut a[a.len() - 1..]` and `a[1..][a[0] as usize] = v` are E0502/E0503
//! in rustc, unlike the built-in `a[a.len() - 1] = v`): see [`Ranges`]. For the same
//! reason a `&mut self` method call on an element of a range place is not two-phase
//! (`a[1..][0].Bump(a[0].X)` is E0503), see `BodyGen::receiver_ref`.

use super::{Access, BodyGen, Expr, LoopKind, Place};
use crate::program::{int_literal, ConstValue, RetTy};
use crate::ty::{IntTy, Prim, Ty, ValTy};

const USIZE: Prim = Prim::Int(IntTy::Usize);

/// Whether, and how, [`BodyGen::finish_place`] may put range indices into a place (#10):
/// before an index segment, so the place keeps its type (`a[i]` becomes `a[1..][i]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ranges {
    /// Not at all: a scrutinee, whose patterns decide how the place is used.
    No,
    /// The place is read or borrowed shared: an `Index::index` call, whose bounds and
    /// later indices may read the base.
    Shared,
    /// The place is written or borrowed mutably: an `IndexMut::index_mut` call, whose
    /// bounds and later indices must not touch the base (its root is held as mutably
    /// borrowed meanwhile).
    Mut,
}

impl Ranges {
    pub(super) fn of(access: Access) -> Self {
        match access {
            Access::Write | Access::Borrow(true) => Ranges::Mut,
            Access::Read | Access::Borrow(false) => Ranges::Shared,
        }
    }
}

/// Whether a place or expression text holds a range (`..` only appears in ranges).
pub(super) fn has_range(text: &str) -> bool {
    text.contains("..")
}

/// A `usize` literal, now and then suffixed.
fn usize_lit(gen: &mut BodyGen<'_>, v: usize) -> String {
    if gen.rng.chance(20) {
        format!("{v}usize")
    } else {
        v.to_string()
    }
}

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
        // An element may be written `[]` only where this literal's type is known (seed
        // 12000841 wrote `let v = [[], []];`, E0282 in rustc).
        let elems: Vec<String> = (0..len)
            .map(|_| {
                let d = depth.saturating_sub(1);
                if typed {
                    self.typed_expr(elem, d).text
                } else {
                    self.val_expr(elem, d).text
                }
            })
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
    /// With `ranges`, now and then a range index goes before an index (`a[1..][k]`), and
    /// for [`Ranges::Mut`] the root is then held as mutably borrowed instead (see
    /// [`Ranges`]).
    pub(super) fn finish_place(&mut self, place: &mut Place, ranges: Ranges) {
        if !place.text.contains('[') {
            return;
        }
        let with_range: Vec<bool> = place
            .lens
            .iter()
            .map(|_| match ranges {
                Ranges::No => false,
                Ranges::Shared => self.range_depth == 0 && self.rng.chance(10),
                Ranges::Mut => self.range_depth == 0 && self.rng.chance(18),
            })
            .collect();
        let any_range = with_range.contains(&true);
        let mutable = any_range && ranges == Ranges::Mut;
        let lock = [(place.root, mutable)];
        self.lock(&lock);
        self.range_depth += u32::from(any_range);
        let text = place.text.clone();
        let mut out = String::new();
        let mut lens = place.lens.iter().copied();
        let mut rest = text.as_str();
        let mut segment = 0;
        while let Some(open) = rest.find('[') {
            out.push_str(&rest[..open]);
            let close = open + rest[open..].find(']').expect("a closing bracket");
            let inner = &rest[open + 1..close];
            let mut len = lens.next().expect("a length per index");
            let has_range_here = with_range[segment];
            segment += 1;
            if has_range_here {
                let range = self.range_index(&out, len, !mutable);
                out.push('[');
                out.push_str(&range);
                out.push(']');
                len = None;
            }
            let index = if inner == "?" || has_range_here || self.rng.chance(20) {
                self.index_expr(&out, len, !mutable)
            } else {
                inner.to_owned()
            };
            out.push('[');
            out.push_str(&index);
            out.push(']');
            rest = &rest[close + 1..];
        }
        out.push_str(rest);
        self.range_depth -= u32::from(any_range);
        self.unlock(&lock);
        place.text = out;
    }

    /// An index into `base` (an array of `len` elements, or a slice): usually in bounds,
    /// sometimes out of bounds or overflowing on the way. Without `len_ok` the index may
    /// not measure a slice (`base.len()` would read a mutably borrowed base): mostly `0`.
    fn index_expr(&mut self, base: &str, len: Option<usize>, len_ok: bool) -> String {
        if len.is_none() && !len_ok {
            return match self.rng.weighted(&[50, 20, 15, 15]) {
                0 => usize_lit(self, 0),
                1 => {
                    let e = self.index_operand();
                    format!("({e} % 2usize)")
                }
                2 => usize_lit(self, 1),
                _ => self.prim_leaf(USIZE).text,
            };
        }
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

    /// A range to index `base` with (#10), one of the six forms, without the brackets:
    /// constant bounds within `len` mostly, computed ones (`(e % 4usize)..`, `..s.len()`)
    /// now and then, and sometimes bounds out of range, reversed or overflowing. For a
    /// base of unknown length (a slice, a string) the bounds measure it (`(s.len() -
    /// 1usize)`) when `len_ok`, and are small constants otherwise.
    pub(super) fn range_index(&mut self, base: &str, len: Option<usize>, len_ok: bool) -> String {
        let form = self.rng.weighted(&[26, 18, 16, 8, 18, 14]);
        match len {
            Some(n) => self.sized_range(form, n),
            None if len_ok => {
                let measured = format!("{}.len()", len_receiver(base));
                self.measured_range(form, &measured)
            }
            None => self.blind_range(form),
        }
    }

    /// `(e % m)`: a computed value below `m` (`m > 0`).
    fn below(&mut self, m: &str) -> String {
        let e = self.index_operand();
        format!("({e} % {m})")
    }

    /// A range into an array of `n` elements (or a string of `n` bytes).
    fn sized_range(&mut self, form: usize, n: usize) -> String {
        let bad = self.rng.chance(5);
        // An inclusive end is out of range of an empty array: `..` instead, unless `bad`.
        let form = if n == 0 && form >= 4 && !bad { 3 } else { form };
        let computed = !bad && self.rng.chance(25);
        let up_to = format!("{}usize", n + 1);
        match form {
            // `a..b`
            0 => {
                if bad {
                    if n == 0 || self.rng.chance(50) {
                        let lo = self.rng.index(n + 1);
                        format!("{}..{}", usize_lit(self, lo), usize_lit(self, n + 1))
                    } else {
                        let hi = self.rng.index(n);
                        format!("{}..{}", usize_lit(self, hi + 1), usize_lit(self, hi))
                    }
                } else if computed {
                    let c = self.below(&up_to);
                    if self.rng.chance(50) {
                        format!("{c}..{}", usize_lit(self, n))
                    } else {
                        format!("0..{c}")
                    }
                } else {
                    let lo = self.rng.index(n + 1);
                    let hi = lo + self.rng.index(n - lo + 1);
                    format!("{}..{}", usize_lit(self, lo), usize_lit(self, hi))
                }
            }
            // `a..` and `..b`
            1 | 2 => {
                let bound = if bad {
                    usize_lit(self, n + 1)
                } else if computed {
                    self.below(&up_to)
                } else {
                    let k = self.rng.index(n + 1);
                    usize_lit(self, k)
                };
                if form == 1 {
                    format!("{bound}..")
                } else {
                    format!("..{bound}")
                }
            }
            3 => "..".to_owned(),
            // `a..=b`: `b < n` and `a <= b + 1`.
            4 => {
                if bad || n == 0 {
                    if n == 0 || self.rng.chance(50) {
                        let lo = self.rng.index(n + 1);
                        format!("{}..={}", usize_lit(self, lo), usize_lit(self, n))
                    } else {
                        let hi = self.rng.index(n);
                        format!("{}..={}", usize_lit(self, hi + 2), usize_lit(self, hi))
                    }
                } else if computed {
                    if self.rng.chance(50) {
                        let c = self.below(&up_to);
                        format!("{c}..={}", usize_lit(self, n - 1))
                    } else {
                        let c = self.below(&format!("{n}usize"));
                        format!("0..={c}")
                    }
                } else {
                    let hi = self.rng.index(n);
                    let lo = self.rng.index(hi + 2);
                    format!("{}..={}", usize_lit(self, lo), usize_lit(self, hi))
                }
            }
            // `..=b`
            _ => {
                if bad || n == 0 {
                    format!("..={}", usize_lit(self, n))
                } else if computed {
                    let c = self.below(&format!("{n}usize"));
                    format!("..={c}")
                } else {
                    let hi = self.rng.index(n);
                    format!("..={}", usize_lit(self, hi))
                }
            }
        }
    }

    /// A range into a slice or string whose length `len` (`s.len()`) may be read: the
    /// last element is `(len - 1usize)`, which overflows when it is empty.
    fn measured_range(&mut self, form: usize, len: &str) -> String {
        let last = format!("({len} - 1usize)");
        let up_to = format!("({len} + 1usize)");
        match form {
            0 => match self.rng.weighted(&[30, 20, 20, 15, 15]) {
                0 => format!("0..{len}"),
                1 => format!("1..{len}"),
                2 => format!("{}..{len}", self.below(&up_to)),
                3 => format!("0..{}", self.below(&up_to)),
                _ => format!("0..{last}"),
            },
            1 => match self.rng.weighted(&[30, 15, 15, 25, 15]) {
                0 => "1..".to_owned(),
                1 => "0..".to_owned(),
                2 => format!("{len}.."),
                3 => format!("{}..", self.below(&up_to)),
                _ => format!("{last}.."),
            },
            2 => match self.rng.weighted(&[25, 15, 30, 30]) {
                0 => format!("..{len}"),
                1 => "..1".to_owned(),
                2 => format!("..{}", self.below(&up_to)),
                _ => format!("..{last}"),
            },
            3 => "..".to_owned(),
            4 => match self.rng.weighted(&[40, 40, 20]) {
                0 => format!("0..={last}"),
                1 => format!("{}..={last}", self.below(&up_to)),
                _ => "1..=1".to_owned(),
            },
            _ => {
                if self.rng.chance(70) {
                    format!("..={last}")
                } else {
                    "..=0".to_owned()
                }
            }
        }
    }

    /// A range into a slice or string of unknown length that may not be measured: small
    /// constants (or now and then a small computed start).
    fn blind_range(&mut self, form: usize) -> String {
        match form {
            0 => (*self.rng.pick(&["0..1", "1..2", "0..0", "1..1"])).to_owned(),
            1 => {
                if self.rng.chance(20) {
                    format!("{}..", self.below("2usize"))
                } else {
                    (*self.rng.pick(&["0..", "1.."])).to_owned()
                }
            }
            2 => (*self.rng.pick(&["..0", "..1"])).to_owned(),
            3 => "..".to_owned(),
            4 => "0..=0".to_owned(),
            _ => "..=0".to_owned(),
        }
    }

    /// A range index for the place `place` of an array, slice or string type of `len`
    /// elements, with its root held (as mutably borrowed when `mutable`, see [`Ranges`])
    /// while the bounds are generated.
    pub(super) fn locked_range(
        &mut self,
        place: &Place,
        len: Option<usize>,
        mutable: bool,
    ) -> String {
        let lock = [(place.root, mutable)];
        self.lock(&lock);
        self.range_depth += 1;
        let range = self.range_index(&place.text, len, !mutable);
        self.range_depth -= 1;
        self.unlock(&lock);
        range
    }

    /// `&a[lo..hi]` or `&mut a[lo..hi]` (#10): a reference to the slice type `slice`, of a
    /// range place of an array or slice place of its element type.
    pub(super) fn range_borrow(&mut self, mutable: bool, slice: ValTy) -> Option<Expr> {
        let prog = self.prog;
        let (elem, None) = prog.array_of(slice)? else {
            return None;
        };
        let place = self.place_where(
            |t| prog.array_of(t).is_some_and(|(e, _)| e == elem),
            Access::Borrow(mutable),
        )?;
        let (_, len) = prog.array_of(place.ty).expect("an array");
        let range = self.locked_range(&place, len, mutable);
        Some(Expr {
            text: format!(
                "&{}{}[{range}]",
                if mutable { "mut " } else { "" },
                len_receiver(&place.text)
            ),
            roots: vec![(place.root, mutable)],
        })
    }

    /// A range slice of the reference parameter `p` (or of a sub-place of its referent)
    /// for the tail of a function returning a `&[T]`, `&mut [T]` or `&str` (`inner`):
    /// `&s[1..]`, `&mut self.Arr[..k]`; now and then, when there is one.
    pub(super) fn range_tail(&mut self, p: usize, mutable: bool, inner: ValTy) -> Option<String> {
        let prog = self.prog;
        let Ty::Ref { inner: pinner, .. } = self.vars[p].ty else {
            return None;
        };
        let elem = match inner {
            ValTy::Str => None,
            _ => match prog.array_of(inner)? {
                (elem, None) => Some(elem),
                (_, Some(_)) => return None,
            },
        };
        let subs: Vec<(String, ValTy)> = prog
            .sub_places(pinner)
            .into_iter()
            .filter(|&(_, t)| match elem {
                None => t == ValTy::Str,
                Some(elem) => prog.array_of(t).is_some_and(|(e, _)| e == elem),
            })
            .map(|(path, t)| (path.replace("[?]", "[0]"), t))
            .collect();
        if subs.is_empty() || !self.rng.chance(50) {
            return None;
        }
        let (path, ty) = self.rng.pick(&subs).clone();
        let text = format!("{}{path}", self.vars[p].name);
        let len = prog.array_of(ty).and_then(|(_, len)| len);
        let place = Place {
            text: text.clone(),
            ty,
            root: p,
            via_ref: Some(mutable),
            whole: path.is_empty(),
            no_move: true,
            lens: Vec::new(),
        };
        let range = self.locked_range(&place, len, mutable);
        Some(format!(
            "&{}{text}[{range}]",
            if mutable { "mut " } else { "" }
        ))
    }

    /// `let m: &mut [T] = &mut a[lo..hi];` (or a shared one) over an array or slice place
    /// `a`, then for a mutable one now and then a write or compound assignment through it
    /// (`m[k] = v;`, `m[k].X += v;`), its right-hand side generated (and evaluated) first.
    pub(super) fn stmt_range_ref(&mut self, depth: u32) -> bool {
        let prog = self.prog;
        let slices: Vec<usize> = (0..prog.arrays.len())
            .filter(|&a| prog.arrays[a].len.is_none())
            .collect();
        if slices.is_empty() {
            return false;
        }
        let slice = ValTy::Array(*self.rng.pick(&slices));
        let mutable = self.rng.chance(70);
        let Some(e) = self.range_borrow(mutable, slice) else {
            return false;
        };
        let name = self.fresh_name();
        let ty = Ty::Ref {
            mutable,
            inner: slice,
        };
        self.line(&format!("let {name}: {} = {};", prog.ty_name(ty), e.text));
        self.bind_ref(name.clone(), ty, &e);
        if !mutable || !self.rng.chance(75) {
            return true;
        }
        let (elem, _) = prog.array_of(slice).expect("a slice");
        let numeric: Vec<(String, Prim)> = prog
            .leaves(elem)
            .into_iter()
            .filter(|(_, p)| p.is_numeric())
            .collect();
        if !numeric.is_empty() && self.rng.chance(45) {
            let (path, p) = self.rng.pick(&numeric).clone();
            let op = if p.is_int() {
                *self.rng.pick(&["+=", "-=", "*=", "^=", "|="])
            } else {
                *self.rng.pick(&["+=", "-=", "*="])
            };
            let rhs = self.prim_expr(p, 1);
            let index = self.index_expr(&name, None, true);
            self.line(&format!("{name}[{index}]{path} {op} {};", rhs.text));
        } else {
            let rhs = self.typed_expr(elem, depth.saturating_sub(1));
            let index = self.index_expr(&name, None, true);
            self.line(&format!("{name}[{index}] = {};", rhs.text));
        }
        true
    }

    fn index_operand(&mut self) -> String {
        if self.simple > 0 {
            self.prim_leaf(USIZE).text
        } else {
            self.prim_expr(USIZE, 1).text
        }
    }

    /// `x.len()` for a place `x` of array or slice type that may be borrowed (or, with the
    /// violation knob, one that may not), now and then of a range place of it
    /// (`x[1..].len()`), or the length of a string (#10).
    pub(super) fn len_expr(&mut self) -> Option<Expr> {
        let prog = self.prog;
        let choice = self.rng.weighted(&[55, 20, 25]);
        if choice == 2 {
            return Some(self.str_len_expr());
        }
        let (place, _) = self.any_place(Access::Borrow(false), |t| prog.array_of(t).is_some())?;
        let receiver = len_receiver(&place.text);
        if choice == 1 && self.range_depth == 0 {
            let (_, len) = prog.array_of(place.ty).expect("an array");
            let range = self.locked_range(&place, len, false);
            return Some(Expr::value(format!("{receiver}[{range}].len()")));
        }
        Some(Expr::value(format!("{receiver}.len()")))
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
        // index expressions are leaves, and its ranges are made for a mutable use.
        let found = self.simply(|gen| {
            gen.any_place_with(
                Access::Borrow(false),
                |t| prog.array_of(t).is_some(),
                Ranges::Mut,
            )
        });
        let Some((place, _)) = found else {
            return false;
        };
        let (elem, len) = prog.array_of(place.ty).expect("an array");
        let mut base = len_receiver(&place.text);
        if self.range_depth == 0 && self.rng.chance(35) {
            // Walk a range place of it (`while i < a[1..].len() { a[1..][i] .. }`).
            let range = self.simply(|gen| gen.locked_range(&place, len, true));
            base = format!("{base}[{range}]");
        }
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

/// The receiver of `.len()` (or the base of a range index) for a place: `*s` is written
/// `s` (auto-deref).
pub(super) fn len_receiver(text: &str) -> String {
    match text.strip_prefix('*') {
        Some(name) => name.to_owned(),
        None => text.to_owned(),
    }
}
