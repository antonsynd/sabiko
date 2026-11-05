//! Strings (#10): `&str` locals, parameters and returns, ASCII string literals (some with
//! escapes), range slices of strings in all six forms (`&s[1..]`, `&"abc"[..k]`,
//! `&s[i..][..2]`, with constant and computed bounds, sometimes out of range or
//! overflowing), `.len()` of strings and of their range places, the six comparisons
//! between `&str` operands, and `match`, `if let` and `let`-`else` on a `&str` with literal
//! patterns, or-patterns, bindings, guards and a catch-all; loops walking a string by index
//! (`while i < s.len() { .. &s[i..=i] .. i += 1usize; }`) and loops shrinking a `mut` one
//! (`while t.len() > 0usize { ..; t = &t[1..]; }`, `while let "a" | "b" = t { .. }`).
//!
//! A `&str` is a shared reference ([`Ty::STR`]) to the unsized [`ValTy::Str`], which only
//! a reference local or parameter `s` holds: the place `*s`, sliced as `s[a..b]` and
//! measured as `s.len()`. Nothing writes through one and nothing borrows a `&str` local
//! itself, so a string's loans only matter where it may carry another loan: a `&str`
//! returned by a method taking `&mut self` keeps the receiver borrowed, and a local bound
//! to it gets a region (`bind_ref`). A `&str` made of literals and plain `&str` locals is a
//! plain local that lives to the end of its block, as rustc lets it (a slice of `*s`
//! borrows the string, not `s`, so it may outlive `s` and `s` may be reassigned meanwhile).
//!
//! Every `match` on a string is exhaustive by its last arm (`_` or a binding), every other
//! arm's literals are distinct from all others and a binding before the last arm is
//! guarded, so no arm is unreachable (see the `matches` module on TODO(#47)).
//!
//! Not generated: TODO(#11) `&&str` (E0500 until #11): no `&s` of a `&str` local and no
//! `&str` behind another reference or in a reference parameter's referent. TODO(#43)
//! `&str` in structs, enums and arrays (and so in their patterns), and in the generator's
//! tuple types. TODO(E0208) comparisons of `str` places (`*s == *t`): only `&str`
//! operands are compared. Ranges as values (E0107 outside an index), `&mut str`, and
//! non-ASCII text (see `STR_LITERALS` and decision 62).

use super::{BodyGen, Expr, LoopKind};
use crate::program::{str_literal, RetTy, STR_ESCAPED, STR_LITERALS};
use crate::ty::{IntTy, Prim, Ty, ValTy};

const USIZE: Prim = Prim::Int(IntTy::Usize);

/// An unsized string place to slice or measure.
#[derive(Debug, Clone)]
struct StrBase {
    /// `s` (the place `*s` of a `&str` local), a literal, a call returning a `&str`, or a
    /// range of one of those (`s[1..]`).
    text: String,
    roots: Vec<(usize, bool)>,
    /// The length, when it is known (a literal's).
    len: Option<usize>,
    /// Whether `text.len()` may be evaluated again (not a call).
    measurable: bool,
}

/// One arm of a `match` on a string, before its guard and body are generated.
#[derive(Debug, Clone)]
struct StrArm {
    /// Literal patterns (an or-pattern of them), or a binding.
    pat: String,
    binding: Option<String>,
    guard: bool,
}

impl BodyGen<'_> {
    /// The functions (and methods) returning a `&str`.
    fn str_fns(&self) -> Vec<usize> {
        self.callables(|r| {
            matches!(
                r,
                RetTy::Ref {
                    inner: ValTy::Str,
                    ..
                }
            )
        })
    }

    fn has_str_vars(&self) -> bool {
        self.vars
            .iter()
            .any(|v| v.ty == Ty::STR && !v.dead && !v.hidden)
    }

    fn str_lit(&mut self) -> Expr {
        Expr::value(str_literal(self.rng).0)
    }

    /// A string to slice or measure: a `&str` local, a literal, a call returning a `&str`
    /// or (one level) a range of one of those.
    fn str_base(&mut self, depth: u32) -> StrBase {
        let fns = self.str_fns();
        let calls = !fns.is_empty() && depth > 0 && self.simple == 0;
        let choice = self.rng.weighted(&[
            30,
            if self.has_str_vars() { 45 } else { 0 },
            if calls { 15 } else { 0 },
            if self.range_depth == 0 { 12 } else { 0 },
        ]);
        match choice {
            1 => {
                if let Some(e) = self.ref_var(false, ValTy::Str) {
                    return StrBase {
                        text: e.text,
                        roots: e.roots,
                        len: None,
                        measurable: true,
                    };
                }
            }
            2 => {
                let callee = *self.rng.pick(&fns);
                if let Some(e) = self.call(callee, depth) {
                    return StrBase {
                        text: e.text,
                        roots: e.roots,
                        len: None,
                        measurable: false,
                    };
                }
            }
            3 => {
                self.range_depth += 1;
                let inner = self.str_base(depth.saturating_sub(1));
                self.range_depth -= 1;
                let range = self.str_range(&inner);
                return StrBase {
                    text: format!("{}[{range}]", inner.text),
                    roots: inner.roots,
                    len: None,
                    measurable: inner.measurable,
                };
            }
            _ => {}
        }
        let (text, len) = str_literal(self.rng);
        StrBase {
            text,
            roots: Vec::new(),
            len: Some(len),
            measurable: true,
        }
    }

    /// A range to slice `base` with; the base's loans are held while the bounds are
    /// generated (they are evaluated after it).
    fn str_range(&mut self, base: &StrBase) -> String {
        self.lock(&base.roots);
        self.range_depth += 1;
        let range = self.range_index(&base.text, base.len, base.measurable);
        self.range_depth -= 1;
        self.unlock(&base.roots);
        range
    }

    /// An expression of type `&str`: a literal, a `&str` local, a range slice
    /// (`&s[1..k]`), a call returning a `&str` or an `if` choosing between two. Its roots
    /// are the loans it may carry. Without calls in `simple` mode.
    pub(super) fn str_expr(&mut self, depth: u32) -> Expr {
        let fns = self.str_fns();
        let full = depth > 0 && self.simple == 0;
        match self.rng.weighted(&[
            22,
            if self.has_str_vars() { 28 } else { 0 },
            34,
            if full && !fns.is_empty() { 10 } else { 0 },
            if full { 6 } else { 0 },
        ]) {
            0 => self.str_lit(),
            1 => self
                .ref_var(false, ValTy::Str)
                .unwrap_or_else(|| self.str_lit()),
            2 => {
                let base = self.str_base(depth);
                let range = self.str_range(&base);
                Expr {
                    text: format!("&{}[{range}]", base.text),
                    roots: base.roots,
                }
            }
            3 => {
                let callee = *self.rng.pick(&fns);
                self.call(callee, depth).unwrap_or_else(|| self.str_lit())
            }
            _ => self.str_if(depth),
        }
    }

    /// `(if c { a } else { b })` of type `&str`, carrying the loans of both.
    fn str_if(&mut self, depth: u32) -> Expr {
        let c = self.prim_expr(Prim::Bool, depth.saturating_sub(1));
        let before = self.snapshot_moved();
        let a = self.str_expr(depth.saturating_sub(1));
        let after_a = self.snapshot_moved();
        self.restore_moved(&before);
        self.lock(&a.roots);
        let b = self.str_expr(depth.saturating_sub(1));
        self.unlock(&a.roots);
        self.join_moved(&after_a);
        let mut roots = a.roots;
        roots.extend(b.roots);
        Expr {
            text: format!("(if {} {{ {} }} else {{ {} }})", c.text, a.text, b.text),
            roots,
        }
    }

    /// The length of a string: `s.len()`, `"abc".len()`, `F(s).len()`, `s[1..].len()`.
    pub(super) fn str_len_expr(&mut self) -> Expr {
        let base = self.str_base(1);
        if self.range_depth == 0 && self.rng.chance(30) {
            let range = self.str_range(&base);
            return Expr::value(format!("{}[{range}].len()", base.text));
        }
        Expr::value(format!("{}.len()", base.text))
    }

    /// `(a op b)` between two `&str` operands, the left one's loans held while the right
    /// one is generated (both are alive until the comparison).
    pub(super) fn str_compare(&mut self, op: &str, depth: u32) -> Expr {
        let a = self.str_expr(depth);
        self.lock(&a.roots);
        let b = self.str_expr(depth);
        self.unlock(&a.roots);
        Expr::value(format!("({} {op} {})", a.text, b.text))
    }

    /// Whether a `&str` value carries no loan but those of plain `&str` locals: a local
    /// bound to it needs no region.
    fn is_plain_str(&self, e: &Expr) -> bool {
        e.roots.iter().all(|&(v, mutable)| {
            !mutable && self.vars[v].ty == Ty::STR && self.vars[v].expires.is_none()
        })
    }

    /// Declares a `&str` local bound to `e`: plain, or with a region over `e`'s loans.
    fn bind_str(&mut self, name: String, mutable: bool, e: &Expr) -> usize {
        if self.is_plain_str(e) {
            self.declare(name, Ty::STR, mutable, false)
        } else {
            let v = self.bind_ref(name, Ty::STR, e);
            self.vars[v].mutable = mutable;
            v
        }
    }

    /// `let s: &str = e;` (the annotation now and then left out).
    pub(super) fn stmt_let_str(&mut self, depth: u32) -> bool {
        let e = self.str_expr(depth.saturating_sub(1));
        let name = self.fresh_name();
        let annotation = if self.rng.chance(50) { ": &str" } else { "" };
        self.line(&format!("let {name}{annotation} = {};", e.text));
        self.bind_str(name, false, &e);
        true
    }

    // ----- matching ----------------------------------------------------------------------

    /// `k` distinct literals for a pattern, none of them in `used` (which gets them): an
    /// arm's literals never repeat an earlier arm's, so no arm is unreachable. With
    /// `nonempty`, never `""`.
    fn pattern_literals(
        &mut self,
        k: usize,
        used: &mut Vec<&'static str>,
        nonempty: bool,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..k * 4 {
            if out.len() == k {
                break;
            }
            let pool = if self.rng.chance(10) {
                STR_ESCAPED
            } else {
                STR_LITERALS
            };
            let src = *self.rng.pick(pool);
            if used.contains(&src) || (nonempty && src.is_empty()) {
                continue;
            }
            used.push(src);
            out.push(format!("\"{src}\""));
        }
        if out.is_empty() {
            // Every try was taken: a literal no pool holds.
            out.push(format!("\"{}\"", "z".repeat(used.len() + 2)));
        }
        out
    }

    /// The arms of a `match` on a string: one to three arms of literal patterns (an
    /// or-pattern now and then, a guard now and then) and, with `bindings`, guarded
    /// bindings; then a catch-all, `_` or (with `bindings`) a binding.
    fn str_arms(&mut self, bindings: bool) -> Vec<StrArm> {
        let mut used = Vec::new();
        let n = self.rng.range(1, 3);
        let mut arms = Vec::new();
        for _ in 0..n {
            let kind = self.rng.weighted(&[50, 30, if bindings { 20 } else { 0 }]);
            if kind == 2 {
                let name = self.fresh_name();
                arms.push(StrArm {
                    pat: name.clone(),
                    binding: Some(name),
                    guard: true,
                });
            } else {
                let k = if kind == 0 {
                    1
                } else {
                    usize::try_from(self.rng.range(2, 3)).expect("small")
                };
                let lits = self.pattern_literals(k, &mut used, false);
                arms.push(StrArm {
                    pat: lits.join(" | "),
                    binding: None,
                    guard: self.rng.chance(25),
                });
            }
        }
        if bindings && self.rng.chance(40) {
            let name = self.fresh_name();
            arms.push(StrArm {
                pat: name.clone(),
                binding: Some(name),
                guard: false,
            });
        } else {
            arms.push(StrArm {
                pat: "_".to_owned(),
                binding: None,
                guard: false,
            });
        }
        arms
    }

    /// ` if cond` for an arm: about the binding (its length, a comparison with a literal or
    /// another string) or a comparison of leaves. Nothing is called, moved or written.
    fn str_guard(&mut self, binding: Option<&str>) -> String {
        self.simple += 1;
        let op = *self.rng.pick(&["==", "!=", "<", "<=", ">", ">="]);
        let text = match binding {
            Some(b) if self.rng.chance(80) => match self.rng.weighted(&[40, 30, 30]) {
                0 => {
                    let k = self.rng.range(0, 3);
                    format!("{b}.len() > {k}usize")
                }
                1 => format!("{b} {op} {}", str_literal(self.rng).0),
                _ => {
                    let other = self.str_expr(0);
                    format!("{b} {op} {}", other.text)
                }
            },
            _ => {
                if self.rng.chance(50) {
                    self.prim_leaf(Prim::Bool).text
                } else {
                    let a = self.prim_leaf(USIZE).text;
                    let k = self.rng.range(0, 4);
                    format!("{a} {op} {k}usize")
                }
            }
        };
        self.simple -= 1;
        format!(" if {text}")
    }

    /// A one-line arm of unit type: a print (of the binding, preferably), `{}` or a panic.
    fn str_arm_unit(&mut self, binding: Option<&str>, depth: u32) -> String {
        match self.rng.weighted(&[60, 20, 3]) {
            0 => match binding {
                Some(b) if self.rng.chance(70) => format!("PrintLine({b})"),
                _ => {
                    if self.rng.chance(40) {
                        let e = self.str_expr(depth.saturating_sub(1));
                        format!("PrintLine({})", e.text)
                    } else {
                        let p = crate::program::random_prim(self.rng);
                        let e = self.prim_expr(p, depth.saturating_sub(1));
                        format!("PrintLine({})", e.text)
                    }
                }
            },
            1 => "{}".to_owned(),
            _ => "Panic(\"generated arm\")".to_owned(),
        }
    }

    /// `match s { arms }` on a string, as a statement.
    pub(super) fn stmt_str_match(&mut self, depth: u32) -> bool {
        let s = self.str_expr(depth.saturating_sub(1));
        self.str_match_on(&s, depth);
        true
    }

    /// A `match` statement on the `&str` expression `s`, whose loans are held throughout
    /// (a binding may carry them into its arm). Each arm is a block of statements or a
    /// one-line unit expression; checks are only skipped in the first arm.
    fn str_match_on(&mut self, s: &Expr, depth: u32) {
        let arms = self.str_arms(true);
        self.lock(&s.roots);
        self.line(&format!(
            "match {} {{",
            super::matches::scrutinee_text(s.text.clone())
        ));
        self.indent += 1;
        let before = self.snapshot_moved();
        let mut after = vec![false; before.len()];
        for (i, arm) in arms.into_iter().enumerate() {
            let first = i == 0;
            self.no_violations += u32::from(!first);
            self.restore_moved(&before);
            self.open_block();
            if let Some(name) = &arm.binding {
                self.bind_str(name.clone(), false, s);
            }
            let guard = if arm.guard {
                self.str_guard(arm.binding.as_deref())
            } else {
                String::new()
            };
            let head = format!("{}{guard} => ", arm.pat);
            if self.remaining() > 4 && self.rng.chance(50) {
                self.indent -= 1;
                self.line(&format!("{head}{{"));
                self.indent += 1;
                if let Some(name) = &arm.binding {
                    if self.rng.chance(60) {
                        self.line(&format!("PrintLine({name});"));
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
                let body = self.str_arm_unit(arm.binding.as_deref(), depth);
                self.close_block();
                self.line(&format!("{head}{body},"));
            }
            let end = self.snapshot_moved();
            self.no_violations -= u32::from(!first);
            for (a, e) in after.iter_mut().zip(end) {
                *a |= e;
            }
        }
        self.restore_moved(&after);
        self.indent -= 1;
        self.line("}");
        self.unlock(&s.roots);
    }

    /// `(match s { "a" => v, "b" | "c" if g => v, _ => v })` of the primitive type `ty`, on
    /// one line.
    pub(super) fn str_inline_match(&mut self, ty: ValTy, depth: u32) -> Expr {
        let s = self.str_expr(depth.saturating_sub(2));
        let arms = self.str_arms(false);
        self.lock(&s.roots);
        let before = self.snapshot_moved();
        let mut after = vec![false; before.len()];
        let mut parts = Vec::new();
        for (i, arm) in arms.into_iter().enumerate() {
            let first = i == 0;
            self.no_violations += u32::from(!first);
            self.restore_moved(&before);
            let guard = if arm.guard {
                self.str_guard(None)
            } else {
                String::new()
            };
            let v = self.val_expr(ty, depth.saturating_sub(1));
            parts.push(format!("{}{guard} => {}", arm.pat, v.text));
            let end = self.snapshot_moved();
            self.no_violations -= u32::from(!first);
            for (a, e) in after.iter_mut().zip(end) {
                *a |= e;
            }
        }
        self.restore_moved(&after);
        self.unlock(&s.roots);
        Expr::value(format!(
            "(match {} {{ {} }})",
            super::matches::scrutinee_text(s.text),
            parts.join(", ")
        ))
    }

    /// `if let "a" | "b" = s { .. } [else { .. }]`.
    pub(super) fn stmt_str_if_let(&mut self, depth: u32) -> bool {
        let s = self.str_expr(depth.saturating_sub(1));
        let k = usize::try_from(self.rng.range(1, 3)).expect("small");
        let lits = self.pattern_literals(k, &mut Vec::new(), false);
        self.lock(&s.roots);
        let before = self.snapshot_moved();
        self.line(&format!(
            "if let {} = {} {{",
            lits.join(" | "),
            super::matches::scrutinee_text(s.text.clone())
        ));
        self.block_body(2);
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
        self.unlock(&s.roots);
        true
    }

    /// `let "a" = s else { diverge };` (an or-pattern in parentheses).
    pub(super) fn stmt_str_let_else(&mut self, depth: u32) -> bool {
        let s = self.str_expr(depth.saturating_sub(1));
        let k = usize::try_from(self.rng.range(1, 2)).expect("small");
        let lits = self.pattern_literals(k, &mut Vec::new(), false);
        let pat = if lits.len() > 1 {
            format!("({})", lits.join(" | "))
        } else {
            lits[0].clone()
        };
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
        true
    }

    // ----- loops -------------------------------------------------------------------------

    /// A loop over a string: by index, or shrinking a `mut` local.
    pub(super) fn stmt_str_loop(&mut self, depth: u32) -> bool {
        if self.rng.chance(55) {
            self.str_index_loop(depth)
        } else {
            self.str_shrink_loop(depth)
        }
    }

    /// `let mut i: usize = 0usize; while i < s.len() { ..&s[i..=i].. ; i += 1usize; }`
    /// over a `&str` local `s` (a new one now and then): the body starts with a use of a
    /// one-byte slice at `i` (or of the rest from `i`) and ends with the increment, so no
    /// `continue` may skip it.
    fn str_index_loop(&mut self, depth: u32) -> bool {
        let found = if self.rng.chance(65) {
            self.ref_var(false, ValTy::Str)
        } else {
            None
        };
        let s = if let Some(e) = found {
            e
        } else {
            let e = self.str_expr(depth.saturating_sub(1));
            let name = self.fresh_name();
            self.line(&format!("let {name}: &str = {};", e.text));
            let v = self.bind_str(name.clone(), false, &e);
            Expr {
                text: name,
                roots: vec![(v, false)],
            }
        };
        let i = self.fresh_name();
        self.line(&format!("let mut {i}: usize = 0usize;"));
        self.declare(i.clone(), Ty::prim(USIZE), true, true);
        let before = self.snapshot_moved();
        self.loops.push(LoopKind::Index);
        self.line(&format!("while {i} < {}.len() {{", s.text));
        self.open_block();
        let name = &s.text;
        let piece = match self.rng.weighted(&[40, 30, 15, 15]) {
            0 => format!("&{name}[{i}..={i}]"),
            1 => format!("&{name}[{i}..{i} + 1usize]"),
            2 => format!("&{name}[{i}..]"),
            _ => format!("&{name}[..{i}]"),
        };
        let piece = Expr {
            text: piece,
            roots: s.roots.clone(),
        };
        match self.rng.weighted(&[35, 25, 20, 20]) {
            0 => {
                let f = if self.rng.chance(60) {
                    "Print"
                } else {
                    "PrintLine"
                };
                self.line(&format!("{f}({});", piece.text));
            }
            1 => {
                let v = self.fresh_name();
                self.line(&format!("let {v} = {};", piece.text));
                self.bind_str(v, false, &piece);
            }
            2 => {
                let lit = str_literal(self.rng).0;
                let op = *self.rng.pick(&["==", "!=", "<", ">="]);
                self.line(&format!(
                    "if {} {op} {lit} {{ PrintLine({i}); }}",
                    piece.text
                ));
            }
            _ => self.str_match_on(&piece, depth),
        }
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

    /// `let mut t: &str = e; while t.len() > k { ..; t = &t[1..]; }` (or `while let "a" |
    /// "ab" = t`), the reassignment last so no `continue` may skip it. The step stays
    /// within the length the condition guarantees, but now and then goes past it (a
    /// panic).
    fn str_shrink_loop(&mut self, depth: u32) -> bool {
        let e = self.str_expr(depth.saturating_sub(1));
        let e = if self.is_plain_str(&e) {
            e
        } else {
            self.str_lit()
        };
        let t = self.fresh_name();
        let annotation = if self.rng.chance(50) { ": &str" } else { "" };
        self.line(&format!("let mut {t}{annotation} = {};", e.text));
        let v = self.bind_str(t.clone(), true, &e);
        let before = self.snapshot_moved();
        self.loops.push(LoopKind::Index);
        let tail = if self.rng.chance(55) {
            let k = self.rng.range(0, 1);
            self.line(&format!("while {t}.len() > {k}usize {{"));
            let k = usize::try_from(k).expect("small");
            match self.rng.weighted(&[60, 30, 10]) {
                0 => format!("{t} = &{t}[{}..];", 1 + self.rng.index(k + 1)),
                1 => format!("{t} = &{t}[..({t}.len() - 1usize)];"),
                _ => format!("{t} = &{t}[{}..];", k + 2),
            }
        } else {
            let k = usize::try_from(self.rng.range(1, 3)).expect("small");
            let lits = self.pattern_literals(k, &mut Vec::new(), true);
            self.line(&format!("while let {} = {t} {{", lits.join(" | ")));
            format!("{t} = &{t}[1..];")
        };
        self.open_block();
        let current = Expr {
            text: t.clone(),
            roots: vec![(v, false)],
        };
        match self.rng.weighted(&[45, 25, 30]) {
            0 => self.line(&format!("PrintLine({t});")),
            1 => self.str_match_on(&current, depth),
            _ => {
                let piece = Expr {
                    text: format!("&{t}[..1]"),
                    roots: current.roots.clone(),
                };
                let name = self.fresh_name();
                self.line(&format!("let {name}: &str = {};", piece.text));
                self.bind_str(name, false, &piece);
            }
        }
        let n = self.rng.range(0, 2);
        for _ in 0..n {
            if self.remaining() <= 3 {
                break;
            }
            self.stmt();
        }
        self.line(&tail);
        self.close_block();
        self.loops.pop();
        self.join_moved(&before);
        self.line("}");
        true
    }
}
