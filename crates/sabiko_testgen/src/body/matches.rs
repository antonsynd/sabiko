//! Enums and pattern matching (#8): variant construction, `match` as a statement, as the
//! initialiser of a `let` and inline in expressions, `if let` (statement and expression),
//! `while let` over a state variable, `let`-`else` and irrefutable destructuring `let`s.
//!
//! Every `match` is exhaustive by construction: a few optional arms with refutable
//! patterns (literals, ranges, or-patterns, nested variants, `x @ p`) or guards come first,
//! then unguarded arms that cover the type: a catch-all, or a *cover* (one arm per variant,
//! `true`/`false`, contiguous integer ranges from the type's minimum to its maximum, nested
//! covers of one payload element, some arms merged into or-patterns, a suffix replaced by a
//! catch-all).
//!
//! Patterns are generated against the scope model. A scrutinee is a place matched by value
//! (`match p.A`, `match *r`), a reference (`match &x`, `match r`, `match F(&mut x)`: default
//! binding modes, every binding a reference) or a temporary (`match F()`, a variant
//! literal). By value, a binding of a non-`Copy` part moves it, which the model allows only
//! where a move of the place is allowed and never out of a type with `impl Drop` (E0509);
//! `ref` and `ref mut` borrow the place and are reference locals with a region, like the
//! bindings under a default binding mode. A by-value move marks the place's root moved for
//! the rest of the arm and, joined over the arms, after the `match`. Guards only read
//! (comparisons of primitive places and bindings with literals), with the scrutinee
//! locked as the guard's fake borrow does. The violation knob relaxes the move and `ref
//! mut` rules now and then, which gives E0505/E0507/E0509/E0596-style errors, but only
//! in code that is surely reachable (see `Arm::first`).
//!
//! TODO(#8 follow-up): Sabiko does not borrow-check code that the patterns make
//! unreachable, where rustc does (its match lowering adds false edges from each candidate
//! to the next, and an irrefutable `if let`/`let`-`else` keeps its `else`): an assignment
//! to an immutable binding or a use after move in an unreachable arm (`match W::Only(1, 2)
//! { W::Only(_, b) => .., W::Only(a, _) => { a = 3; } }`) or in the `else` of an
//! irrefutable `if let` or `let`-`else` is E0384/E0382 in rustc and accepted by Sabiko,
//! and a move there makes a later use an error in rustc only
//! (`tests/pending/enums/known_gap/unreachable_arms_borrowck.sabi`; seeds 2001893 and
//! 3000551). So the generator makes no arm unreachable: the arms before a cover are always
//! guarded, the cover's arms are disjoint, and an `if let` or `let`-`else` pattern surely
//! refutes (`PatCx::refuted`). As a second line, checks are only skipped in the first
//! arm of a `match` and never in an `else`.

use super::{Access, BodyGen, Expr, LoopKind, Place};
use crate::program::{int_literal, random_int, VariantKind, CHARS};
use crate::ty::{IntTy, Prim, Ty, ValTy};

/// The default binding mode at the top of a pattern: by value, or (matching a reference
/// with a non-reference pattern) by `&` or `&mut` reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Move,
    Ref(bool),
}

#[derive(Debug, Clone)]
struct Binding {
    name: String,
    ty: Ty,
    mutable: bool,
}

/// What the patterns of one scrutinee may do, and what one pattern did.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // independent permissions and outcomes
struct PatCx {
    mode: Mode,
    can_move: bool,
    can_ref: bool,
    can_ref_mut: bool,
    /// False inside an or-pattern alternative and under `x @`: only binding-free patterns.
    bindings_ok: bool,
    binds: Vec<Binding>,
    /// A by-value binding moved a non-`Copy` part of the scrutinee (or, at the top of a
    /// pattern on a `&mut` reference, the reference itself).
    moved: bool,
    /// Inside a struct, tuple or variant pattern: a binding there under a default binding
    /// mode borrows through the reference; at the top it binds the reference itself.
    nested: bool,
    /// The pattern surely rejects some value (a literal, a range, a variant of an enum with
    /// several): `refutable` may produce an irrefutable pattern (floats, single-variant
    /// enums), which must not be an unguarded early arm, an `if let` with an `else` or a
    /// `let`-`else`, since code it makes unreachable is checked differently (module docs).
    refuted: bool,
}

/// The scrutinee of a `match`, `if let`, `while let` or `let`.
#[derive(Debug, Clone)]
struct Scrut {
    text: String,
    ty: ValTy,
    mode: Mode,
    /// The root of a place matched by value, which a by-value move out of it moves.
    place_root: Option<usize>,
    /// What a reference binding borrows: the place's root (by value), the scrutinee
    /// reference's loans (default binding mode); nothing for a temporary.
    roots: Vec<usize>,
    /// Held while a guard is generated: the fake borrow of a place matched by value, or the
    /// scrutinee reference's loans.
    guard_lock: Vec<(usize, bool)>,
    can_move: bool,
    can_ref: bool,
    can_ref_mut: bool,
    /// `can_move` and `can_ref_mut` without violations, for patterns that may be
    /// unreachable; `None` when they hold none.
    strict: Option<(bool, bool)>,
}

/// Part of a cover: a set of patterns of a type that together match every value.
#[derive(Debug, Clone)]
enum Shape {
    /// An irrefutable pattern.
    Any,
    Bool(bool),
    /// An integer literal or range pattern, as text.
    Range(String),
    /// Not part of a cover: a refutable pattern (a marker for the element `refutable`
    /// refines).
    Refutable,
    /// A variant, with at most one payload element refined by a nested shape.
    Variant(usize, Option<(usize, Box<Shape>)>),
    /// A struct or tuple with one element refined by a nested shape.
    Field(usize, Box<Shape>),
}

/// An element of a tuple-like or struct-like pattern.
#[derive(Debug, Clone)]
enum Sub {
    Wild,
    Pat(String),
    /// A struct field written in shorthand (`X`, `ref mut X`): the binding has the field's
    /// name.
    Short(String),
}

#[derive(Debug, Clone)]
struct Arm {
    pat: String,
    cx: PatCx,
    /// `Some(forced)` when the arm is not needed for exhaustiveness and may carry a guard
    /// (must, when `forced`).
    guard: Option<bool>,
    /// The first arm, the only one surely reachable: violations go there only.
    first: bool,
}

/// `u128::MAX`, which the generator's `i128` value model cannot hold.
const U128_MAX: &str = "340282366920938463463374607431768211455u128";

/// `text` as a scrutinee: a struct or variant literal (or anything containing one) is not
/// allowed bare before the arms' brace.
fn scrutinee_text(text: String) -> String {
    if text.contains('{') && !text.starts_with('(') {
        format!("({text})")
    } else {
        text
    }
}

/// A pattern in a `let`: a top-level or-pattern needs parentheses.
fn let_pattern(pat: String) -> String {
    if pat.contains('|') {
        format!("({pat})")
    } else {
        pat
    }
}

impl BodyGen<'_> {
    // ----- enum values -----------------------------------------------------------------

    /// An expression of enum type `e`: a variant literal, a place, a call, an `if` or an
    /// inline `match`.
    pub(super) fn enum_expr(&mut self, e: usize, depth: u32) -> Expr {
        let ty = ValTy::Enum(e);
        let has_calls = !self
            .callables(|r| r == crate::program::RetTy::Val(ty))
            .is_empty();
        let choice = if depth == 0 {
            self.rng.weighted(&[50, 50, 0, 0, 0])
        } else {
            self.rng
                .weighted(&[40, 30, if has_calls { 15 } else { 0 }, 10, 5])
        };
        match choice {
            0 => self.variant_literal(e, depth),
            1 => match self.place_of(ty, Access::Read) {
                Some(place) => Expr::value(place.text),
                None => self.variant_literal(e, depth),
            },
            2 => self
                .value_call(ty, depth)
                .unwrap_or_else(|| self.variant_literal(e, depth)),
            3 => self.if_expr(ty, depth),
            _ => self
                .deref_call(ty, depth)
                .unwrap_or_else(|| self.variant_literal(e, depth)),
        }
    }

    /// `E::V`, `E::V(a, b)` or `E::V { F: a, G: b }` (fields now and then out of order;
    /// they are evaluated as written).
    pub(super) fn variant_literal(&mut self, e: usize, depth: u32) -> Expr {
        let prog = self.prog;
        let def = &prog.enums[e];
        let v = &def.variants[self.rng.index(def.variants.len())];
        let path = format!("{}::{}", def.name, v.name);
        let text = match &v.kind {
            VariantKind::Unit => path,
            VariantKind::Tuple(elems) => {
                let args: Vec<String> = elems
                    .iter()
                    .map(|t| self.val_expr(*t, depth.saturating_sub(1)).text)
                    .collect();
                format!("{path}({})", args.join(", "))
            }
            VariantKind::Struct(fields) => {
                let mut order: Vec<usize> = (0..fields.len()).collect();
                if order.len() > 1 && self.rng.chance(30) {
                    order.reverse();
                }
                let parts: Vec<String> = order
                    .into_iter()
                    .map(|i| {
                        let value = self.val_expr(fields[i].ty, depth.saturating_sub(1));
                        format!("{}: {}", fields[i].name, value.text)
                    })
                    .collect();
                format!("{path} {{ {} }}", parts.join(", "))
            }
        };
        Expr::value(text)
    }

    /// A value of type `ty` that is never a place (so a `ref mut` binding of a scrutinee
    /// made of it borrows a temporary): a literal, a call or an `if`. Primitive
    /// expressions may still be places; their bindings only copy.
    fn temp_value(&mut self, ty: ValTy, depth: u32) -> Expr {
        let has_calls = !self
            .callables(|r| r == crate::program::RetTy::Val(ty))
            .is_empty();
        let choice = self.rng.weighted(&[55, if has_calls { 25 } else { 0 }, 20]);
        if let ValTy::Prim(p) = ty {
            return self.prim_expr(p, depth);
        }
        if choice == 1 {
            if let Some(e) = self.value_call(ty, depth) {
                return e;
            }
        }
        match ty {
            _ if choice == 2 && depth > 0 => self.if_expr(ty, depth),
            ValTy::Struct(s) => self.struct_literal(s, depth),
            ValTy::Tuple(t) => self.tuple_literal(t, depth),
            ValTy::Enum(e) => self.variant_literal(e, depth),
            ValTy::Prim(_) => unreachable!(),
        }
    }

    // ----- scrutinees ------------------------------------------------------------------

    /// A type to match on: enums most often, then integers, `bool`, `char`, structs and
    /// tuples; half of the time the type of a place in scope.
    fn matchable_ty(&mut self) -> ValTy {
        if self.rng.chance(50) {
            let places: Vec<ValTy> = self
                .candidates()
                .into_iter()
                .map(|p| p.ty)
                .filter(|t| !matches!(t, ValTy::Prim(Prim::F32 | Prim::F64)))
                .collect();
            if !places.is_empty() {
                let enums: Vec<ValTy> = places
                    .iter()
                    .copied()
                    .filter(|t| matches!(t, ValTy::Enum(_)))
                    .collect();
                if !enums.is_empty() && self.rng.chance(60) {
                    return *self.rng.pick(&enums);
                }
                return *self.rng.pick(&places);
            }
        }
        let prog = self.prog;
        match self.rng.weighted(&[
            if prog.enums.is_empty() { 0 } else { 55 },
            20,
            8,
            5,
            if prog.structs.is_empty() { 0 } else { 6 },
            if prog.tuples.is_empty() { 0 } else { 6 },
        ]) {
            0 => ValTy::Enum(self.rng.index(prog.enums.len())),
            1 => ValTy::Prim(Prim::Int(*self.rng.pick(&IntTy::ALL))),
            2 => ValTy::Prim(Prim::Bool),
            3 => ValTy::Prim(Prim::Char),
            4 => ValTy::Struct(self.rng.index(prog.structs.len())),
            _ => ValTy::Tuple(self.rng.index(prog.tuples.len())),
        }
    }

    /// A scrutinee of type `ty`: a place by value, a reference or a temporary.
    fn scrutinee(&mut self, ty: ValTy, depth: u32) -> Scrut {
        let first = self.rng.weighted(&[45, 30, 25]);
        for kind in (0..3).map(|k| (first + k) % 3) {
            let found = match kind {
                0 => self.place_scrut(ty),
                1 => self.ref_scrut(ty, depth),
                _ => None,
            };
            if let Some(s) = found {
                return s;
            }
        }
        self.temp_scrut(ty, depth)
    }

    /// Whether the discriminant (or a `Copy` part) of a place may be read.
    fn readable(&self, place: &Place) -> bool {
        let root = &self.vars[place.root];
        !root.moved && root.mut_locks == 0
    }

    /// Whether a non-`Copy` part of a place may be moved out by a pattern.
    fn movable(&self, place: &Place) -> bool {
        let root = &self.vars[place.root];
        self.readable(place)
            && place.via_ref.is_none()
            && !place.in_drop
            && root.shared_locks == 0
            && root.loop_depth == self.loop_depth()
    }

    fn place_scrut(&mut self, ty: ValTy) -> Option<Scrut> {
        let typed: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| p.ty == ty)
            .collect();
        let (ok, bad): (Vec<Place>, Vec<Place>) = typed.into_iter().partition(|p| self.readable(p));
        let pool = self.choose_pool(ok, bad)?;
        let place = self.rng.pick(&pool).clone();
        let strict_move = self.movable(&place);
        let can_ref = self.permitted(&place, Access::Borrow(false));
        let strict_ref_mut = self.permitted(&place, Access::Borrow(true));
        let can_move = strict_move || self.violate();
        let can_ref_mut = strict_ref_mut || self.violate();
        Some(Scrut {
            text: place.text.clone(),
            ty,
            mode: Mode::Move,
            place_root: Some(place.root),
            roots: vec![place.root],
            guard_lock: vec![(place.root, false)],
            can_move,
            can_ref,
            can_ref_mut,
            strict: Some((strict_move, strict_ref_mut)),
        })
    }

    fn ref_scrut(&mut self, ty: ValTy, depth: u32) -> Option<Scrut> {
        let mutable = self.rng.chance(45);
        let mut e = self.ref_expr(mutable, ty, depth)?;
        // A bare reference local stays a place (`match r`) half of the time, which a
        // binding of the whole value moves (a `&mut`); otherwise it is reborrowed
        // explicitly. A `&mut` local wanted as `&` is always reborrowed (`&*r`): bare, it
        // would match as `&mut` (seed 1000935 bound it whole and moved it).
        // A parameter is always reborrowed: a function returning a reference returns one
        // into it at the end, whatever moved before (seed 3001487 moved `self` so).
        let mut place_root = None;
        if let [(root, _)] = e.roots[..] {
            if let (true, Ty::Ref { mutable: m, .. }) =
                (e.text == self.vars[root].name, self.vars[root].ty)
            {
                if m != mutable || root < self.sig.params.len() || self.rng.chance(50) {
                    e.text = format!("&{}*{}", if mutable { "mut " } else { "" }, e.text);
                } else {
                    place_root = Some(root);
                }
            }
        }
        Some(Scrut {
            text: scrutinee_text(e.text),
            ty,
            mode: Mode::Ref(mutable),
            place_root,
            roots: e.roots.iter().map(|&(r, _)| r).collect(),
            guard_lock: e.roots,
            can_move: false,
            can_ref: false,
            can_ref_mut: false,
            strict: None,
        })
    }

    fn temp_scrut(&mut self, ty: ValTy, depth: u32) -> Scrut {
        let e = self.temp_value(ty, depth);
        let aggregate = !matches!(ty, ValTy::Prim(_));
        Scrut {
            text: scrutinee_text(e.text),
            ty,
            mode: Mode::Move,
            place_root: None,
            roots: Vec::new(),
            guard_lock: Vec::new(),
            can_move: true,
            can_ref: aggregate,
            can_ref_mut: aggregate,
            strict: None,
        }
    }

    fn pat_cx(s: &Scrut) -> PatCx {
        PatCx {
            mode: s.mode,
            can_move: s.can_move,
            can_ref: s.can_ref,
            can_ref_mut: s.can_ref_mut,
            bindings_ok: true,
            binds: Vec::new(),
            moved: false,
            nested: false,
            refuted: false,
        }
    }

    // ----- patterns --------------------------------------------------------------------

    /// A binding of a value of type `ty` (named `name`, or a fresh name): by value (`x`,
    /// `mut x`), `ref x` or `ref mut x` as the scrutinee allows, or a reference under a
    /// default binding mode. `None` when no binding is allowed here.
    fn binding(
        &mut self,
        ty: ValTy,
        cx: &mut PatCx,
        in_drop: bool,
        name: Option<String>,
    ) -> Option<String> {
        if !cx.bindings_ok {
            return None;
        }
        let copy = self.prog.is_copy(Ty::Val(ty));
        let (prefix, bty, mutable) = match cx.mode {
            Mode::Ref(m) => {
                if !cx.nested {
                    // The scrutinee reference itself, by value: a `&mut` one is moved.
                    cx.moved |= m;
                }
                (
                    "",
                    Ty::Ref {
                        mutable: m,
                        inner: ty,
                    },
                    false,
                )
            }
            Mode::Move => {
                let by_value = copy || (cx.can_move && (!in_drop || self.violate()));
                let choice = self.rng.weighted(&[
                    if by_value { 60 } else { 0 },
                    if cx.can_ref { 20 } else { 0 },
                    if cx.can_ref_mut { 20 } else { 0 },
                    1,
                ]);
                match choice {
                    0 => {
                        if !copy {
                            cx.moved = true;
                        }
                        if self.rng.chance(30) {
                            ("mut ", Ty::Val(ty), true)
                        } else {
                            ("", Ty::Val(ty), false)
                        }
                    }
                    1 => (
                        "ref ",
                        Ty::Ref {
                            mutable: false,
                            inner: ty,
                        },
                        false,
                    ),
                    2 => (
                        "ref mut ",
                        Ty::Ref {
                            mutable: true,
                            inner: ty,
                        },
                        false,
                    ),
                    _ => return None,
                }
            }
        };
        let name = name.unwrap_or_else(|| self.fresh_name());
        cx.binds.push(Binding {
            name: name.clone(),
            ty: bty,
            mutable,
        });
        Some(format!("{prefix}{name}"))
    }

    /// An irrefutable pattern of type `ty`: `_`, a binding, `x @ p` or a destructuring of
    /// a struct, tuple or single-variant enum.
    fn irrefutable(&mut self, ty: ValTy, cx: &mut PatCx, in_drop: bool, depth: u32) -> String {
        let destructurable = match ty {
            ValTy::Struct(_) | ValTy::Tuple(_) => true,
            ValTy::Enum(e) => self.prog.enums[e].variants.len() == 1,
            ValTy::Prim(_) => false,
        };
        let choice = self.rng.weighted(&[
            20,
            if cx.bindings_ok { 45 } else { 0 },
            if destructurable && depth < 3 { 35 } else { 0 },
            if destructurable && cx.bindings_ok && depth < 3 {
                4
            } else {
                0
            },
        ]);
        match choice {
            0 => "_".to_owned(),
            1 => self
                .binding(ty, cx, in_drop, None)
                .unwrap_or_else(|| "_".to_owned()),
            2 => self.destructure(ty, cx, in_drop, depth),
            _ => self.at_pattern(ty, cx, in_drop, |gen, cx| {
                gen.destructure(ty, cx, in_drop, depth)
            }),
        }
    }

    /// `x @ p` where `p` (from `sub`) binds nothing; `p` alone when no binding is allowed.
    fn at_pattern(
        &mut self,
        ty: ValTy,
        cx: &mut PatCx,
        in_drop: bool,
        sub: impl FnOnce(&mut Self, &mut PatCx) -> String,
    ) -> String {
        let binding = self.binding(ty, cx, in_drop, None);
        let saved = cx.bindings_ok;
        cx.bindings_ok = false;
        let p = sub(self, cx);
        cx.bindings_ok = saved;
        match binding {
            Some(b) => format!("{b} @ {p}"),
            None => p,
        }
    }

    /// A struct, tuple or single-variant enum pattern whose elements are irrefutable.
    fn destructure(&mut self, ty: ValTy, cx: &mut PatCx, in_drop: bool, depth: u32) -> String {
        let shape = match ty {
            ValTy::Enum(_) => Shape::Variant(0, None),
            _ => Shape::Any,
        };
        self.render_composite(ty, &shape, cx, in_drop, depth)
    }

    /// Renders a struct, tuple or variant pattern of `ty` (`shape` selects the variant and
    /// a refined element); unrefined elements are irrefutable or wildcards.
    fn render_composite(
        &mut self,
        ty: ValTy,
        shape: &Shape,
        cx: &mut PatCx,
        in_drop: bool,
        depth: u32,
    ) -> String {
        let prog = self.prog;
        let in_drop = in_drop || prog.has_drop_impl(ty);
        let (head, elems): (String, Vec<(Option<String>, ValTy)>) = match ty {
            ValTy::Struct(s) => (
                prog.structs[s].name.clone(),
                prog.structs[s]
                    .fields
                    .iter()
                    .map(|f| (Some(f.name.clone()), f.ty))
                    .collect(),
            ),
            ValTy::Tuple(t) => (
                String::new(),
                prog.tuples[t].iter().map(|e| (None, *e)).collect(),
            ),
            ValTy::Enum(e) => {
                let Shape::Variant(v, _) = shape else {
                    unreachable!("an enum pattern names a variant")
                };
                let def = &prog.enums[e];
                let variant = &def.variants[*v];
                (
                    format!("{}::{}", def.name, variant.name),
                    variant
                        .elems()
                        .into_iter()
                        .map(|(n, t)| (n.map(str::to_owned), t))
                        .collect(),
                )
            }
            ValTy::Prim(_) => unreachable!("a primitive is not a composite"),
        };
        let refined = match shape {
            Shape::Variant(_, Some((i, s))) | Shape::Field(i, s) => Some((*i, (**s).clone())),
            _ => None,
        };
        let mut subs = Vec::new();
        let outer = cx.nested;
        cx.nested = true;
        for (i, (name, ety)) in elems.iter().enumerate() {
            let sub = match &refined {
                Some((r, s)) if *r == i => {
                    Sub::Pat(self.render_shape(s, *ety, cx, in_drop, depth + 1))
                }
                _ => self.element_sub(name.as_deref(), *ety, cx, in_drop, depth),
            };
            subs.push(sub);
        }
        cx.nested = outer;
        let is_struct = elems.first().is_some_and(|(n, _)| n.is_some());
        if elems.is_empty() {
            head
        } else if is_struct {
            let names: Vec<String> = elems
                .iter()
                .map(|(n, _)| n.clone().unwrap_or_default())
                .collect();
            format!("{head} {}", self.render_struct_fields(&names, subs))
        } else {
            let one_tuple = matches!(ty, ValTy::Tuple(_)) && elems.len() == 1;
            format!("{head}{}", self.render_tuple_elems(subs, one_tuple))
        }
    }

    /// An unrefined element of a composite pattern: a wildcard, an irrefutable pattern, or
    /// (a struct field, now and then) a shorthand binding named after the field.
    fn element_sub(
        &mut self,
        name: Option<&str>,
        ty: ValTy,
        cx: &mut PatCx,
        in_drop: bool,
        depth: u32,
    ) -> Sub {
        if let Some(field) = name {
            if cx.bindings_ok && self.rng.chance(20) && !self.used_names.iter().any(|u| u == field)
            {
                if let Some(b) = self.binding(ty, cx, in_drop, Some(field.to_owned())) {
                    self.used_names.push(field.to_owned());
                    return Sub::Short(b);
                }
            }
        }
        if self.rng.chance(30) {
            Sub::Wild
        } else {
            Sub::Pat(self.irrefutable(ty, cx, in_drop, depth + 1))
        }
    }

    /// `(a, _, c)`, `(a, ..)`, `(.., c)` or `(..)` (wildcards elided into `..`).
    fn render_tuple_elems(&mut self, subs: Vec<Sub>, one_tuple: bool) -> String {
        let texts: Vec<Option<String>> = subs
            .into_iter()
            .map(|s| match s {
                Sub::Wild => None,
                Sub::Pat(t) | Sub::Short(t) => Some(t),
            })
            .collect();
        let first = texts.iter().position(Option::is_some);
        let last = texts.iter().rposition(Option::is_some);
        let wild = |t: &Option<String>| t.clone().unwrap_or_else(|| "_".to_owned());
        match (first, last) {
            (None, _) if self.rng.chance(50) => "(..)".to_owned(),
            (_, Some(l)) if l + 1 < texts.len() && self.rng.chance(50) => {
                let mut parts: Vec<String> = texts[..=l].iter().map(wild).collect();
                parts.push("..".to_owned());
                format!("({})", parts.join(", "))
            }
            (Some(f), _) if f > 0 && self.rng.chance(40) => {
                let mut parts = vec!["..".to_owned()];
                parts.extend(texts[f..].iter().map(wild));
                format!("({})", parts.join(", "))
            }
            _ => {
                let parts: Vec<String> = texts.iter().map(wild).collect();
                if one_tuple {
                    format!("({},)", parts[0])
                } else {
                    format!("({})", parts.join(", "))
                }
            }
        }
    }

    /// `{ A: p, B }` or `{ A: p, .. }` (wildcard fields elided or written `F: _`).
    fn render_struct_fields(&mut self, names: &[String], subs: Vec<Sub>) -> String {
        let elide = self.rng.chance(75);
        let mut parts = Vec::new();
        let mut elided = false;
        for (name, sub) in names.iter().zip(subs) {
            match sub {
                Sub::Wild if elide => elided = true,
                Sub::Wild => parts.push(format!("{name}: _")),
                Sub::Pat(t) => parts.push(format!("{name}: {t}")),
                Sub::Short(t) => parts.push(t),
            }
        }
        if elided {
            parts.push("..".to_owned());
        }
        format!("{{ {} }}", parts.join(", "))
    }

    /// A pattern of type `ty` that may not match: a literal, a range, an or-pattern, a
    /// variant (with nested patterns), `x @ p`, or a struct or tuple with a refutable
    /// element. Float types (no float patterns) get an irrefutable one.
    fn refutable(&mut self, ty: ValTy, cx: &mut PatCx, in_drop: bool, depth: u32) -> String {
        match ty {
            ValTy::Prim(Prim::Int(i)) => self.int_refutable(i, cx),
            ValTy::Prim(Prim::Bool) => {
                cx.refuted = true;
                (if self.rng.chance(50) { "true" } else { "false" }).to_owned()
            }
            ValTy::Prim(Prim::Char) => self.char_refutable(cx),
            ValTy::Prim(_) => self.irrefutable(ty, cx, in_drop, depth),
            ValTy::Enum(e) => self.enum_refutable(e, cx, in_drop, depth),
            ValTy::Struct(_) | ValTy::Tuple(_) => {
                let n = match ty {
                    ValTy::Struct(s) => self.prog.structs[s].fields.len(),
                    ValTy::Tuple(t) => self.prog.tuples[t].len(),
                    _ => unreachable!(),
                };
                if depth >= 2 {
                    return self.irrefutable(ty, cx, in_drop, depth);
                }
                let i = self.rng.index(n);
                let shape = Shape::Field(i, Box::new(Shape::Refutable));
                self.render_composite(ty, &shape, cx, in_drop, depth)
            }
        }
    }

    fn enum_refutable(&mut self, e: usize, cx: &mut PatCx, in_drop: bool, depth: u32) -> String {
        let prog = self.prog;
        let def = &prog.enums[e];
        let ty = ValTy::Enum(e);
        let choice = self.rng.weighted(&[
            70,
            if def.variants.len() >= 2 { 15 } else { 0 },
            if cx.bindings_ok { 12 } else { 0 },
        ]);
        // One variant of several leaves the others out; two of exactly two do not.
        cx.refuted |= def.variants.len() > if choice == 1 { 2 } else { 1 };
        match choice {
            0 => {
                let v = self.rng.index(def.variants.len());
                let n = def.variants[v].elems().len();
                let refine = (n > 0 && depth < 2 && self.rng.chance(45))
                    .then(|| (self.rng.index(n), Box::new(Shape::Refutable)));
                self.render_composite(ty, &Shape::Variant(v, refine), cx, in_drop, depth)
            }
            1 => {
                // Two variants, binding nothing.
                let a = self.rng.index(def.variants.len());
                let b = (a + 1 + self.rng.index(def.variants.len() - 1)) % def.variants.len();
                let saved = cx.bindings_ok;
                cx.bindings_ok = false;
                let pa = self.render_composite(ty, &Shape::Variant(a, None), cx, in_drop, depth);
                let pb = self.render_composite(ty, &Shape::Variant(b, None), cx, in_drop, depth);
                cx.bindings_ok = saved;
                format!("{pa} | {pb}")
            }
            _ => {
                let v = self.rng.index(def.variants.len());
                self.at_pattern(ty, cx, in_drop, |gen, cx| {
                    gen.render_composite(ty, &Shape::Variant(v, None), cx, in_drop, depth)
                })
            }
        }
    }

    /// A value for an integer pattern: small most of the time, sometimes anything or a
    /// boundary of the type.
    fn int_pat_value(&mut self, i: IntTy) -> i128 {
        match self.rng.weighted(&[70, 15, 15]) {
            0 => {
                let v = if i.signed() {
                    self.rng.range(-3, 12)
                } else {
                    self.rng.range(0, 12)
                };
                i128::from(v)
            }
            1 => random_int(self.rng, i),
            _ => {
                if self.rng.chance(50) {
                    i.min()
                } else {
                    i.max()
                }
            }
        }
    }

    fn int_refutable(&mut self, i: IntTy, cx: &mut PatCx) -> String {
        // Every form below leaves out most of the type (a range spans at most nine values).
        cx.refuted = true;
        let v = self.int_pat_value(i);
        match self
            .rng
            .weighted(&[35, 30, 10, 15, if cx.bindings_ok { 10 } else { 0 }])
        {
            1 => self.int_range(i, v),
            2 if v < i.max() => {
                let hi = v
                    .saturating_add(i128::from(self.rng.range(1, 8)))
                    .min(i.max());
                format!("{}..{}", int_literal(i, v), int_literal(i, hi))
            }
            3 => {
                let mut values = vec![v];
                for _ in 0..self.rng.range(1, 2) {
                    let w = self.int_pat_value(i);
                    if !values.contains(&w) {
                        values.push(w);
                    }
                }
                let parts: Vec<String> = values.iter().map(|w| int_literal(i, *w)).collect();
                parts.join(" | ")
            }
            4 => {
                let range = self.int_range(i, v);
                let ty = ValTy::Prim(Prim::Int(i));
                self.at_pattern(ty, cx, false, |_, _| range)
            }
            // A literal (also when there is no room for an exclusive range).
            _ => int_literal(i, v),
        }
    }

    /// `lo..=hi` from `lo` up to a few values higher.
    fn int_range(&mut self, i: IntTy, lo: i128) -> String {
        let hi = lo
            .saturating_add(i128::from(self.rng.range(0, 8)))
            .min(i.max());
        format!("{}..={}", int_literal(i, lo), int_literal(i, hi))
    }

    fn char_refutable(&mut self, cx: &mut PatCx) -> String {
        cx.refuted = true;
        let a = *self.rng.pick(CHARS);
        let b = *self.rng.pick(CHARS);
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        match self
            .rng
            .weighted(&[40, 30, 20, if cx.bindings_ok { 10 } else { 0 }])
        {
            1 => format!("'{lo}'..='{hi}'"),
            2 if a != b => format!("'{a}' | '{b}'"),
            3 => {
                let ty = ValTy::Prim(Prim::Char);
                self.at_pattern(ty, cx, false, |_, _| format!("'{lo}'..='{hi}'"))
            }
            // A literal (also for an or-pattern of one character twice).
            _ => format!("'{a}'"),
        }
    }

    /// Shapes that together cover every value of `ty`.
    fn cover(&mut self, ty: ValTy, depth: u32) -> Vec<Shape> {
        let prog = self.prog;
        let shapes = match ty {
            ValTy::Prim(Prim::Bool) => {
                let first = self.rng.chance(50);
                vec![Shape::Bool(first), Shape::Bool(!first)]
            }
            ValTy::Prim(Prim::Int(i))
                // `usize`/`isize` have no fixed maximum for exhaustiveness (E0004).
                if !matches!(i, IntTy::Usize | IntTy::Isize) && self.rng.chance(50) =>
            {
                self.int_cover(i)
            }
            ValTy::Enum(e) => {
                let def = &prog.enums[e];
                let mut shapes = Vec::new();
                for (v, variant) in def.variants.iter().enumerate() {
                    let elems = variant.elems();
                    let mut refined = None;
                    if !elems.is_empty() && depth < 2 && self.rng.chance(30) {
                        let f = self.rng.index(elems.len());
                        let sub = self.cover(elems[f].1, depth + 1);
                        if sub.len() > 1 {
                            refined = Some((f, sub));
                        }
                    }
                    match refined {
                        Some((f, sub)) => shapes.extend(
                            sub.into_iter()
                                .map(|s| Shape::Variant(v, Some((f, Box::new(s))))),
                        ),
                        None => shapes.push(Shape::Variant(v, None)),
                    }
                }
                shapes
            }
            ValTy::Struct(_) | ValTy::Tuple(_) if depth < 2 && self.rng.chance(35) => {
                let elems: Vec<ValTy> = match ty {
                    ValTy::Struct(s) => prog.structs[s].fields.iter().map(|f| f.ty).collect(),
                    ValTy::Tuple(t) => prog.tuples[t].clone(),
                    _ => unreachable!(),
                };
                let f = self.rng.index(elems.len());
                let sub = self.cover(elems[f], depth + 1);
                if sub.len() > 1 {
                    sub.into_iter()
                        .map(|s| Shape::Field(f, Box::new(s)))
                        .collect()
                } else {
                    vec![Shape::Any]
                }
            }
            _ => vec![Shape::Any],
        };
        if shapes.len() > 10 {
            vec![Shape::Any]
        } else {
            shapes
        }
    }

    /// Two or three contiguous ranges from the type's minimum to its maximum.
    fn int_cover(&mut self, i: IntTy) -> Vec<Shape> {
        let mut cuts: Vec<i128> = Vec::new();
        for _ in 0..self.rng.range(1, 2) {
            let c = self.int_pat_value(i);
            if c > i.min() && !cuts.contains(&c) {
                cuts.push(c);
            }
        }
        cuts.sort_unstable();
        let mut bounds = vec![i.min()];
        bounds.extend(cuts.iter().copied());
        let mut shapes = Vec::new();
        for (k, &lo) in bounds.iter().enumerate() {
            let top = k + 1 == bounds.len();
            let hi = if top { i.max() } else { bounds[k + 1] - 1 };
            let hi_text = if top && i == IntTy::U128 {
                U128_MAX.to_owned()
            } else {
                int_literal(i, hi)
            };
            let text = if lo == hi && !(top && i == IntTy::U128) {
                int_literal(i, lo)
            } else if !top && self.rng.chance(30) {
                format!("{}..{}", int_literal(i, lo), int_literal(i, hi + 1))
            } else {
                format!("{}..={hi_text}", int_literal(i, lo))
            };
            shapes.push(Shape::Range(text));
        }
        shapes
    }

    /// Renders a shape of a cover (or a refutable pattern for `Shape::Refutable`).
    fn render_shape(
        &mut self,
        shape: &Shape,
        ty: ValTy,
        cx: &mut PatCx,
        in_drop: bool,
        depth: u32,
    ) -> String {
        match shape {
            Shape::Any => self.irrefutable(ty, cx, in_drop, depth),
            Shape::Bool(b) => b.to_string(),
            Shape::Refutable => self.refutable(ty, cx, in_drop, depth),
            Shape::Range(text) => {
                if cx.bindings_ok && self.rng.chance(15) {
                    let text = text.clone();
                    self.at_pattern(ty, cx, in_drop, |_, _| text)
                } else {
                    text.clone()
                }
            }
            Shape::Variant(..) | Shape::Field(..) => {
                self.render_composite(ty, shape, cx, in_drop, depth)
            }
        }
    }

    /// The pattern context of an arm: the first arm may break the rules as the scrutinee
    /// allows; a later one may be unreachable, so it gets the strict permissions.
    fn arm_cx(s: &Scrut, first: bool) -> PatCx {
        let mut cx = Self::pat_cx(s);
        if let (false, Some((can_move, can_ref_mut))) = (first, s.strict) {
            cx.can_move = can_move;
            cx.can_ref_mut = can_ref_mut;
        }
        cx
    }

    /// The arms of a `match` on `s`: optional refutable or guarded arms, then a cover.
    /// Patterns after the first are generated without violations.
    fn arms(&mut self, s: &Scrut) -> Vec<Arm> {
        let mut arms: Vec<Arm> = Vec::new();
        for _ in 0..self.rng.range(0, 2) {
            let first = arms.is_empty();
            let mut cx = Self::arm_cx(s, first);
            self.no_violations += u32::from(!first);
            let pat = if self.rng.chance(25) {
                self.irrefutable(s.ty, &mut cx, false, 0)
            } else {
                self.refutable(s.ty, &mut cx, false, 0)
            };
            self.no_violations -= u32::from(!first);
            // Always guarded, so that no later arm is unreachable (an unguarded `E::A(..)`
            // would make a cover's `E::A(x)` so).
            arms.push(Arm {
                pat,
                cx,
                guard: Some(true),
                first,
            });
        }
        let mut shapes = if self.rng.chance(30) {
            vec![Shape::Any]
        } else {
            self.cover(s.ty, 0)
        };
        if shapes.len() > 1 && self.rng.chance(30) {
            shapes.reverse();
        }
        if shapes.len() > 1 && self.rng.chance(25) {
            let keep = self.rng.index(shapes.len());
            shapes.truncate(keep);
            shapes.push(Shape::Any);
        }
        let mut i = 0;
        while i < shapes.len() {
            let first = arms.is_empty();
            let mut cx = Self::arm_cx(s, first);
            self.no_violations += u32::from(!first);
            let merge =
                i + 1 < shapes.len() && !matches!(shapes[i + 1], Shape::Any) && self.rng.chance(20);
            let pat = if merge {
                cx.bindings_ok = false;
                let a = self.render_shape(&shapes[i].clone(), s.ty, &mut cx, false, 0);
                let b = self.render_shape(&shapes[i + 1].clone(), s.ty, &mut cx, false, 0);
                cx.bindings_ok = true;
                i += 2;
                format!("{a} | {b}")
            } else {
                let shape = shapes[i].clone();
                i += 1;
                self.render_shape(&shape, s.ty, &mut cx, false, 0)
            };
            self.no_violations -= u32::from(!first);
            arms.push(Arm {
                pat,
                cx,
                guard: None,
                first,
            });
        }
        arms
    }

    // ----- bindings and guards -----------------------------------------------------------

    /// Declares a pattern's bindings in the current block and records its move; returns
    /// the indices of the new locals.
    fn bind_all(&mut self, s: &Scrut, cx: &PatCx) -> Vec<usize> {
        if cx.moved {
            if let Some(root) = s.place_root {
                self.vars[root].moved = true;
            }
        }
        let mut out = Vec::new();
        for b in &cx.binds {
            let v = match b.ty {
                Ty::Ref { mutable, .. } => {
                    let roots = s.roots.iter().map(|&r| (r, mutable)).collect();
                    let e = Expr {
                        text: String::new(),
                        roots,
                    };
                    self.bind_ref(b.name.clone(), b.ty, &e)
                }
                Ty::Val(_) => self.declare(b.name.clone(), b.ty, b.mutable, false),
            };
            out.push(v);
        }
        out
    }

    /// ` if cond`: reads primitive places (bindings preferred) and compares them with
    /// literals; nothing is moved, written or called, and the scrutinee is locked as the
    /// guard's fake borrow locks it.
    fn guard(&mut self, s: &Scrut, binds: &[usize]) -> String {
        self.lock(&s.guard_lock);
        let saved = self.prefer_root;
        if !binds.is_empty() {
            self.prefer_root = Some(binds[self.rng.index(binds.len())]);
        }
        let text = match self.rng.weighted(&[60, 20, 20]) {
            0 => self.guard_compare(),
            1 => self.prim_leaf(Prim::Bool).text,
            _ => {
                let a = self.guard_compare();
                let b = self.guard_compare();
                let op = *self.rng.pick(&["&&", "||"]);
                format!("({a} {op} {b})")
            }
        };
        self.prefer_root = saved;
        self.unlock(&s.guard_lock);
        format!(" if {text}")
    }

    fn guard_compare(&mut self) -> String {
        let prims: Vec<Place> = self
            .candidates()
            .into_iter()
            .filter(|p| matches!(p.ty, ValTy::Prim(_)) && self.permitted(p, Access::Read))
            .collect();
        if prims.is_empty() {
            return self.prim_leaf(Prim::Bool).text;
        }
        let mut pool = prims;
        if let Some(root) = self.prefer_root {
            if pool.iter().any(|p| p.root == root) && self.rng.chance(70) {
                pool.retain(|p| p.root == root);
            }
        }
        let place = self.rng.pick(&pool).clone();
        let ValTy::Prim(p) = place.ty else {
            unreachable!()
        };
        let op = *self.rng.pick(&["==", "!=", "<", "<=", ">", ">="]);
        let rhs = self.literal(p).text;
        format!("({} {op} {rhs})", place.text)
    }

    /// A statement or value that uses one of the new bindings, so that what a pattern
    /// bound shows in the output.
    fn binding_use(&self, binds: &[usize]) -> Option<String> {
        let usable: Vec<usize> = binds
            .iter()
            .copied()
            .filter(|&v| {
                let var = &self.vars[v];
                !var.dead && !var.moved && var.mut_locks == 0
            })
            .collect();
        let &v = usable.first()?;
        let var = &self.vars[v];
        let (base, inner) = match var.ty {
            Ty::Val(t) => (var.name.clone(), t),
            Ty::Ref { inner, .. } => (format!("*{}", var.name), inner),
        };
        match inner {
            ValTy::Enum(_) => None,
            _ => Some(self.show_line(&base, inner)),
        }
    }

    // ----- match statements ----------------------------------------------------------------

    /// `match s { arms }` as a statement: each arm a block of statements or a one-line
    /// unit expression.
    pub(super) fn stmt_match(&mut self, depth: u32) -> bool {
        let ty = self.matchable_ty();
        let s = self.scrutinee(ty, depth.saturating_sub(1));
        let arms = self.arms(&s);
        self.line(&format!("match {} {{", s.text));
        self.indent += 1;
        let before = self.snapshot_moved();
        let mut after = vec![false; before.len()];
        for arm in arms {
            self.no_violations += u32::from(!arm.first);
            self.restore_moved(&before);
            self.open_block();
            let binds = self.bind_all(&s, &arm.cx);
            let guard = match arm.guard {
                Some(forced) if forced || self.rng.chance(30) => self.guard(&s, &binds),
                _ => String::new(),
            };
            let head = format!("{}{guard} => ", arm.pat);
            if self.remaining() > 4 && self.rng.chance(55) {
                self.indent -= 1;
                self.line(&format!("{head}{{"));
                self.indent += 1;
                if let Some(text) = self.binding_use(&binds) {
                    if self.rng.chance(60) {
                        self.line(&text);
                    }
                }
                let n = self.rng.range(1, 2);
                for _ in 0..n {
                    if self.remaining() <= 2 {
                        break;
                    }
                    self.stmt();
                }
                self.close_block();
                self.line("}");
            } else {
                let body = self.arm_unit_expr(&binds, depth);
                self.close_block();
                self.line(&format!("{head}{body},"));
            }
            let end = self.snapshot_moved();
            self.no_violations -= u32::from(!arm.first);
            for (a, e) in after.iter_mut().zip(end) {
                *a |= e;
            }
        }
        self.restore_moved(&after);
        self.indent -= 1;
        self.line("}");
        true
    }

    /// A one-line arm of unit type: a print (of a binding, preferably), `{}` or a panic.
    fn arm_unit_expr(&mut self, binds: &[usize], depth: u32) -> String {
        match self.rng.weighted(&[60, 20, 3]) {
            0 => {
                let prims: Vec<usize> = binds
                    .iter()
                    .copied()
                    .filter(|&v| {
                        let var = &self.vars[v];
                        matches!(
                            var.ty,
                            Ty::Val(ValTy::Prim(_))
                                | Ty::Ref {
                                    inner: ValTy::Prim(_),
                                    ..
                                }
                        ) && !var.moved
                            && var.mut_locks == 0
                    })
                    .collect();
                let text = if !prims.is_empty() && self.rng.chance(70) {
                    let var = &self.vars[prims[0]];
                    if var.ty.is_ref() {
                        format!("*{}", var.name)
                    } else {
                        var.name.clone()
                    }
                } else {
                    let p = crate::program::random_prim(self.rng);
                    self.prim_expr(p, depth.saturating_sub(1)).text
                };
                format!("PrintLine({text})")
            }
            1 => "{}".to_owned(),
            _ => "Panic(\"generated arm\")".to_owned(),
        }
    }

    /// `let v: T = match s { arms };`: each arm a value, a block of statements ending in
    /// one, or (rarely) a panic.
    pub(super) fn stmt_let_match(&mut self, depth: u32) -> bool {
        let ty = self.matchable_ty();
        let s = self.scrutinee(ty, depth.saturating_sub(1));
        let value_ty = self.random_val_ty();
        let arms = self.arms(&s);
        let name = self.fresh_name();
        let mutable = self.rng.chance(40);
        let header_at = self.lines.len();
        self.line("");
        self.indent += 1;
        let before = self.snapshot_moved();
        let mut after = vec![false; before.len()];
        let mut diverges = false;
        for arm in arms {
            self.no_violations += u32::from(!arm.first);
            self.restore_moved(&before);
            self.open_block();
            let binds = self.bind_all(&s, &arm.cx);
            let guard = match arm.guard {
                Some(forced) if forced || self.rng.chance(30) => self.guard(&s, &binds),
                _ => String::new(),
            };
            let head = format!("{}{guard} => ", arm.pat);
            match self
                .rng
                .weighted(&[if self.remaining() > 4 { 40 } else { 0 }, 55, 3])
            {
                0 => {
                    self.indent -= 1;
                    self.line(&format!("{head}{{"));
                    self.indent += 1;
                    if let Some(text) = self.binding_use(&binds) {
                        if self.rng.chance(50) {
                            self.line(&text);
                        }
                    }
                    let n = self.rng.range(1, 2);
                    for _ in 0..n {
                        if self.remaining() <= 2 {
                            break;
                        }
                        self.stmt();
                    }
                    self.expire_refs();
                    self.end_block_regions();
                    let v = self.arm_value(value_ty, &binds, depth);
                    self.line(&v);
                    self.close_block();
                    self.line("}");
                }
                1 => {
                    let v = self.arm_value(value_ty, &binds, depth);
                    self.close_block();
                    self.line(&format!("{head}{v},"));
                }
                _ => {
                    diverges = true;
                    self.close_block();
                    self.line(&format!("{head}Panic(\"generated arm\"),"));
                }
            }
            let end = self.snapshot_moved();
            self.no_violations -= u32::from(!arm.first);
            for (a, e) in after.iter_mut().zip(end) {
                *a |= e;
            }
        }
        self.restore_moved(&after);
        self.indent -= 1;
        self.line("};");
        let annotation = if diverges || self.rng.chance(50) {
            format!(": {}", self.prog.val_name(value_ty))
        } else {
            String::new()
        };
        let header = format!(
            "{}let {}{name}{annotation} = match {} {{",
            super::INDENT.repeat(self.indent),
            if mutable { "mut " } else { "" },
            s.text
        );
        self.lines[header_at] = header;
        self.declare(name, Ty::Val(value_ty), mutable, false);
        true
    }

    /// Ends the regions of the reference locals of the current block: used before a block's
    /// tail value, after which none of them is used.
    fn end_block_regions(&mut self) {
        let first = self.blocks[self.cur_block()].first_var;
        for v in first..self.vars.len() {
            if self.vars[v].expires.is_some() {
                self.kill(v);
            }
        }
    }

    /// The value of a match arm: a binding of the right type now and then, else any
    /// expression of the type.
    fn arm_value(&mut self, ty: ValTy, binds: &[usize], depth: u32) -> String {
        let direct: Vec<String> = binds
            .iter()
            .filter_map(|&v| {
                let var = &self.vars[v];
                if var.dead || var.moved || var.mut_locks > 0 {
                    return None;
                }
                match var.ty {
                    Ty::Val(t) if t == ty => Some(var.name.clone()),
                    Ty::Ref { inner, .. } if inner == ty && self.prog.is_copy(Ty::Val(ty)) => {
                        Some(format!("*{}", var.name))
                    }
                    _ => None,
                }
            })
            .collect();
        if !direct.is_empty() && self.rng.chance(40) {
            let text = self.rng.pick(&direct).clone();
            // A non-`Copy` binding is moved into the result.
            if let Some(&v) = binds.iter().find(|&&v| self.vars[v].name == text) {
                if !self.prog.is_copy(self.vars[v].ty) {
                    self.vars[v].moved = true;
                }
            }
            return text;
        }
        self.val_expr(ty, depth.saturating_sub(1)).text
    }

    /// An inline `(match s { p => v, .. })` or `(if let p = s { v } else { v })` of type
    /// `ty`, on one line.
    pub(super) fn inline_match(&mut self, ty: ValTy, depth: u32) -> Expr {
        let sty = self.matchable_ty();
        let s = self.scrutinee(sty, depth.saturating_sub(2));
        let before = self.snapshot_moved();
        let mut after = vec![false; before.len()];
        let mut parts = Vec::new();
        let mut if_let = None;
        if self.rng.chance(30) {
            // An `if let` needs a pattern that surely refutes, or its `else` is
            // unreachable; otherwise this is a `match`.
            let mut cx = Self::pat_cx(&s);
            let pat = self.refutable(s.ty, &mut cx, false, 0);
            if cx.refuted {
                if_let = Some((pat, cx));
            }
        }
        let is_if_let = if_let.is_some();
        let arms = if let Some((pat, cx)) = if_let {
            vec![
                Arm {
                    pat,
                    cx,
                    guard: None,
                    first: true,
                },
                Arm {
                    pat: String::new(),
                    cx: Self::pat_cx(&s),
                    guard: None,
                    first: false,
                },
            ]
        } else {
            self.arms(&s)
        };
        for arm in &arms {
            self.no_violations += u32::from(!arm.first);
            self.restore_moved(&before);
            self.open_block();
            let binds = self.bind_all(&s, &arm.cx);
            let guard = match arm.guard {
                Some(forced) if forced || self.rng.chance(30) => self.guard(&s, &binds),
                _ => String::new(),
            };
            let v = self.arm_value(ty, &binds, depth);
            self.close_block();
            parts.push((format!("{}{guard}", arm.pat), v));
            let end = self.snapshot_moved();
            self.no_violations -= u32::from(!arm.first);
            for (a, e) in after.iter_mut().zip(end) {
                *a |= e;
            }
        }
        self.restore_moved(&after);
        let text = if is_if_let {
            format!(
                "(if let {} = {} {{ {} }} else {{ {} }})",
                parts[0].0, s.text, parts[0].1, parts[1].1
            )
        } else {
            let arms: Vec<String> = parts.iter().map(|(p, v)| format!("{p} => {v}")).collect();
            format!("(match {} {{ {} }})", s.text, arms.join(", "))
        };
        Expr::value(text)
    }

    // ----- if let, while let, let-else, destructuring let -------------------------------

    /// `if let p = s { .. } [else { .. }]`.
    pub(super) fn stmt_if_let(&mut self, depth: u32) -> bool {
        let ty = self.matchable_ty();
        let s = self.scrutinee(ty, depth.saturating_sub(1));
        let mut cx = Self::pat_cx(&s);
        let pat = self.refutable(ty, &mut cx, false, 0);
        if !cx.refuted {
            // An irrefutable `if let` makes its `else` (or fall-through) unreachable.
            return false;
        }
        let before = self.snapshot_moved();
        self.line(&format!("if let {pat} = {} {{", s.text));
        self.open_block();
        let binds = self.bind_all(&s, &cx);
        if let Some(text) = self.binding_use(&binds) {
            if self.rng.chance(60) {
                self.line(&text);
            }
        }
        let n = self.rng.range(1, 2);
        for _ in 0..n {
            if self.remaining() <= 2 {
                break;
            }
            self.stmt();
        }
        self.close_block();
        let after_then = self.snapshot_moved();
        if self.rng.chance(60) {
            self.restore_moved(&before);
            self.line("} else {");
            self.strictly(|gen| gen.block_body(2));
            self.join_moved(&after_then);
        } else {
            self.join_moved(&before);
        }
        self.line("}");
        true
    }

    /// `while let p = st { k += 1; if k > n { break; } ..; st = e; }` over a fresh state
    /// local `st` of an enum type, matched by value (copies and `ref` bindings only) or
    /// through `&st`/`&mut st`; the counter bounds the loop whatever the body does.
    pub(super) fn stmt_while_let(&mut self, depth: u32) -> bool {
        if self.prog.enums.is_empty() {
            return false;
        }
        let e = self.rng.index(self.prog.enums.len());
        let ty = ValTy::Enum(e);
        let k = self.counter();
        let init = self.enum_expr(e, depth.saturating_sub(1));
        let name = self.fresh_name();
        self.line(&format!(
            "let mut {name}: {} = {};",
            self.prog.val_name(ty),
            init.text
        ));
        let st = self.declare(name.clone(), Ty::Val(ty), true, false);
        let s = match self.rng.weighted(&[40, 30, 30]) {
            0 => Scrut {
                text: name.clone(),
                ty,
                mode: Mode::Move,
                place_root: Some(st),
                roots: vec![st],
                guard_lock: vec![(st, false)],
                can_move: false,
                can_ref: true,
                can_ref_mut: true,
                strict: None,
            },
            kind => {
                let mutable = kind == 2;
                Scrut {
                    text: format!("&{}{name}", if mutable { "mut " } else { "" }),
                    ty,
                    mode: Mode::Ref(mutable),
                    place_root: None,
                    roots: vec![st],
                    guard_lock: vec![(st, mutable)],
                    can_move: false,
                    can_ref: false,
                    can_ref_mut: false,
                    strict: None,
                }
            }
        };
        let mut cx = Self::pat_cx(&s);
        let pat = self.refutable(ty, &mut cx, false, 0);
        let bound = self.rng.range(1, 4);
        let before = self.snapshot_moved();
        self.loops.push(LoopKind::Stmt);
        self.line(&format!("while let {pat} = {} {{", s.text));
        self.open_block();
        self.bind_all(&s, &cx);
        self.line(&format!("{k} += 1i32;"));
        self.line(&format!("if {k} > {bound}i32 {{ break; }}"));
        let n = self.rng.range(1, 2);
        for _ in 0..n {
            if self.remaining() <= 2 {
                break;
            }
            self.stmt();
        }
        // Nothing of the body is used after this point: its regions end, and the state
        // moves on (unless the body moved or borrowed it in a way that forbids the write).
        let first = self.blocks[self.cur_block()].first_var;
        for v in first..self.vars.len() {
            self.kill(v);
        }
        let place = Place {
            text: name.clone(),
            ty,
            root: st,
            via_ref: None,
            whole: true,
            in_drop: false,
        };
        let next = self.enum_expr(e, depth.saturating_sub(1));
        if self.permitted(&place, Access::Write) {
            self.apply(&place, Access::Write);
            self.line(&format!("{name} = {};", next.text));
        }
        self.close_block();
        self.loops.pop();
        self.join_moved(&before);
        self.line("}");
        true
    }

    /// The statement an `else` of a `let`-`else` diverges with: `return`, `break`,
    /// `continue` or a panic.
    fn diverge(&mut self, depth: u32) -> Option<String> {
        let kind = self.loops.last().copied();
        match self
            .rng
            .weighted(&[35, 25, if kind.is_some() { 40 } else { 0 }])
        {
            0 => self.return_text(depth),
            1 => Some("Panic(\"let-else\");".to_owned()),
            _ => Some(match kind.expect("in a loop") {
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
                        let v = self.val_expr(ty, depth.saturating_sub(1));
                        format!("break {};", v.text)
                    }
                }
            }),
        }
    }

    /// `let p = s else { diverge };`, the bindings in scope afterwards.
    pub(super) fn stmt_let_else(&mut self, depth: u32) -> bool {
        let ty = self.matchable_ty();
        let s = self.scrutinee(ty, depth.saturating_sub(1));
        let mut cx = Self::pat_cx(&s);
        let pat = let_pattern(self.refutable(ty, &mut cx, false, 0));
        if !cx.refuted {
            return false;
        }
        let Some(diverge) = self.strictly(|gen| gen.diverge(depth)) else {
            return false;
        };
        self.line(&format!("let {pat} = {} else {{", s.text));
        self.indent += 1;
        if self.rng.chance(30) {
            self.line("PrintLine(\"else\");");
        }
        self.line(&diverge);
        self.indent -= 1;
        self.line("};");
        self.bind_all(&s, &cx);
        true
    }

    /// `let p = s;` with an irrefutable pattern destructuring a struct, a tuple or a
    /// single-variant enum (by value, `ref`, or through a reference).
    pub(super) fn stmt_let_destructure(&mut self, depth: u32) -> bool {
        let prog = self.prog;
        let mut types: Vec<ValTy> = (0..prog.enums.len())
            .filter(|&e| prog.enums[e].variants.len() == 1)
            .map(ValTy::Enum)
            .collect();
        types.extend((0..prog.structs.len()).map(ValTy::Struct));
        if types.is_empty() {
            return false;
        }
        let ty = *self.rng.pick(&types);
        let s = self.scrutinee(ty, depth.saturating_sub(1));
        let mut cx = Self::pat_cx(&s);
        let pat = self.destructure(ty, &mut cx, false, 0);
        self.line(&format!("let {pat} = {};", s.text));
        self.bind_all(&s, &cx);
        true
    }

    /// Prints an enum value through a reference (`reference` is a `&E` or `&mut E`
    /// expression): the variant's name and its primitive payload elements.
    pub(super) fn show_enum(&mut self, reference: &str, e: usize) {
        let prog = self.prog;
        let def = &prog.enums[e];
        self.line(&format!("match {reference} {{"));
        self.indent += 1;
        for v in &def.variants {
            self.line(&def.print_arm(v, "shown", ""));
        }
        self.indent -= 1;
        self.line("}");
    }
}
