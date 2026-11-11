//! Pattern lowering: `match`, `if let`, `while let`, `let`-`else`, and destructuring
//! `let`s and parameters.
//!
//! A `match` is lowered as a sequence of *candidates*, one per arm and per choice of
//! alternatives of its or-patterns, tried in source order: each tests its pattern against
//! the scrutinee place (a discriminant switch per variant pattern of an enum with more
//! than one variant, a comparison per literal, two per range; nothing for bindings,
//! wildcards, tuples or single-variant enums), and a failed test jumps to the next
//! candidate; the last one's failure is unreachable, since the type checker proved the
//! match exhaustive. A candidate whose tests pass binds its names and jumps to its arm's
//! body, which every candidate of the arm shares. The scrutinee is evaluated once, into a
//! temporary unless it is a place, and mentioned (`PlaceMention`, as rustc does): a match
//! reads only what its tests read (a lone `_` arm reads nothing).
//!
//! Guards (rustc's scheme): before the guard runs, the candidate's bindings are made for
//! it, then the places the match inspects are *fake*-borrowed (shallow borrows the borrow
//! checker treats as shared ones, so assigning or mutably borrowing them in the guard is
//! an error, rustc's E0510). A `ref`/`ref mut` binding gets its arm's reference there and
//! then (rustc's `RefWithinGuard`, #100), so while the arm uses it the guard may not read
//! a place it mutably borrows or write one it borrows; a by-value binding is bound by
//! shared reference, kept alive to the end of the guard (rustc's fake read), so the guard
//! may not write its place either. The fake-borrowed places are rustc's: across every
//! candidate of the match, each place a test reads (an enum's discriminant, a compared
//! value) and each prefix of it that ends just before a `Deref`, and the prefixes before a
//! `Deref` of each binding's place; a match whose patterns test nothing and bind nothing
//! behind a reference takes none (`match p { _ if g => .. }` reads nothing of `p`, so a
//! live `&mut p` is no conflict). Inside the guard a name denotes `*g` for a reference `g`
//! to its place (or `g` itself for a binding that is a reference, the arm's own local for
//! a `ref` binding, and `*g` for a `g` borrowing a `ref mut` binding's local), so a guard
//! can neither move out of the scrutinee nor write through a binding (`mut n`, `ref mut
//! n`: E0594 in rustc). A guard that fails ends its `ref`/`ref mut` bindings' storage and
//! tries the next candidate; one that holds keeps the fake borrows and the by-value
//! bindings' references alive to its end and then binds by value, so a binding moves its
//! part of the scrutinee only once its arm is taken.
//!
//! Tests are made once per place, as rustc's decision tree makes them (decision 72): a
//! discriminant read, or the value of a place more than one candidate compares, goes into
//! a temporary, and a later candidate reached only along paths that made the read reuses
//! the temporary instead of reading the place again. A guard that moved the scrutinee out
//! and failed therefore leaves the next candidates nothing of it to read where they reuse
//! a read, and one that wrote a tested place through a `&mut` taken before the match does
//! not change what those tests see.
//!
//! False edges (rustc's): after each candidate's tests, a `FalseEdge` goes on to the
//! candidate's guard and bindings, and imaginarily to the next candidate, or for the last
//! candidate of an `if let`, `while let` or `let`-`else` to the code that runs when the
//! pattern does not match. The borrow checker follows both, so an arm, an alternative or
//! an `else` that the patterns before it make unreachable is checked as in rustc (#47);
//! drop elaboration, after borrow checking, turns them into `goto`s and simplifies, which
//! removes that code.

use std::collections::{HashMap, HashSet};

use sabiko_ast::{Arm, Block, Expr, ExprKind, NodeId, Pat, PatKind};
use sabiko_resolve::{DefKind, LocalId, Res};
use sabiko_span::Span;
use sabiko_types::{rest_positions, ConstValue, Mutability, PatConst, Ty};

use super::{is_extending_pattern, is_place_expr, writes_once, Builder, LoopScope, ValueDest};
use crate::{
    BasicBlock, BinOp, BorrowKind, Constant, FakeBorrowKind, Local, LocalDecl, LocalKind, Operand,
    Place, PlaceElem, Rvalue, StatementKind, TerminatorKind,
};

/// An enum and the index of one of its variants.
type Variant = (sabiko_resolve::DefId, usize);

/// Which alternative each or-pattern (by node) takes in one candidate.
pub(super) type Choice = HashMap<NodeId, usize>;

/// What the candidates of one match have tested so far (see the module docs).
#[derive(Debug, Default)]
pub(super) struct TestCache {
    /// Places whose value more than one candidate compares: the first comparison copies
    /// the value into a temporary the later ones reuse.
    shared: HashSet<Place>,
    /// The temporary holding the discriminant (`true`) or the value (`false`) of each
    /// tested place, assigned on every path to the point being lowered.
    known: HashMap<(Place, bool), Local>,
    /// What is known on every jump to the current candidate's failure block lowered so
    /// far; `None` before the first.
    at_fail: Option<HashMap<(Place, bool), Local>>,
}

/// How a candidate's bindings are made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BindMode {
    /// A destructuring `let` or parameter: declare each binding in the enclosing block
    /// scope as it is written.
    Declare,
    /// A `match` arm, `if let`, `while let` or `let`-`else`: the bindings' locals exist
    /// already; mark them live and write them, without opening their cleanup scopes (the
    /// caller does, where the candidates join).
    Prepared,
}

/// Every choice of one alternative per or-pattern in `pat`, in source order.
pub(super) fn choices(pat: &Pat) -> Vec<Choice> {
    let product = |parts: Vec<Vec<Choice>>| {
        parts.into_iter().fold(vec![Choice::new()], |acc, part| {
            let mut out = Vec::with_capacity(acc.len() * part.len());
            for a in &acc {
                for p in &part {
                    let mut c = a.clone();
                    c.extend(p.iter().map(|(k, v)| (*k, *v)));
                    out.push(c);
                }
            }
            out
        })
    };
    match &pat.kind {
        PatKind::Ident { sub: Some(sub), .. } => choices(sub),
        PatKind::Tuple(ps) | PatKind::TupleStruct(_, ps) => {
            product(ps.iter().map(choices).collect())
        }
        PatKind::Struct { fields, .. } => product(fields.iter().map(|f| choices(&f.pat)).collect()),
        PatKind::Or(alts) => alts
            .iter()
            .enumerate()
            .flat_map(|(i, alt)| {
                choices(alt).into_iter().map(move |mut c| {
                    c.insert(pat.id, i);
                    c
                })
            })
            .collect(),
        PatKind::Ident { sub: None, .. }
        | PatKind::Assignee(_)
        | PatKind::Wild
        | PatKind::Rest
        | PatKind::Path(_)
        | PatKind::Lit(_)
        | PatKind::Range(..) => vec![Choice::new()],
    }
}

/// Whether `pat` contains an or-pattern.
pub(super) fn has_or(pat: &Pat) -> bool {
    match &pat.kind {
        PatKind::Or(_) => true,
        PatKind::Ident { sub, .. } => sub.as_deref().is_some_and(has_or),
        PatKind::Tuple(ps) | PatKind::TupleStruct(_, ps) => ps.iter().any(has_or),
        PatKind::Struct { fields, .. } => fields.iter().any(|f| has_or(&f.pat)),
        _ => false,
    }
}

impl Builder<'_> {
    /// The place a pattern node matches: `place`, dereferenced as many times as the
    /// pattern looked through references (default binding modes).
    fn pat_place(&self, pat: &Pat, place: Place) -> Place {
        let mut place = place;
        for _ in 0..self.types.pat_derefs.get(&pat.id).copied().unwrap_or(0) {
            place = place.deref();
        }
        place
    }

    /// The type of the fields of a tuple, struct or variant at `place` (variant `variant`
    /// for an enum).
    fn field_tys(&self, place: &Place, variant: Option<usize>) -> Vec<Ty> {
        match place.ty(&self.body) {
            Ty::Tuple(tys) => *tys,
            ty @ Ty::Adt(..) => self.typeck.items.field_tys(&ty, variant),
            _ => Vec::new(),
        }
    }

    /// The subpatterns of a tuple, tuple-struct or struct pattern with the places they
    /// match, and the variant the pattern names (if any).
    fn subpatterns<'p>(
        &self,
        pat: &'p Pat,
        place: &Place,
    ) -> (Option<Variant>, Vec<(&'p Pat, Place)>) {
        let variant = self.types.pat_variants.get(&pat.id).copied();
        let base = match variant {
            Some((_, index)) => place.clone().downcast(index),
            None => place.clone(),
        };
        let tys = self.field_tys(place, variant.map(|(_, i)| i));
        let mut out = Vec::new();
        match &pat.kind {
            PatKind::Tuple(ps) | PatKind::TupleStruct(_, ps) => {
                for (p, pos) in ps.iter().zip(rest_positions(ps, tys.len())) {
                    if let Some(i) = pos {
                        out.push((p, base.clone().field(i, tys[i].clone())));
                    }
                }
            }
            PatKind::Struct { fields, .. } => {
                let names: Vec<String> = match place.ty(&self.body) {
                    Ty::Adt(def, _) => {
                        let adt = self.typeck.items.adt(def);
                        let defs = match variant {
                            Some((_, v)) => &adt.variant(v).fields,
                            None => &adt.fields,
                        };
                        defs.iter().map(|f| f.name.clone()).collect()
                    }
                    _ => Vec::new(),
                };
                for f in fields {
                    let i = names
                        .iter()
                        .position(|n| *n == f.name.name)
                        .expect("struct pattern fields exist in a checked body");
                    out.push((&f.pat, base.clone().field(i, tys[i].clone())));
                }
            }
            _ => {}
        }
        (variant, out)
    }

    /// Whether an identifier pattern compares (a constant, a unit struct or an imported
    /// unit variant) instead of binding, or an assignee does (a unit struct or variant).
    fn ident_is_path(&self, pat: &Pat) -> bool {
        matches!(pat.kind, PatKind::Ident { .. } | PatKind::Assignee(_))
            && self.resolve.pat_res(pat.id).is_some()
    }

    // ----- tests -----

    /// Emits the tests of `pat` (under `choice`) against `place`: a failed test jumps to
    /// `fail`; the builder continues where every test passed.
    fn test_pat(&mut self, pat: &Pat, place: &Place, choice: &Choice, fail: BasicBlock) {
        if self.ident_is_path(pat) {
            let place = self.pat_place(pat, place.clone());
            // A lone identifier naming an imported unit variant (the prelude's `None`)
            // tests like a path pattern.
            if let Some(&(adt, index)) = self.types.pat_variants.get(&pat.id) {
                self.test_variant(&place, adt, index, fail, pat.span);
            } else {
                self.test_value(pat, &place, fail);
            }
            return;
        }
        match &pat.kind {
            PatKind::Ident { sub, .. } => {
                if let Some(sub) = sub {
                    self.test_pat(sub, place, choice, fail);
                }
            }
            PatKind::Wild | PatKind::Rest | PatKind::Assignee(_) => {}
            PatKind::Or(alts) => {
                let alt = &alts[choice[&pat.id]];
                self.test_pat(alt, place, choice, fail);
            }
            PatKind::Path(_) => {
                let place = self.pat_place(pat, place.clone());
                if let Some(&(adt, index)) = self.types.pat_variants.get(&pat.id) {
                    self.test_variant(&place, adt, index, fail, pat.span);
                } else {
                    self.test_value(pat, &place, fail);
                }
            }
            PatKind::Tuple(_) | PatKind::TupleStruct(..) | PatKind::Struct { .. } => {
                let place = self.pat_place(pat, place.clone());
                let (variant, subs) = self.subpatterns(pat, &place);
                if let Some((adt, index)) = variant {
                    self.test_variant(&place, adt, index, fail, pat.span);
                }
                for (p, sub_place) in subs {
                    self.test_pat(p, &sub_place, choice, fail);
                }
            }
            PatKind::Lit(_) | PatKind::Range(..) => {
                let place = self.pat_place(pat, place.clone());
                self.test_value(pat, &place, fail);
            }
        }
    }

    /// The test that the enum at `place` is variant `index` of `adt`: a read of its
    /// discriminant, unless the enum has a single variant (then nothing is read).
    fn test_variant(
        &mut self,
        place: &Place,
        adt: sabiko_resolve::DefId,
        index: usize,
        fail: BasicBlock,
        span: Span,
    ) {
        if !self.tests_discriminant(adt) {
            return;
        }
        let def = self.typeck.items.adt(adt);
        #[allow(clippy::cast_sign_loss)]
        let discr = def.variant(index).discr as u128;
        let key = (place.clone(), true);
        let temp = if let Some(&temp) = self.tests.known.get(&key) {
            temp
        } else {
            let temp = self.new_temp(Ty::Prim(sabiko_resolve::PrimTy::Isize), span);
            self.assign(
                Place::local(temp),
                Rvalue::Discriminant(place.clone()),
                span,
            );
            self.tests.known.insert(key, temp);
            temp
        };
        let ok = self.new_block();
        self.note_fail();
        self.switch_int(
            Operand::Copy(Place::local(temp)),
            vec![(discr, ok)],
            fail,
            span,
        );
        self.enter(ok);
    }

    /// The test of a literal, range or constant pattern: comparisons of the value at
    /// `place` with the pattern's constants.
    fn test_value(&mut self, pat: &Pat, place: &Place, fail: BasicBlock) {
        let span = pat.span;
        let ty = place.ty(&self.body);
        let pat_const = self.types.pat_consts.get(&pat.id).cloned();
        match pat_const.clone() {
            Some(PatConst::Value(v)) => self.test_const(place, &ty, v, fail, span),
            Some(PatConst::Range { .. }) if is_full_range(pat_const.as_ref(), &ty) => {
                // `i32::MIN..=i32::MAX` matches every value: nothing is read, as in rustc.
            }
            Some(PatConst::Range {
                lo,
                hi,
                inclusive: true,
            }) if lo == hi => {
                // `4..=4` is the constant 4, one comparison, as rustc lowers it.
                self.test_const(place, &ty, lo, fail, span);
            }
            Some(PatConst::Range { lo, hi, inclusive }) => {
                self.compare(BinOp::Ge, place, &ty, lo, fail, span);
                let op = if inclusive { BinOp::Le } else { BinOp::Lt };
                self.compare(op, place, &ty, hi, fail, span);
            }
            None => {
                // A unit struct: one value, nothing to test.
                if let Some(Res::Def(def)) = self.resolve.pat_res(pat.id) {
                    debug_assert_eq!(self.resolve.def(def).kind, DefKind::Struct);
                }
            }
        }
    }

    /// The test of a constant `value` in a pattern against `place`, jumping to `fail` when
    /// they differ. A tuple constant is tested element by element, left to right, as
    /// rustc expands it into a tuple pattern of constants (#50).
    fn test_const(
        &mut self,
        place: &Place,
        ty: &Ty,
        value: ConstValue,
        fail: BasicBlock,
        span: Span,
    ) {
        match value {
            ConstValue::Bool(b) => {
                let value = self.tested_value(place, span);
                let ok = self.new_block();
                self.note_fail();
                // rustc switches on the value with `0` first: for `false` the failure is
                // its `otherwise` (the order matters to the constant-propagation lints).
                let (targets, otherwise) = if b {
                    (vec![(0, fail)], ok)
                } else {
                    (vec![(0, ok)], fail)
                };
                self.switch_int(value, targets, otherwise, span);
                self.enter(ok);
            }
            ConstValue::Unit => {}
            ConstValue::Tuple(values) => {
                let Ty::Tuple(tys) = ty else {
                    unreachable!("a tuple constant has a tuple type")
                };
                for (index, (value, ty)) in values.into_iter().zip(tys.iter()).enumerate() {
                    let field = place.clone().field(index, ty.clone());
                    self.test_const(&field, ty, value, fail, span);
                }
            }
            value => self.compare(BinOp::Eq, place, ty, value, fail, span),
        }
    }

    /// `place op value`, jumping to `fail` when false.
    fn compare(
        &mut self,
        op: BinOp,
        place: &Place,
        ty: &Ty,
        value: ConstValue,
        fail: BasicBlock,
        span: Span,
    ) {
        let lhs = self.tested_value(place, span);
        let temp = self.new_temp(Ty::BOOL, span);
        let constant = Operand::Constant(Constant {
            ty: ty.clone(),
            value,
            span,
        });
        self.assign(
            Place::local(temp),
            Rvalue::BinaryOp(op, lhs, constant),
            span,
        );
        let ok = self.new_block();
        self.note_fail();
        // rustc switches on an integer or `char` itself, the failure its `otherwise`, and
        // branches on the comparison otherwise (a range, a float, a string), the failure
        // first; the order matters to the constant-propagation lints (decision 73).
        let on_value = op == BinOp::Eq && (ty.is_integer() || *ty == Ty::CHAR);
        let (targets, otherwise) = if on_value {
            (vec![(1, ok)], fail)
        } else {
            (vec![(0, fail)], ok)
        };
        self.switch_int(Operand::Copy(Place::local(temp)), targets, otherwise, span);
        self.enter(ok);
    }

    /// The operand a test reads the value at `place` through: the temporary an earlier
    /// test copied it into, a new such copy when another candidate compares it too
    /// (`TestCache::shared`), else the place itself.
    fn tested_value(&mut self, place: &Place, span: Span) -> Operand {
        let key = (place.clone(), false);
        if let Some(&temp) = self.tests.known.get(&key) {
            return Operand::Copy(Place::local(temp));
        }
        if !self.tests.shared.contains(place) {
            return Operand::Copy(place.clone());
        }
        let temp = self.new_temp(place.ty(&self.body), span);
        self.assign(
            Place::local(temp),
            Rvalue::Use(Operand::Copy(place.clone())),
            span,
        );
        self.tests.known.insert(key, temp);
        Operand::Copy(Place::local(temp))
    }

    /// Records a jump from the point being lowered to the current candidate's failure
    /// block: the next candidate knows only what every such jump knows.
    fn note_fail(&mut self) {
        let known = &self.tests.known;
        let at_fail = match self.tests.at_fail.take() {
            None => known.clone(),
            Some(mut at) => {
                at.retain(|key, temp| known.get(key) == Some(temp));
                at
            }
        };
        self.tests.at_fail = Some(at_fail);
    }

    /// A fresh [`TestCache`] for a match with these patterns against `place`, replacing
    /// the current one (a match in a guard is lowered between two candidates of its own);
    /// returns the one to restore.
    fn start_tests(&mut self, pats: &[&Pat], place: &Place) -> TestCache {
        let mut counts: HashMap<Place, usize> = HashMap::new();
        for pat in pats {
            for choice in choices(pat) {
                let mut places = Vec::new();
                self.value_test_places(pat, place, &choice, &mut places);
                let places: HashSet<Place> = places.into_iter().collect();
                for p in places {
                    *counts.entry(p).or_default() += 1;
                }
            }
        }
        let shared = counts
            .into_iter()
            .filter(|&(_, n)| n > 1)
            .map(|(p, _)| p)
            .collect();
        std::mem::replace(
            &mut self.tests,
            TestCache {
                shared,
                ..TestCache::default()
            },
        )
    }

    /// Adds to `out` each place whose value [`Self::test_pat`] compares for `pat` (under
    /// `choice`) against `place`.
    fn value_test_places(&self, pat: &Pat, place: &Place, choice: &Choice, out: &mut Vec<Place>) {
        let value = |pat: &Pat, place: Place, out: &mut Vec<Place>| {
            let ty = place.ty(&self.body);
            match self.types.pat_consts.get(&pat.id) {
                Some(PatConst::Value(value)) => push_const_places(out, &place, &ty, value),
                Some(c @ PatConst::Range { .. }) if !is_full_range(Some(c), &ty) => out.push(place),
                _ => {}
            }
        };
        if self.ident_is_path(pat) {
            value(pat, self.pat_place(pat, place.clone()), out);
            return;
        }
        match &pat.kind {
            PatKind::Ident { sub, .. } => {
                if let Some(sub) = sub {
                    self.value_test_places(sub, place, choice, out);
                }
            }
            PatKind::Wild | PatKind::Rest | PatKind::Assignee(_) => {}
            PatKind::Or(alts) => {
                self.value_test_places(&alts[choice[&pat.id]], place, choice, out);
            }
            PatKind::Path(_) => {
                if !self.types.pat_variants.contains_key(&pat.id) {
                    value(pat, self.pat_place(pat, place.clone()), out);
                }
            }
            PatKind::Tuple(_) | PatKind::TupleStruct(..) | PatKind::Struct { .. } => {
                let place = self.pat_place(pat, place.clone());
                let (_, subs) = self.subpatterns(pat, &place);
                for (p, sub_place) in subs {
                    self.value_test_places(p, &sub_place, choice, out);
                }
            }
            PatKind::Lit(_) | PatKind::Range(..) => {
                value(pat, self.pat_place(pat, place.clone()), out);
            }
        }
    }

    // ----- fake borrows -----

    /// The places a guard of the match with these arms fake-borrows (see the module
    /// docs), each once, in the order the candidates reach them; none when no arm has a
    /// guard.
    fn guard_fake_places(&self, arms: &[Arm], place: &Place) -> Vec<Place> {
        let mut out = Vec::new();
        if arms.iter().any(|arm| arm.guard.is_some()) {
            for arm in arms {
                for choice in choices(&arm.pat) {
                    self.collect_fake_places(&arm.pat, place, &choice, &mut out);
                }
            }
        }
        out
    }

    /// Adds to `out` what the candidate of `pat` (under `choice`) against `place` makes a
    /// guard fake-borrow: the places [`Self::test_pat`] reads, and the places before a
    /// `Deref` in those and in the places [`Self::bind_pat`] binds.
    fn collect_fake_places(&self, pat: &Pat, place: &Place, choice: &Choice, out: &mut Vec<Place>) {
        let reads_value = |pat: &Pat, ty: &Ty| match self.types.pat_consts.get(&pat.id) {
            Some(c) => !is_full_range(Some(c), ty),
            None => false,
        };
        if let Some(PatConst::Value(value @ (ConstValue::Tuple(_) | ConstValue::Unit))) =
            self.types.pat_consts.get(&pat.id)
        {
            // A tuple constant reads what its expansion's element constants read.
            let place = self.pat_place(pat, place.clone());
            push_const_fake_places(out, &place, &place.ty(&self.body), value);
            return;
        }
        if self.ident_is_path(pat) {
            let place = self.pat_place(pat, place.clone());
            let reads = reads_value(pat, &place.ty(&self.body));
            if reads {
                push_fake_place(out, &place, true);
            }
            return;
        }
        match &pat.kind {
            PatKind::Ident { sub, .. } => {
                push_fake_place(out, place, false);
                if let Some(sub) = sub {
                    self.collect_fake_places(sub, place, choice, out);
                }
            }
            // Only in a destructuring assignment, which has no guard.
            PatKind::Wild | PatKind::Rest | PatKind::Assignee(_) => {}
            PatKind::Or(alts) => {
                let alt = &alts[choice[&pat.id]];
                self.collect_fake_places(alt, place, choice, out);
            }
            PatKind::Path(_) => {
                let place = self.pat_place(pat, place.clone());
                let reads = match self.types.pat_variants.get(&pat.id) {
                    Some(&(adt, _)) => self.tests_discriminant(adt),
                    None => reads_value(pat, &place.ty(&self.body)),
                };
                if reads {
                    push_fake_place(out, &place, true);
                }
            }
            PatKind::Tuple(_) | PatKind::TupleStruct(..) | PatKind::Struct { .. } => {
                let place = self.pat_place(pat, place.clone());
                let (variant, subs) = self.subpatterns(pat, &place);
                let reads = variant.is_some_and(|(adt, _)| self.tests_discriminant(adt));
                if reads {
                    push_fake_place(out, &place, true);
                }
                for (p, sub_place) in subs {
                    self.collect_fake_places(p, &sub_place, choice, out);
                }
            }
            PatKind::Lit(_) | PatKind::Range(..) => {
                let place = self.pat_place(pat, place.clone());
                let reads = reads_value(pat, &place.ty(&self.body));
                if reads {
                    push_fake_place(out, &place, true);
                }
            }
        }
    }

    /// Whether a variant pattern of `adt` reads the discriminant ([`Self::test_variant`]).
    fn tests_discriminant(&self, adt: sabiko_resolve::DefId) -> bool {
        self.typeck.items.adt(adt).variants().len() > 1
    }

    // ----- bindings -----

    /// Creates the locals of the bindings of `pat` (its first alternative's, which every
    /// alternative shares), without declaring them in a scope.
    fn prepare_locals(&mut self, pat: &Pat) -> Vec<Local> {
        let mut ids: Vec<LocalId> = Vec::new();
        pat.walk_idents(&mut |p| {
            if let Some(id) = self.resolve.local_of_pat(p.id) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        });
        ids.into_iter()
            .map(|id| {
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
                local
            })
            .collect()
    }

    /// Binds the names of `pat` (under `choice`) to the parts of `place`.
    pub(super) fn bind_pat(&mut self, pat: &Pat, place: Place, choice: &Choice, mode: BindMode) {
        if self.ident_is_path(pat) {
            return;
        }
        match &pat.kind {
            PatKind::Ident { sub, .. } => {
                self.bind_local(pat, &place, mode);
                if let Some(sub) = sub {
                    self.bind_pat(sub, place, choice, mode);
                }
            }
            PatKind::Assignee(_) => self.bind_local(pat, &place, mode),
            PatKind::Wild
            | PatKind::Rest
            | PatKind::Path(_)
            | PatKind::Lit(_)
            | PatKind::Range(..) => {}
            PatKind::Or(alts) => {
                let alt = &alts[choice.get(&pat.id).copied().unwrap_or(0)];
                self.bind_pat(alt, place, choice, mode);
            }
            PatKind::Tuple(_) | PatKind::TupleStruct(..) | PatKind::Struct { .. } => {
                let place = self.pat_place(pat, place);
                let (_, subs) = self.subpatterns(pat, &place);
                for (p, sub_place) in subs {
                    self.bind_pat(p, sub_place, choice, mode);
                }
            }
        }
    }

    /// Binds the local of the identifier pattern or assignee `pat` to `place`.
    fn bind_local(&mut self, pat: &Pat, place: &Place, mode: BindMode) {
        let id = self
            .resolve
            .local_of_pat(pat.id)
            .expect("identifier patterns introduce locals");
        let by_value = !self.types.ref_bindings.contains_key(&pat.id);
        let rvalue = match self.types.ref_bindings.get(&pat.id) {
            Some(&m) => Rvalue::Ref(BorrowKind::new(m), place.clone()),
            None => Rvalue::Use(self.place_operand(place.clone())),
        };
        match mode {
            BindMode::Declare => {
                let local = self.declare_user_local(id, pat.span);
                self.assign(Place::local(local), rvalue, pat.span);
            }
            BindMode::Prepared => {
                let local = self.mir_local(id);
                if self.bound_for_guard.contains(&local) {
                    return;
                }
                if by_value {
                    self.body.locals[local.index()].match_span = self.binding_scrutinee;
                }
                self.push_stmt(StatementKind::StorageLive(local), pat.span);
                self.push_stmt(StatementKind::Assign(Place::local(local), rvalue), pat.span);
            }
        }
    }

    /// For a guard (rustc's scheme, see the module docs): a `ref`/`ref mut` binding gets
    /// its arm's reference now, which the guard sees (a `ref mut` one through a shared
    /// reference to it, so the guard cannot write through it), and which stays the arm's;
    /// a by-value binding denotes its place through a shared reference, recorded in
    /// `values` to be kept alive to the end of the guard. Records what each name denotes in
    /// `guard_places`.
    fn bind_for_guard(
        &mut self,
        pat: &Pat,
        place: Place,
        choice: &Choice,
        values: &mut Vec<Local>,
    ) {
        if self.ident_is_path(pat) {
            return;
        }
        match &pat.kind {
            PatKind::Ident { sub, .. } => {
                let id = self
                    .resolve
                    .local_of_pat(pat.id)
                    .expect("identifier patterns introduce locals");
                if let Some(&m) = self.types.ref_bindings.get(&pat.id) {
                    let local = self.mir_local(id);
                    self.push_stmt(StatementKind::StorageLive(local), pat.span);
                    self.push_stmt(
                        StatementKind::Assign(
                            Place::local(local),
                            Rvalue::Ref(BorrowKind::new(m), place.clone()),
                        ),
                        pat.span,
                    );
                    self.bound_for_guard.push(local);
                    let guard_place = if m == Mutability::Mut {
                        let ty = self.body.local_decl(local).ty.clone();
                        let temp = self.new_temp(Ty::shared_ref(ty), pat.span);
                        self.assign(
                            Place::local(temp),
                            Rvalue::Ref(BorrowKind::Shared, Place::local(local)),
                            pat.span,
                        );
                        Place::local(temp).deref()
                    } else {
                        Place::local(local)
                    };
                    self.guard_places.insert(id, guard_place);
                } else {
                    let ty = place.ty(&self.body);
                    // A binding of a reference denotes a shared reborrow of it rather than
                    // a reference to the reference.
                    let (borrowed, inner_ref) = match &ty {
                        Ty::Ref(_, inner) => (place.clone().deref(), Some((**inner).clone())),
                        _ => (place.clone(), None),
                    };
                    let temp_ty = Ty::shared_ref(inner_ref.clone().unwrap_or(ty));
                    let temp = self.new_temp(temp_ty, pat.span);
                    self.assign(
                        Place::local(temp),
                        Rvalue::Ref(BorrowKind::Shared, borrowed),
                        pat.span,
                    );
                    values.push(temp);
                    let guard_place = if inner_ref.is_some() {
                        Place::local(temp)
                    } else {
                        Place::local(temp).deref()
                    };
                    self.guard_places.insert(id, guard_place);
                }
                if let Some(sub) = sub {
                    self.bind_for_guard(sub, place, choice, values);
                }
            }
            PatKind::Wild
            | PatKind::Rest
            | PatKind::Assignee(_)
            | PatKind::Path(_)
            | PatKind::Lit(_)
            | PatKind::Range(..) => {}
            PatKind::Or(alts) => {
                let alt = &alts[choice[&pat.id]];
                self.bind_for_guard(alt, place, choice, values);
            }
            PatKind::Tuple(_) | PatKind::TupleStruct(..) | PatKind::Struct { .. } => {
                let place = self.pat_place(pat, place);
                let (_, subs) = self.subpatterns(pat, &place);
                for (p, sub_place) in subs {
                    self.bind_for_guard(p, sub_place, choice, values);
                }
            }
        }
    }

    /// Declares the bindings of `pat` without initialising them (`let (a, b);`).
    pub(super) fn declare_pattern_locals(&mut self, pat: &Pat) {
        let mut ids: Vec<(LocalId, Span)> = Vec::new();
        pat.walk_idents(&mut |p| {
            if let Some(id) = self.resolve.local_of_pat(p.id) {
                ids.push((id, p.span));
            }
        });
        for (id, span) in ids {
            let local = self.declare_user_local(id, span);
            self.open_pending(local);
        }
    }

    // ----- candidates -----

    /// The place a scrutinee denotes: itself if it is a place, else a temporary holding
    /// its value. Mentioned, as rustc does for a `match`. A place that `pats` bind by
    /// `ref mut` is used mutably (see `place_mutability`).
    fn scrutinee_place(&mut self, scrutinee: &Expr, pats: &[&Pat]) -> Place {
        let place = if is_place_expr(self.resolve, scrutinee) {
            let m = Mutability::from_bool(pats.iter().any(|p| binds_ref_mut(p)));
            self.as_place_mut(scrutinee, m)
        } else {
            Place::local(self.as_temp(scrutinee))
        };
        self.push_stmt(StatementKind::PlaceMention(place.clone()), scrutinee.span);
        place
    }

    /// Lowers the candidates of `pat` (with an optional guard, which fake-borrows `fakes`):
    /// each that matches binds and jumps to `success`; the builder continues where the
    /// last one failed. Each candidate's tests end in a false edge to the next candidate,
    /// and the last one's to where the builder continues when `edge_after_last`.
    fn lower_candidates(
        &mut self,
        pat: &Pat,
        guard: Option<(&Expr, &[Place])>,
        place: &Place,
        scrutinee: Span,
        success: BasicBlock,
        edge_after_last: bool,
    ) {
        let choices = choices(pat);
        let last = choices.len() - 1;
        for (index, choice) in choices.into_iter().enumerate() {
            let fail = self.new_block();
            self.tests.at_fail = None;
            self.test_pat(pat, place, &choice, fail);
            if index < last || edge_after_last {
                let next = self.new_block();
                self.note_fail();
                self.terminate(
                    TerminatorKind::FalseEdge {
                        real_target: next,
                        imaginary_target: fail,
                    },
                    pat.span,
                );
                self.enter(next);
            }
            if let Some((guard, fakes)) = guard {
                self.lower_guard(guard, fakes, pat, place, &choice, fail);
            }
            let saved = self.binding_scrutinee.replace(scrutinee);
            self.bind_pat(pat, place.clone(), &choice, BindMode::Prepared);
            self.binding_scrutinee = saved;
            self.bound_for_guard.clear();
            let span = pat.span;
            self.terminate(TerminatorKind::Goto { target: success }, span);
            self.enter(fail);
            self.tests.known = self.tests.at_fail.take().unwrap_or_default();
        }
    }

    /// The guard of a candidate whose tests passed, fake-borrowing `fakes` while it runs:
    /// jumps to `fail` when it is false and continues when it holds (see the module docs).
    fn lower_guard(
        &mut self,
        guard: &Expr,
        fakes: &[Place],
        pat: &Pat,
        place: &Place,
        choice: &Choice,
        fail: BasicBlock,
    ) {
        let span = guard.span;
        // The bindings first, then the fake borrows, as rustc: a `ref mut` binding's borrow
        // of a place the match tested is not taken while that place is fake-borrowed.
        let saved = std::mem::take(&mut self.guard_places);
        let mut values = Vec::new();
        self.bind_for_guard(pat, place.clone(), choice, &mut values);
        let temps: Vec<Local> = fakes
            .iter()
            .map(|fake| {
                let temp = self.new_temp(Ty::shared_ref(fake.ty(&self.body)), span);
                self.assign(
                    Place::local(temp),
                    Rvalue::Ref(BorrowKind::Fake(FakeBorrowKind::Guard), fake.clone()),
                    span,
                );
                temp
            })
            .collect();
        // A `match` in the guard binds its own candidates.
        let bound = std::mem::take(&mut self.bound_for_guard);
        let (holds, fails) = if guard_has_let(guard) {
            self.lower_let_guard(guard, span)
        } else {
            self.branch_on(guard, span)
        };
        self.bound_for_guard = bound;
        self.guard_places = saved;
        self.enter(fails);
        // The next candidate binds its own (an or-pattern's alternatives share the locals).
        let bound = self.bound_for_guard.clone();
        self.storage_dead(&bound, span);
        self.note_fail();
        self.terminate(TerminatorKind::Goto { target: fail }, span);
        self.enter(holds);
        // The fake borrows, and the references a by-value binding is seen through (rustc's
        // `FakeRead(ForGuardBinding)`), live to the end of the guard: the guard may not
        // write what the match tested or what a binding will copy or move.
        for temp in temps.into_iter().chain(values) {
            self.push_stmt(StatementKind::PlaceMention(Place::local(temp)), span);
        }
    }

    /// A guard with `let`s (`p if let q = e && c =>`, #98): lowered as a let chain in a
    /// scope of its own, whose failures drop what it holds so far and go to the returned
    /// `fails` block. Where it holds, what the scope holds (the `let`s' scrutinee
    /// temporaries and bindings) is not dropped but handed to the arm (`guard_held`), whose
    /// body scope takes it over, so it lives to the end of the arm as in rustc; its
    /// cleanup scopes open again at the body. Returns the blocks where it holds and where
    /// it fails.
    fn lower_let_guard(&mut self, guard: &Expr, span: Span) -> (BasicBlock, BasicBlock) {
        let operands = guard_operands(guard);
        let saved_scope = self.push_temp_scope();
        let depth = self.scopes.len() - 1;
        let fails = self.new_block();
        self.lower_chain(&operands, span, &mut |this: &mut Self| {
            this.leave_to(depth, fails, span);
        });
        let held = self.scopes.pop().expect("pushed above");
        self.temp_scope = saved_scope;
        self.pending.retain(|l| !held.contains(l));
        if let Some(index) = self.chain.iter().position(|(l, _)| held.contains(l)) {
            let keep: Vec<Local> = self
                .chain
                .drain(index..)
                .map(|(l, _)| l)
                .filter(|l| !held.contains(l))
                .collect();
            for local in keep {
                self.push_cleanup(local);
            }
            self.chain_changed();
        }
        for local in held {
            if !self.guard_held.contains(&local) {
                self.guard_held.push(local);
            }
        }
        (self.current, fails)
    }

    /// The body of an arm (or the `then` of an `if let`, the body of a `while let`), in a
    /// scope holding the bindings, whose cleanup scopes open here, where the candidates
    /// join.
    fn with_bindings(&mut self, locals: &[Local], span: Span, f: impl FnOnce(&mut Self)) {
        self.push_scope();
        for &local in locals {
            self.scopes.last_mut().expect("pushed above").push(local);
            self.track_drop(local);
        }
        for &local in locals {
            self.open_pending(local);
        }
        f(self);
        self.pop_scope(span);
    }

    /// `match scrutinee { arms }`, writing the arms' values into `dest`.
    pub(super) fn lower_match(
        &mut self,
        dest: Option<ValueDest>,
        scrutinee: &Expr,
        arms: &[Arm],
        span: Span,
    ) {
        if self.ty(scrutinee).is_never() {
            // `match return {}`: the scrutinee diverges; nothing after it runs.
            self.lower_expr_discard(scrutinee);
            return;
        }
        let pats: Vec<&Pat> = arms.iter().map(|arm| &arm.pat).collect();
        let place = self.scrutinee_place(scrutinee, &pats);
        let fakes = self.guard_fake_places(arms, &place);
        let saved = self.start_tests(&pats, &place);
        let outer_held = std::mem::take(&mut self.guard_held);
        let mut entries = Vec::new();
        for (index, arm) in arms.iter().enumerate() {
            let mut locals = self.prepare_locals(&arm.pat);
            if let Some(guard) = &arm.guard {
                for operand in guard_operands(guard) {
                    if let ExprKind::Let(pat, _) = &operand.kind {
                        let prepared = self.prepare_locals(pat);
                        self.prepared_lets.insert(operand.id, prepared);
                    }
                }
            }
            let body = self.new_block();
            let guard = arm.guard.as_ref().map(|g| (g, &fakes[..]));
            let is_last = index + 1 == arms.len();
            self.lower_candidates(&arm.pat, guard, &place, scrutinee.span, body, !is_last);
            // What the guard's `let`s hold lives through the body, after the pattern's
            // bindings, so it is dropped first (rustc's order).
            locals.append(&mut self.guard_held);
            entries.push((body, locals));
        }
        self.guard_held = outer_held;
        self.tests = saved;
        // Exhaustiveness was checked: no value gets here.
        self.terminate(TerminatorKind::Unreachable, span);
        let join = self.new_block();
        for (arm, (body, locals)) in arms.iter().zip(entries) {
            self.enter(body);
            let arm_span = arm.body.span;
            self.with_bindings(&locals, arm_span, |this| {
                this.in_temp_scope(arm_span, |this| match dest {
                    Some(dest) => this.value_into(dest.place, &arm.body, dest.extending),
                    None => this.lower_expr_discard(&arm.body),
                });
            });
            self.terminate(TerminatorKind::Goto { target: join }, arm_span);
        }
        self.enter(join);
    }

    /// `if let pat = scrutinee { then } else { els }`: a `match` with one tested arm. The
    /// scrutinee is a temporary scope of its own (edition 2024): its temporaries live
    /// through `then` and are dropped after it, and on the other path before `else` runs.
    pub(super) fn lower_if_let(
        &mut self,
        dest: Option<ValueDest>,
        pat: &Pat,
        scrutinee: &Expr,
        then: &Block,
        els: Option<&Expr>,
        span: Span,
    ) {
        if self.ty(scrutinee).is_never() {
            self.lower_expr_discard(scrutinee);
            return;
        }
        let saved_scope = self.push_temp_scope();
        let place = self.scrutinee_place(scrutinee, &[pat]);
        let locals = self.prepare_locals(pat);
        let then_block = self.new_block();
        let saved = self.start_tests(&[pat], &place);
        self.lower_candidates(pat, None, &place, scrutinee.span, then_block, true);
        self.tests = saved;
        let join = self.new_block();
        // The other path ends the scrutinee's temporaries first. They stay in their scope
        // for `then`, so they are taken out while `else` is lowered (an early exit from it
        // must not drop them again), as for `let`-`else`.
        let saved_chain = (self.chain.clone(), self.pending.clone());
        let temps = std::mem::take(&mut self.scopes[self.temp_scope]);
        for &local in temps.iter().rev() {
            self.end_local(local, span);
        }
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
        self.terminate(TerminatorKind::Goto { target: join }, span);
        self.scopes[self.temp_scope] = temps;
        (self.chain, self.pending) = saved_chain;
        self.enter(then_block);
        self.with_bindings(&locals, then.span, |this| {
            this.in_temp_scope(then.span, |this| match dest {
                Some(dest) => this.lower_block_into(dest.place, then, dest.extending),
                None => this.lower_block_discard(then),
            });
        });
        self.close_temp_scope(saved_scope, span);
        self.terminate(TerminatorKind::Goto { target: join }, span);
        self.enter(join);
    }

    /// `while let pat = scrutinee { body }`: a loop around a `match` whose other arm
    /// breaks. The scrutinee's temporaries are dropped every iteration.
    pub(super) fn lower_while_let(
        &mut self,
        pat: &Pat,
        scrutinee: &Expr,
        body: &Block,
        span: Span,
    ) {
        let header = self.new_block();
        self.terminate(TerminatorKind::Goto { target: header }, span);
        self.enter(header);
        let depth = self.scopes.len();
        self.loops.push(LoopScope {
            header,
            exit: None,
            dest: None,
            scope_depth: depth,
        });
        let saved = self.push_temp_scope();
        if self.ty(scrutinee).is_never() {
            // `while let E::A(x) = return 1 {}` (#99): the scrutinee leaves, so neither the
            // pattern nor the body is reached.
            self.lower_expr_discard(scrutinee);
            self.close_temp_scope(saved, span);
            self.terminate(TerminatorKind::Goto { target: header }, span);
            let scope = self.loops.pop().expect("loop scope pushed above");
            let exit = scope.exit.unwrap_or_else(|| self.new_block());
            self.enter(exit);
            return;
        }
        let place = self.scrutinee_place(scrutinee, &[pat]);
        let locals = self.prepare_locals(pat);
        let body_block = self.new_block();
        let saved_tests = self.start_tests(&[pat], &place);
        self.lower_candidates(pat, None, &place, scrutinee.span, body_block, true);
        self.tests = saved_tests;
        // No match: leave the loop, dropping the scrutinee's temporaries.
        self.lower_break(None, span);
        self.terminate(TerminatorKind::Unreachable, span);
        self.enter(body_block);
        self.with_bindings(&locals, body.span, |this| {
            this.in_temp_scope(body.span, |this| this.lower_block_discard(body));
        });
        self.close_temp_scope(saved, span);
        self.terminate(TerminatorKind::Goto { target: header }, span);
        let scope = self.loops.pop().expect("loop scope pushed above");
        let exit = scope.exit.unwrap_or_else(|| self.new_block());
        self.enter(exit);
    }

    /// `if` with a let chain (edition 2024): `if let A = a && c && let B = b { then } else
    /// { els }`. As in rustc, the condition is one temporary scope holding each `let`'s
    /// scrutinee temporaries and bindings, in order, which live through `then`; any other
    /// operand is a temporary scope of its own, dropped before the next one runs. Where
    /// an operand fails, what the condition holds so far is dropped (last first) before
    /// `else`.
    pub(super) fn lower_if_chain(
        &mut self,
        dest: Option<ValueDest>,
        operands: &[&Expr],
        then: &Block,
        els: Option<&Expr>,
        span: Span,
    ) {
        let saved_scope = self.push_temp_scope();
        let depth = self.scopes.len() - 1;
        let else_block = self.new_block();
        let reachable = self.lower_chain(operands, span, &mut |this: &mut Self| {
            this.leave_to(depth, else_block, span);
        });
        if reachable {
            self.in_temp_scope(then.span, |this| match dest {
                Some(dest) => this.lower_block_into(dest.place, then, dest.extending),
                None => this.lower_block_discard(then),
            });
        }
        self.close_temp_scope(saved_scope, span);
        let join = self.new_block();
        self.terminate(TerminatorKind::Goto { target: join }, span);
        // As after `if`'s `then`, `else` starts under the cleanup scopes open after `then`.
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
        self.terminate(TerminatorKind::Goto { target: join }, span);
        self.enter(join);
    }

    /// `while` with a let chain: a loop around [`Self::lower_if_chain`]'s condition whose
    /// failure breaks. The condition's scope is dropped every iteration.
    pub(super) fn lower_while_chain(&mut self, operands: &[&Expr], body: &Block, span: Span) {
        let header = self.new_block();
        self.terminate(TerminatorKind::Goto { target: header }, span);
        self.enter(header);
        self.loops.push(LoopScope {
            header,
            exit: None,
            dest: None,
            scope_depth: self.scopes.len(),
        });
        let saved = self.push_temp_scope();
        let reachable = self.lower_chain(operands, span, &mut |this: &mut Self| {
            this.lower_break(None, span);
            this.terminate(TerminatorKind::Unreachable, span);
        });
        if reachable {
            self.in_temp_scope(body.span, |this| this.lower_block_discard(body));
        }
        self.close_temp_scope(saved, span);
        self.terminate(TerminatorKind::Goto { target: header }, span);
        let scope = self.loops.pop().expect("loop scope pushed above");
        let exit = scope.exit.unwrap_or_else(|| self.new_block());
        self.enter(exit);
    }

    /// The operands of a let chain, each on the success path of the ones before it, in
    /// the condition's scope (the temporary scope): a `let`'s scrutinee temporaries go
    /// there, then its bindings, whose cleanup scopes open where it matched. Where an
    /// operand fails, `fail` ends the block the builder is in. Returns whether the chain
    /// can succeed: a `let` whose scrutinee diverges ends it (the builder is then in an
    /// unreachable block).
    fn lower_chain(
        &mut self,
        operands: &[&Expr],
        span: Span,
        fail: &mut dyn FnMut(&mut Self),
    ) -> bool {
        let scope = self.temp_scope;
        for operand in operands {
            let ExprKind::Let(pat, scrutinee) = &operand.kind else {
                let (holds, fails) = self.branch_on(operand, span);
                self.enter(fails);
                fail(self);
                self.enter(holds);
                continue;
            };
            if self.ty(scrutinee).is_never() {
                self.lower_expr_discard(scrutinee);
                return false;
            }
            let place = self.scrutinee_place(scrutinee, &[pat]);
            let locals = match self.prepared_lets.get(&operand.id) {
                Some(locals) => locals.clone(),
                None => self.prepare_locals(pat),
            };
            let matched = self.new_block();
            let saved = self.start_tests(&[pat], &place);
            self.lower_candidates(pat, None, &place, scrutinee.span, matched, true);
            self.tests = saved;
            fail(self);
            self.enter(matched);
            for &local in &locals {
                self.scopes[scope].push(local);
                self.track_drop(local);
            }
            for &local in &locals {
                self.open_pending(local);
            }
        }
        true
    }

    /// `let pat = init else { els };`, and a destructuring `let` whose pattern has an
    /// or-pattern (`els` is `None`: no value fails it). The bindings belong to the
    /// enclosing block; the `else` block diverges. As in rustc, the initialiser's
    /// temporaries are dropped before the `else` block runs, and when the pattern has a
    /// `ref`/`ref mut` binding the initialiser's own temporary lives to the end of the
    /// block on success (an extending pattern, as for a plain `let`).
    pub(super) fn lower_let_else(&mut self, pat: &Pat, init: &Expr, els: Option<&Block>) {
        let span = init.span;
        if self.ty(init).is_never() {
            self.lower_expr_discard(init);
            self.declare_pattern_locals(pat);
            return;
        }
        let (place, extended) = if is_extending_pattern(pat) && !is_place_expr(self.resolve, init) {
            let ty = self.ty(init).clone();
            let temp = self.new_temp_in_block_scope(ty, init.span);
            if !writes_once(init) {
                self.open_pending(temp);
            }
            self.expr_into_dest(&Place::local(temp), init);
            self.temp_complete(temp);
            let place = Place::local(temp);
            self.push_stmt(StatementKind::PlaceMention(place.clone()), init.span);
            (place, Some(temp))
        } else {
            (self.scrutinee_place(init, &[pat]), None)
        };
        let locals = self.prepare_locals(pat);
        let block_scope = self.scopes.len() - 2;
        for &local in &locals {
            self.scopes[block_scope].push(local);
            self.track_drop(local);
        }
        let ok = self.new_block();
        let saved = self.start_tests(&[pat], &place);
        self.lower_candidates(pat, None, &place, span, ok, els.is_some());
        self.tests = saved;
        if let Some(els) = els {
            // The failure path ends the initialiser's temporaries (its own value first, then
            // those made while computing it) before `else`; they stay in their scopes for
            // the success path, so they are taken out while `else` is lowered (an early exit
            // from it must not drop them again).
            let saved = (self.chain.clone(), self.pending.clone());
            let temps = std::mem::take(&mut self.scopes[self.temp_scope]);
            let extended = extended.map(|temp| {
                let (scope, pos) = self.rank(temp);
                (scope, pos, self.scopes[scope].remove(pos))
            });
            if let Some((_, _, temp)) = extended {
                self.end_local(temp, span);
            }
            for &local in temps.iter().rev() {
                self.end_local(local, span);
            }
            if let Some((_, _, temp)) = extended {
                // The statement's temporaries rank above the extended one, so its cleanup
                // scope was not the innermost when it was dropped and stayed open: close
                // it now (the chain is restored for the success path below). Left open,
                // a cleanup scope opened in `else` would rank against a local that is in
                // no scope (seed 8000252: `else { return (false, D { .. }); }`).
                if let Some(index) = self.chain.iter().position(|&(l, _)| l == temp) {
                    let above: Vec<Local> =
                        self.chain.drain(index..).skip(1).map(|(l, _)| l).collect();
                    for local in above {
                        self.push_cleanup(local);
                    }
                    self.chain_changed();
                }
            }
            self.in_temp_scope(els.span, |this| this.lower_block_discard(els));
            self.terminate(TerminatorKind::Unreachable, span);
            self.scopes[self.temp_scope] = temps;
            if let Some((scope, pos, temp)) = extended {
                self.scopes[scope].insert(pos, temp);
            }
            (self.chain, self.pending) = saved;
        } else {
            self.terminate(TerminatorKind::Unreachable, span);
        }
        self.enter(ok);
        for local in locals {
            self.open_pending(local);
        }
    }

    /// The place a binding denotes: its local, or inside a guard its reference.
    pub(super) fn binding_place(&self, id: LocalId) -> Place {
        match self.guard_places.get(&id) {
            Some(place) => place.clone(),
            None => Place::local(self.mir_local(id)),
        }
    }
}

/// Whether `pat` has a `ref mut` binding (the first alternative of an or-pattern stands
/// for all of them: every alternative binds the same names the same way).
pub(super) fn binds_ref_mut(pat: &Pat) -> bool {
    let mut found = false;
    pat.walk_idents(&mut |p| {
        if let PatKind::Ident {
            by_ref: true,
            mutable: true,
            ..
        } = p.kind
        {
            found = true;
        }
    });
    found
}

/// The places a constant pattern `value` at `place` (of type `ty`) compares: the place, or
/// each element of a tuple constant, recursively, but the `()`s.
fn push_const_places(out: &mut Vec<Place>, place: &Place, ty: &Ty, value: &ConstValue) {
    match (value, ty) {
        (ConstValue::Unit, _) => {}
        (ConstValue::Tuple(values), Ty::Tuple(tys)) => {
            for (index, (value, ty)) in values.iter().zip(tys.iter()).enumerate() {
                let field = place.clone().field(index, ty.clone());
                push_const_places(out, &field, ty, value);
            }
        }
        _ => out.push(place.clone()),
    }
}

/// The places a guard fake-borrows for a constant pattern `value` at `place` (of type
/// `ty`): each element of a tuple constant, recursively, but the `()`s, which read nothing.
fn push_const_fake_places(out: &mut Vec<Place>, place: &Place, ty: &Ty, value: &ConstValue) {
    match (value, ty) {
        (ConstValue::Unit, _) => {}
        (ConstValue::Tuple(values), Ty::Tuple(tys)) => {
            for (index, (value, ty)) in values.iter().zip(tys.iter()).enumerate() {
                let field = place.clone().field(index, ty.clone());
                push_const_fake_places(out, &field, ty, value);
            }
        }
        _ => push_fake_place(out, place, true),
    }
}

/// Adds to `out` the places before each `Deref` in `place`, innermost first, after `place`
/// itself when `itself` (a test reads it), skipping places already there (rustc's
/// `FakeBorrowCollector`).
fn push_fake_place(out: &mut Vec<Place>, place: &Place, itself: bool) {
    let mut push = |p: Place| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if itself {
        push(place.clone());
    }
    for (index, elem) in place.projection.iter().enumerate().rev() {
        if *elem == PlaceElem::Deref {
            push(Place {
                local: place.local,
                projection: place.projection[..index].to_vec(),
            });
        }
    }
}

/// Whether `c` is a range pattern matching every value of the integer type `ty`
/// (`i32::MIN..=i32::MAX`, `0u8..=255`): rustc tests nothing for it.
fn is_full_range(c: Option<&PatConst>, ty: &Ty) -> bool {
    let (
        Some(PatConst::Range {
            lo,
            hi,
            inclusive: true,
        }),
        Some(prim),
    ) = (c, ty.as_prim())
    else {
        return false;
    };
    if !prim.is_integer() {
        return false;
    }
    let (min, max) = sabiko_types::consts::int_bounds(prim);
    let lo_is_min = match *lo {
        ConstValue::Int(v) => v == min,
        ConstValue::Uint(v) => i128::try_from(v).is_ok_and(|v| v == min),
        _ => false,
    };
    let hi_is_max = match *hi {
        ConstValue::Int(v) => u128::try_from(v).is_ok_and(|v| v == max),
        ConstValue::Uint(v) => v == max,
        _ => false,
    };
    lo_is_min && hi_is_max
}

/// The operands of a match guard: its let chain's, or the guard alone.
fn guard_operands(guard: &Expr) -> Vec<&Expr> {
    guard.let_chain().unwrap_or_else(|| vec![guard])
}

/// Whether a match guard has a `let` (an `if let` guard or a let chain, #98).
fn guard_has_let(guard: &Expr) -> bool {
    guard_operands(guard)
        .iter()
        .any(|e| matches!(e.kind, ExprKind::Let(..)))
}
