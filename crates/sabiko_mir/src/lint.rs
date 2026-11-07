//! rustc's constant-propagation lints, `arithmetic_overflow` and `unconditional_panic`
//! (decision 73, #44): an operation whose operands are known at compile time and that
//! overflows or panics (an index out of bounds, a division by zero, `MIN / -1`) is an
//! error, as these deny-by-default lints make it in rustc.
//!
//! A port of rustc's `KnownPanicsLint` (decision 73). rustc runs it after drop
//! elaboration without simplifying the control flow again, so the walk here is over the
//! MIR as built, with its false edges removed: every drop as built still ends a block.
//! It visits the blocks reachable from the start depth first, as rustc does (a
//! `switchInt` on a known value takes only that edge, and successors are pushed in
//! `successors` order), and evaluates each statement from the values it knows; it knows
//! nothing a call returns and nothing behind a reference. Which locals hold values
//! (`Mode`, rustc's `CanConstProp`):
//!
//! - a local written once (an argument counts as written on entry) keeps its value until
//!   its `StorageDead`, which wipes it for every path visited later, as in rustc;
//! - a local written more than once keeps a value only up to the end of rustc's basic
//!   block: up to the next terminator other than a `goto` into a block that has no other
//!   predecessor (rustc's `SimplifyCfg` merged those), and up to every `assert` rustc's
//!   MIR has and Sabiko's does not: the bounds check of each index (once, where the index
//!   is evaluated); the zero and overflow checks of `/` and `%`; with overflow checks,
//!   those of `+`, `-`, `*`, `<<`, `>>`, unary `-` and a cast of a variant whose
//!   discriminant derives from an explicit one; and the calls rustc makes for `.len()`,
//!   string and reference comparisons and range slicing;
//! - a local borrowed (unless rustc promotes the borrow to a static, see `promotable`),
//!   dropped after drop elaboration (if only while unwinding) or larger than 1024 bytes
//!   holds no value.
//!
//! Only scalars travel through operands: a copy of a tuple, struct, array or enum is
//! unknown, a write to a part of a local forgets all of it, a payload read through a
//! downcast is unknown, and so is a value that overflowed. A computation behind a
//! promoted borrow is linted wherever the borrow is, as rustc lints a promoted constant's
//! own body.
//!
//! rustc's match lowering is a decision tree that tests each place once; Sabiko's tests
//! candidate after candidate (decisions 57 and 72). So the walk carries facts along its
//! edges (`Allowed`): what each `switchInt` edge says about the local switched on, and
//! about `x` when the switch is on `b = x op k`. A later candidate's repeated test that
//! the facts decide takes only the edge rustc's tree would, so an arm that only a failed
//! guard reaches in rustc is dead here too. Neither of two ways of treating a block
//! reached again (visit it again whenever the new way in knows less, or only to take an
//! edge a repeated test is decided to) follows rustc everywhere, each erring in other
//! programs, so the body is walked both ways and a lint is reported only when both find
//! it: Sabiko may miss a lint rustc reports, but does not report one rustc does not
//! (checked against rustc on generated programs, `crates/sabikoc/tests/generated.rs`).
//! Each lint is reported once, at the statement, with rustc's message and label.

use std::collections::{BTreeMap, HashMap, HashSet};

use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::PrimTy;
use sabiko_span::Span;
use sabiko_types::{ConstValue, Ty, TypeckResults};

use crate::{
    AggregateKind, BasicBlock, BinOp, Body, BorrowKind, Local, Operand, Place, PlaceElem, Rvalue,
    StatementKind, TerminatorKind, UnOp,
};

/// rustc's two lints. Each can be allowed (`sabikoc -A arithmetic_overflow`), as rustc's
/// `-A` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lint {
    ArithmeticOverflow,
    UnconditionalPanic,
}

impl Lint {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Lint::ArithmeticOverflow => "arithmetic_overflow",
            Lint::UnconditionalPanic => "unconditional_panic",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "arithmetic_overflow" => Some(Lint::ArithmeticOverflow),
            "unconditional_panic" => Some(Lint::UnconditionalPanic),
            _ => None,
        }
    }

    fn code(self) -> ErrorCode {
        match self {
            Lint::ArithmeticOverflow => ErrorCode::ARITHMETIC_OVERFLOW,
            Lint::UnconditionalPanic => ErrorCode::UNCONDITIONAL_PANIC,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Lint::ArithmeticOverflow => "this arithmetic operation will overflow",
            Lint::UnconditionalPanic => "this operation will panic at runtime",
        }
    }
}

/// The lints left unreported (rustc's `-A`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllowedLints {
    pub arithmetic_overflow: bool,
    pub unconditional_panic: bool,
}

impl AllowedLints {
    #[must_use]
    pub fn contains(self, lint: Lint) -> bool {
        match lint {
            Lint::ArithmeticOverflow => self.arithmetic_overflow,
            Lint::UnconditionalPanic => self.unconditional_panic,
        }
    }

    pub fn insert(&mut self, lint: Lint) {
        match lint {
            Lint::ArithmeticOverflow => self.arithmetic_overflow = true,
            Lint::UnconditionalPanic => self.unconditional_panic = true,
        }
    }
}

/// How the lints run.
#[derive(Clone, Copy, Debug, Default)]
pub struct LintOptions {
    /// Debug arithmetic: rustc's MIR then has an `assert` after each `+`, `-`, `*`, shift
    /// and negation, which ends what rustc calls a basic block.
    pub overflow_checks: bool,
    /// Lints not reported.
    pub allowed: AllowedLints,
}

/// Lints every body. Each is linted as rustc sees it: after borrow checking, with its false
/// edges removed and its drops elaborated (on a copy; `bodies` are left as they are).
pub fn check_all(
    bodies: &[Body],
    typeck: &TypeckResults,
    options: &LintOptions,
    sink: &mut DiagnosticSink,
) {
    if options.allowed.arithmetic_overflow && options.allowed.unconditional_panic {
        return;
    }
    for body in bodies {
        // rustc lints the MIR after drop elaboration, which it does not simplify again:
        // every drop as built still ends a block (one elaborated away leaves `goto`s), but
        // only the drops elaboration keeps make a local unpropagated. So the walk is over
        // the MIR as built, without its false edges, and the dropped locals are those of
        // the elaborated MIR (whose locals keep their numbers; it only adds drop flags).
        let mut built = body.clone();
        crate::simplify::remove_false_edges(&mut built);
        let mut elaborated = built.clone();
        crate::elaborate::elaborate_body(&mut elaborated, typeck);
        let mut dropped = HashSet::new();
        let mut drop = |place: &Place| {
            if place.projection.first() != Some(&PlaceElem::Deref) {
                dropped.insert(place.local);
            }
        };
        for data in &elaborated.blocks {
            if let Some(TerminatorKind::Drop { place, .. }) =
                data.terminator.as_ref().map(|t| &t.kind)
            {
                drop(place);
            }
        }
        for cleanup in &elaborated.cleanups {
            for d in &cleanup.drops {
                drop(&d.place);
            }
        }
        let body = built;
        // Neither way of revisiting a block (see `Lints::run`) follows rustc everywhere,
        // and each errs in different programs: a lint is reported only when both walks
        // find it, so Sabiko does not reject what rustc accepts (it may accept a few
        // programs rustc rejects).
        let mut once = Lints::new(&body, typeck, options, &dropped, false);
        once.run();
        let mut again = Lints::new(&body, typeck, options, &dropped, true);
        again.run();
        let key = |d: &Diagnostic| (d.code, d.primary_span(), d.labels[0].message.clone());
        let confirmed: HashSet<_> = again.found.iter().map(key).collect();
        for d in once.found {
            if confirmed.contains(&key(&d)) {
                sink.emit(d);
            }
        }
    }
}

/// Which locals may hold a value, and where (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Full,
    OwnBlock,
    None,
}

/// A value known at compile time: a scalar, or a tuple, struct, array or enum with some of
/// its fields known.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Scalar(ConstValue),
    Aggregate {
        variant: Option<usize>,
        fields: Vec<Option<Value>>,
    },
}

/// The values a local the walk does not know may still hold, learnt from the `switchInt`
/// edges taken to reach a block, as disjoint inclusive ranges (of the value, or of a
/// discriminant's or a `bool`'s bits). rustc's decision tree tests each place once, so an
/// arm that only a failed guard leads to is dead there, while Sabiko's candidates test
/// again (decisions 57 and 72) and reach it, unless these facts rule the later test out.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Allowed(Vec<(i128, i128)>);

impl Allowed {
    fn full() -> Self {
        Allowed(vec![(i128::MIN, i128::MAX)])
    }

    fn point(v: i128) -> Self {
        Allowed(vec![(v, v)])
    }

    /// The values `x` with `x op k`.
    fn compared(op: BinOp, k: i128) -> Option<Self> {
        Some(match op {
            BinOp::Eq => Allowed::point(k),
            BinOp::Ne => Allowed::point(k).complement(),
            BinOp::Lt => Allowed(
                k.checked_sub(1)
                    .map(|h| (i128::MIN, h))
                    .into_iter()
                    .collect(),
            ),
            BinOp::Le => Allowed(vec![(i128::MIN, k)]),
            BinOp::Gt => Allowed(
                k.checked_add(1)
                    .map(|l| (l, i128::MAX))
                    .into_iter()
                    .collect(),
            ),
            BinOp::Ge => Allowed(vec![(k, i128::MAX)]),
            _ => return None,
        })
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn complement(&self) -> Self {
        let mut out = Vec::new();
        let mut next = Some(i128::MIN);
        for &(lo, hi) in &self.0 {
            if let Some(n) = next {
                if lo > n {
                    out.push((n, lo - 1));
                }
            }
            next = hi.checked_add(1);
        }
        if let Some(n) = next {
            out.push((n, i128::MAX));
        }
        Allowed(out)
    }

    fn intersect(&self, other: &Self) -> Self {
        let mut out = Vec::new();
        for &(a, b) in &self.0 {
            for &(c, d) in &other.0 {
                let (lo, hi) = (a.max(c), b.min(d));
                if lo <= hi {
                    out.push((lo, hi));
                }
            }
        }
        out.sort_unstable();
        Allowed(out)
    }

    fn join(&self, other: &Self) -> Self {
        let mut all: Vec<(i128, i128)> = self.0.iter().chain(&other.0).copied().collect();
        all.sort_unstable();
        let mut out: Vec<(i128, i128)> = Vec::new();
        for (lo, hi) in all {
            match out.last_mut() {
                Some(last) if last.1.checked_add(1).is_none_or(|n| lo <= n) => {
                    last.1 = last.1.max(hi);
                }
                _ => out.push((lo, hi)),
            }
        }
        Allowed(out)
    }

    fn subset_of(&self, other: &Self) -> bool {
        self.intersect(other) == *self
    }
}

type Facts = BTreeMap<Local, Allowed>;

fn join_facts(a: &Facts, b: &Facts) -> Facts {
    a.iter()
        .filter_map(|(local, x)| b.get(local).map(|y| (*local, x.join(y))))
        .collect()
}

/// A scalar as a point of [`Allowed`]'s line: an integer (an unsigned one above
/// `i128::MAX` has none), a `bool`, a `char`.
fn key(v: &ConstValue) -> Option<i128> {
    match v {
        ConstValue::Int(v) => Some(*v),
        ConstValue::Uint(v) => i128::try_from(*v).ok(),
        ConstValue::Bool(b) => Some(i128::from(*b)),
        ConstValue::Char(c) => Some(i128::from(u32::from(*c))),
        _ => None,
    }
}

/// rustc's `MAX_ALLOC_LIMIT`: a local of a larger type is not propagated.
const MAX_SIZE: u64 = 1024;

struct Lints<'a> {
    body: &'a Body,
    typeck: &'a TypeckResults,
    options: &'a LintOptions,
    modes: Vec<Mode>,
    values: Vec<Option<Value>>,
    /// The locals in `Mode::OwnBlock` written since the current rustc block began.
    written: Vec<Local>,
    preds: Vec<usize>,
    found: Vec<Diagnostic>,
    reported: HashSet<(Span, Lint)>,
    /// What the edges taken tell about unknown locals, in the block being visited.
    facts: Facts,
    /// The blocks visited.
    visited: Vec<bool>,
    /// With `revisit`: the facts each block was entered with, joined over its visits.
    entry: Vec<Option<Facts>>,
    /// Visit a block again whenever a new way in knows less (rather than only for a
    /// repeated test its facts decide).
    revisit: bool,
    /// The `switchInt` edges taken, by block.
    taken: HashSet<(BasicBlock, BasicBlock)>,
    /// The block being visited.
    current: BasicBlock,
    /// `b = x op k` for a comparison `op`, while neither was written again.
    tests: HashMap<Local, (Local, BinOp, i128)>,
    /// The index locals whose bounds check was made since they were written: rustc checks
    /// an index once, where the index expression is evaluated, however often the place is
    /// used afterwards (a `match` on `a[i]` tests it several times).
    checked: HashSet<Local>,
}

impl<'a> Lints<'a> {
    fn new(
        body: &'a Body,
        typeck: &'a TypeckResults,
        options: &'a LintOptions,
        dropped: &HashSet<Local>,
        revisit: bool,
    ) -> Self {
        let mut preds = vec![0; body.blocks.len()];
        for data in &body.blocks {
            if let Some(term) = &data.terminator {
                for target in term.kind.successors() {
                    preds[target.index()] += 1;
                }
            }
        }
        let mut cx = Self {
            body,
            typeck,
            options,
            modes: Vec::new(),
            values: vec![None; body.locals.len()],
            written: Vec::new(),
            preds,
            found: Vec::new(),
            reported: HashSet::new(),
            facts: Facts::new(),
            visited: vec![false; body.blocks.len()],
            entry: vec![None; body.blocks.len()],
            revisit,
            taken: HashSet::new(),
            current: BasicBlock::START,
            tests: HashMap::new(),
            checked: HashSet::new(),
        };
        cx.modes = cx.modes(dropped);
        cx
    }

    // ----- which locals hold values -----

    fn modes(&self, dropped: &HashSet<Local>) -> Vec<Mode> {
        let body = self.body;
        let mut modes: Vec<Mode> = body
            .locals
            .iter()
            .enumerate()
            .map(|(i, decl)| match self.size(&decl.ty) {
                _ if dropped
                    .contains(&Local(u32::try_from(i).expect("fewer than 2^32 locals"))) =>
                {
                    Mode::None
                }
                Some(size) if size <= MAX_SIZE => Mode::Full,
                _ => Mode::None,
            })
            .collect();
        let mut written: HashSet<Local> = body.args().collect();
        let mut store = |modes: &mut Vec<Mode>, place: &Place| {
            if place.projection.first() == Some(&PlaceElem::Deref) {
                return;
            }
            let local = place.local;
            if !written.insert(local) && modes[local.index()] == Mode::Full {
                modes[local.index()] = Mode::OwnBlock;
            }
        };
        let forbid = |modes: &mut Vec<Mode>, place: &Place| {
            if place.projection.first() != Some(&PlaceElem::Deref) {
                modes[place.local.index()] = Mode::None;
            }
        };
        for data in &body.blocks {
            for stmt in &data.statements {
                if let StatementKind::Assign(place, rvalue) = &stmt.kind {
                    store(&mut modes, place);
                    if let Rvalue::Ref(kind, borrowed) = rvalue {
                        // rustc removes fake borrows before this lint, and promotes a
                        // shared borrow of a constant temporary to a static.
                        let promoted = *kind == BorrowKind::Shared && self.promotable(borrowed);
                        if kind.fake().is_none() && !promoted {
                            forbid(&mut modes, borrowed);
                        }
                    }
                }
            }
            if let TerminatorKind::Call { destination, .. } = &data.terminator().kind {
                store(&mut modes, destination);
            }
        }
        modes
    }

    /// Whether rustc would promote a shared borrow of `place` to a static (a rough model of
    /// `promote_consts`): a field or variant path into a temporary without drop glue
    /// written once,
    /// from constants through casts, unary and binary operators (a division only by a
    /// nonzero constant other than `-1`) and aggregates. A guard's binding of a constant
    /// scrutinee (`match 0 { d if g => .. }`) borrows one.
    fn promotable(&self, place: &Place) -> bool {
        place
            .projection
            .iter()
            .all(|e| matches!(e, PlaceElem::Field(..) | PlaceElem::Downcast(_)))
            && self.promotable_local(place.local, 0)
    }

    fn promotable_local(&self, local: Local, depth: u32) -> bool {
        let decl = &self.body.locals[local.index()];
        if decl.kind != crate::LocalKind::Temp
            || depth > 16
            || self.typeck.items.needs_drop(&decl.ty)
        {
            return false;
        }
        let mut writes = Vec::new();
        for data in &self.body.blocks {
            for stmt in &data.statements {
                if let StatementKind::Assign(place, rvalue) = &stmt.kind {
                    if place.local == local {
                        writes.push((place.projection.is_empty(), rvalue));
                    }
                }
            }
            if let TerminatorKind::Call { destination, .. } = &data.terminator().kind {
                if destination.local == local {
                    return false;
                }
            }
        }
        let [(true, rvalue)] = writes[..] else {
            return false;
        };
        let operand = |op: &Operand| match op {
            Operand::Constant(_) => true,
            Operand::Copy(p) | Operand::Move(p) => {
                p.projection.is_empty() && self.promotable_local(p.local, depth + 1)
            }
        };
        match rvalue {
            Rvalue::Use(op) | Rvalue::Cast(op, _) | Rvalue::UnaryOp(_, op) => operand(op),
            Rvalue::BinaryOp(op, l, r) => {
                let divisor_ok = !matches!(op, BinOp::Div | BinOp::Rem)
                    || matches!(r, Operand::Constant(c)
                        if !matches!(c.value, ConstValue::Int(0 | -1) | ConstValue::Uint(0)));
                divisor_ok && operand(l) && operand(r)
            }
            Rvalue::Aggregate(_, ops) => ops.iter().all(operand),
            _ => false,
        }
    }

    /// An estimate of the size of a value of `ty` in bytes, as rustc lays it out (fields
    /// reordered, so only the tail is padded); `None` when unknown.
    fn size(&self, ty: &Ty) -> Option<u64> {
        self.layout(ty).map(|(size, _)| size)
    }

    fn layout(&self, ty: &Ty) -> Option<(u64, u64)> {
        let fields = |tys: &mut dyn Iterator<Item = &Ty>| -> Option<(u64, u64)> {
            let mut size = 0u64;
            let mut align = 1u64;
            for ty in tys {
                let (s, a) = self.layout(ty)?;
                size = size.checked_add(s)?;
                align = align.max(a);
            }
            Some((size.div_ceil(align) * align, align))
        };
        Some(match ty {
            Ty::Prim(p) => match p {
                PrimTy::Bool | PrimTy::I8 | PrimTy::U8 => (1, 1),
                PrimTy::I16 | PrimTy::U16 => (2, 2),
                PrimTy::I32 | PrimTy::U32 | PrimTy::F32 | PrimTy::Char => (4, 4),
                PrimTy::I64 | PrimTy::U64 | PrimTy::Isize | PrimTy::Usize | PrimTy::F64 => (8, 8),
                PrimTy::I128 | PrimTy::U128 => (16, 16),
                PrimTy::Str => return None,
            },
            Ty::Unit | Ty::Never | Ty::FnDef(_) => (0, 1),
            Ty::Ref(_, inner) => match **inner {
                Ty::Slice(_) | Ty::Prim(PrimTy::Str) => (16, 8),
                _ => (8, 8),
            },
            Ty::Tuple(tys) => fields(&mut tys.iter())?,
            Ty::Array(elem, n) => {
                let (s, a) = self.layout(elem)?;
                (s.checked_mul(*n)?, a)
            }
            Ty::Adt(def) => {
                let adt = self.typeck.items.adt(*def);
                if adt.is_enum() {
                    let tag = (8, 8);
                    let mut size = 0u64;
                    let mut align = 1u64;
                    for v in adt.variants() {
                        let (s, a) = fields(&mut v.fields.iter().map(|f| &f.ty))?;
                        size = size.max(s);
                        align = align.max(a);
                    }
                    let align = align.max(tag.1);
                    ((size + tag.0).div_ceil(align) * align, align)
                } else {
                    fields(&mut adt.fields.iter().map(|f| &f.ty))?
                }
            }
            Ty::Slice(_) | Ty::Infer(_) | Ty::Error => return None,
        })
    }

    // ----- the walk -----

    fn run(&mut self) {
        self.lint_promoted();
        let mut worklist = vec![(BasicBlock::START, Facts::new())];
        while let Some((bb, facts)) = worklist.pop() {
            // Each block is visited once, with what the first way in tells, as rustc visits
            // it once: visiting again on a way that knows less would explore arms rustc's
            // tree never reaches, with values a `StorageDead` has wiped meanwhile. The one
            // exception is a test a candidate repeats, which rustc's tree does not have: a
            // later way in whose facts decide it to an edge not taken yet takes that edge.
            let facts = if self.revisit {
                match &self.entry[bb.index()] {
                    None => facts,
                    Some(old) => {
                        let joined = join_facts(old, &facts);
                        if &joined == old {
                            continue;
                        }
                        joined
                    }
                }
            } else {
                facts
            };
            if self.revisit {
                self.entry[bb.index()] = Some(facts.clone());
            } else if std::mem::replace(&mut self.visited[bb.index()], true) {
                let decided = self.decided_switch(bb, &facts);
                if decided.is_none_or(|t| self.taken.contains(&(bb, t))) {
                    continue;
                }
            }
            self.facts = facts;
            self.tests.clear();
            self.block(bb, &mut worklist);
        }
    }

    /// rustc moves the computation behind a promoted borrow into a constant of its own and
    /// lints that one too, whether or not the borrow is reachable: `let E { A: ref a, .. }
    /// = E { A: 1u8 - 2, .. }` in a dead arm still reports the overflow.
    fn lint_promoted(&mut self) {
        let mut roots = Vec::new();
        for data in &self.body.blocks {
            for stmt in &data.statements {
                if let StatementKind::Assign(_, Rvalue::Ref(BorrowKind::Shared, p)) = &stmt.kind {
                    if self.promotable(p) {
                        roots.push(p.local);
                    }
                }
            }
        }
        if roots.is_empty() {
            return;
        }
        let saved = std::mem::replace(&mut self.values, vec![None; self.body.locals.len()]);
        let mut done = HashSet::new();
        for root in roots {
            self.promoted(root, &mut done);
        }
        self.values = saved;
        self.written.clear();
    }

    /// Evaluates the promotable temporary `local` after the ones it is computed from.
    fn promoted(&mut self, local: Local, done: &mut HashSet<Local>) {
        if !done.insert(local) {
            return;
        }
        let body = self.body;
        let Some((rvalue, span)) = body.blocks.iter().find_map(|data| {
            data.statements.iter().find_map(|stmt| match &stmt.kind {
                StatementKind::Assign(place, rvalue) if place.local == local => {
                    Some((rvalue, stmt.span))
                }
                _ => None,
            })
        }) else {
            return;
        };
        for op in rvalue.operands() {
            if let Operand::Copy(p) | Operand::Move(p) = op {
                self.promoted(p.local, done);
            }
        }
        self.values[local.index()] = self.rvalue(rvalue, span);
    }

    /// The edge `facts` decide the `switchInt` ending `bb` to, when it switches on a test
    /// `b = x op k` made in the block (or on a discriminant) that they decide.
    fn decided_switch(&self, bb: BasicBlock, facts: &Facts) -> Option<BasicBlock> {
        let data = self.body.block(bb);
        let TerminatorKind::SwitchInt {
            discr,
            targets,
            otherwise,
        } = &data.terminator().kind
        else {
            return None;
        };
        let local = discr.place().filter(|p| p.projection.is_empty())?.local;
        let test = data
            .statements
            .iter()
            .rev()
            .find_map(|stmt| match &stmt.kind {
                StatementKind::Assign(p, rvalue) if p.projection.is_empty() && p.local == local => {
                    Some(rvalue)
                }
                _ => None,
            });
        let known = match test {
            Some(Rvalue::BinaryOp(op, l, Operand::Constant(c))) => {
                let x = l.place().filter(|p| p.projection.is_empty())?.local;
                let set = Allowed::compared(*op, key(&c.value)?)?;
                let x = facts.get(&x)?;
                if x.subset_of(&set) {
                    1
                } else if x.intersect(&set).is_empty() {
                    0
                } else {
                    return None;
                }
            }
            Some(_) => return None,
            None => match facts.get(&local)?.0[..] {
                [(lo, hi)] if lo == hi => lo,
                _ => return None,
            },
        };
        #[allow(clippy::cast_possible_wrap)]
        Some(
            targets
                .iter()
                .find(|(v, _)| *v as i128 == known)
                .map_or(*otherwise, |(_, t)| *t),
        )
    }

    fn block(&mut self, bb: BasicBlock, worklist: &mut Vec<(BasicBlock, Facts)>) {
        self.current = bb;
        let data = self.body.block(bb);
        for stmt in &data.statements {
            self.statement(&stmt.kind, stmt.span);
        }
        let term = data.terminator();
        match &term.kind {
            TerminatorKind::Goto { target } => {
                worklist.push((*target, self.facts.clone()));
                // rustc's `SimplifyCfg` merged a block into its only predecessor.
                if self.preds[target.index()] != 1 || *target == BasicBlock::START {
                    self.end_block();
                }
                return;
            }
            TerminatorKind::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                self.check_operand_places(discr, term.span);
                // A known value takes one edge (what it tells is kept too: a `StorageDead`
                // on another path may wipe the value before a candidate tests it again).
                #[allow(clippy::cast_possible_wrap)]
                let bits = self
                    .operand(discr)
                    .and_then(|v| switch_bits(&v))
                    .map(|b| b as i128);
                self.switch_edges(discr, targets, *otherwise, bits, worklist);
            }
            TerminatorKind::Call {
                args, destination, ..
            } => {
                for arg in args {
                    self.check_operand_places(arg, term.span);
                }
                self.check_place(destination, term.span);
                self.store(destination, None);
                let facts = self.facts.clone();
                worklist.extend(
                    term.kind
                        .successors()
                        .into_iter()
                        .map(|bb| (bb, facts.clone())),
                );
            }
            _ => {
                let facts = self.facts.clone();
                worklist.extend(
                    term.kind
                        .successors()
                        .into_iter()
                        .map(|bb| (bb, facts.clone())),
                );
            }
        }
        self.end_block();
    }

    /// The edges of a `switchInt` on a value the walk does not know, each with what it
    /// tells, and without those the facts rule out.
    fn switch_edges(
        &mut self,
        discr: &Operand,
        targets: &[(u128, BasicBlock)],
        otherwise: BasicBlock,
        value: Option<i128>,
        worklist: &mut Vec<(BasicBlock, Facts)>,
    ) {
        let local = discr
            .place()
            .filter(|p| p.projection.is_empty())
            .map(|p| p.local);
        let Some(local) = local else {
            #[allow(clippy::cast_possible_wrap)]
            let taken = value.map(|v| {
                targets
                    .iter()
                    .find(|(t, _)| *t as i128 == v)
                    .map_or(otherwise, |(_, bb)| *bb)
            });
            let all = targets.iter().map(|(_, bb)| *bb).chain([otherwise]);
            for bb in all.filter(|bb| taken.is_none_or(|t| t == *bb)) {
                self.taken.insert((self.current, bb));
                worklist.push((bb, self.facts.clone()));
            }
            return;
        };
        let mut known = self.facts.get(&local).cloned().unwrap_or_else(|| {
            if self.body.locals[local.index()].ty.is_bool() {
                Allowed(vec![(0, 1)])
            } else {
                Allowed::full()
            }
        });
        if let Some(v) = value {
            known = Allowed::point(v);
        }
        #[allow(clippy::cast_possible_wrap)]
        let points: Vec<(i128, BasicBlock)> =
            targets.iter().map(|(v, bb)| (*v as i128, *bb)).collect();
        // In `successors` order, as rustc pushes them: the targets, then `otherwise`.
        let mut edges: Vec<(BasicBlock, Allowed)> = points
            .iter()
            .map(|&(v, bb)| (bb, known.intersect(&Allowed::point(v))))
            .collect();
        let taken = points.iter().fold(Allowed(Vec::new()), |acc, &(v, _)| {
            acc.join(&Allowed::point(v))
        });
        edges.push((otherwise, known.intersect(&taken.complement())));
        for (bb, allowed) in edges {
            if allowed.is_empty() {
                continue;
            }
            let mut facts = self.facts.clone();
            // `b = x op k`: an edge that fixes `b` tells about `x`.
            if let Some(&(x, op, k)) = self.tests.get(&local) {
                let holds = match allowed.0[..] {
                    [(0, 0)] => Some(false),
                    [(1, 1)] => Some(true),
                    _ => None,
                };
                if let (Some(holds), Some(set)) = (holds, Allowed::compared(op, k)) {
                    let set = if holds { set } else { set.complement() };
                    let before = facts.get(&x).cloned().unwrap_or_else(Allowed::full);
                    let after = before.intersect(&set);
                    if after.is_empty() {
                        // The edge contradicts what is known: rustc's tree has no such way.
                        continue;
                    }
                    facts.insert(x, after);
                }
            }
            facts.insert(local, allowed);
            self.taken.insert((self.current, bb));
            worklist.push((bb, facts));
        }
    }

    /// `local` was written: what the edges told about it, and the tests reading it, are
    /// stale.
    fn forget(&mut self, local: Local) {
        self.facts.remove(&local);
        self.checked.remove(&local);
        self.tests.retain(|b, (x, _, _)| *b != local && *x != local);
    }

    /// The end of a rustc basic block: the locals written more than once lose their values.
    fn end_block(&mut self) {
        for local in std::mem::take(&mut self.written) {
            self.values[local.index()] = None;
        }
    }

    fn statement(&mut self, kind: &StatementKind, span: Span) {
        match kind {
            StatementKind::Assign(place, rvalue) => {
                let value = self.rvalue(rvalue, span);
                self.check_place(place, span);
                let unknown = value.is_none();
                self.store(place, value);
                // An unknown discriminant is still one of its enum's.
                if let (true, true, Rvalue::Discriminant(p)) =
                    (unknown, place.projection.is_empty(), rvalue)
                {
                    if let Ty::Adt(def) = p.ty(self.body) {
                        let discrs = self
                            .typeck
                            .items
                            .adt(def)
                            .variants()
                            .iter()
                            .fold(Allowed(Vec::new()), |acc, v| {
                                acc.join(&Allowed::point(v.discr))
                            });
                        self.facts.insert(place.local, discrs);
                    }
                }
                if let (true, Rvalue::BinaryOp(op, l, Operand::Constant(c))) =
                    (place.projection.is_empty(), rvalue)
                {
                    let x = l
                        .place()
                        .filter(|p| p.projection.is_empty())
                        .map(|p| p.local);
                    if let (Some(x), Some(k), true) = (x, key(&c.value), op.is_comparison()) {
                        self.tests.insert(place.local, (x, *op, k));
                    }
                }
            }
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                self.values[local.index()] = None;
                self.forget(*local);
            }
            StatementKind::PlaceMention(place) => self.check_place(place, span),
            StatementKind::Nop => {}
        }
    }

    fn store(&mut self, place: &Place, value: Option<Value>) {
        if place.projection.first() == Some(&PlaceElem::Deref) {
            return;
        }
        let local = place.local;
        self.forget(local);
        match self.modes[local.index()] {
            Mode::None => return,
            Mode::OwnBlock => self.written.push(local),
            Mode::Full => {}
        }
        // A write to a part of the local forgets all of it, as in rustc.
        self.values[local.index()] = if place.projection.is_empty() {
            value
        } else {
            None
        };
    }

    // ----- places -----

    /// The bounds checks of the indices in `place`, each followed by the end of a rustc
    /// block (its `assert`).
    fn check_place(&mut self, place: &Place, span: Span) {
        if !place
            .projection
            .iter()
            .any(|e| matches!(e, PlaceElem::Index(_)))
        {
            return;
        }
        let mut ty = self.body.locals[place.local.index()].ty.clone();
        let mut prefix = Place::local(place.local);
        for elem in &place.projection {
            if let PlaceElem::Index(index) = elem {
                if !self.checked.insert(*index) {
                    // Checked where it was evaluated.
                } else if let Ty::Array(_, len) = &ty {
                    let index = self.values[index.index()].as_ref().and_then(as_u128);
                    if let Some(index) = index.filter(|i| *i >= u128::from(*len)) {
                        // rustc shows `usize::MAX` by name and other indices bare.
                        let shown = if index == u128::from(u64::MAX) {
                            "usize::MAX".to_owned()
                        } else {
                            index.to_string()
                        };
                        self.report(
                            Lint::UnconditionalPanic,
                            span,
                            format!(
                                "index out of bounds: the length is {len} but the index is {shown}"
                            ),
                        );
                    }
                    self.end_block();
                } else {
                    self.end_block();
                }
            }
            prefix = Place {
                local: prefix.local,
                projection: [prefix.projection, vec![elem.clone()]].concat(),
            };
            ty = prefix.ty(self.body);
        }
    }

    fn check_operand_places(&mut self, op: &Operand, span: Span) {
        if let Some(place) = op.place() {
            self.check_place(place, span);
        }
    }

    fn place(&self, place: &Place) -> Option<Value> {
        let mut value = self.values[place.local.index()].clone()?;
        for elem in &place.projection {
            value = match (elem, value) {
                (
                    PlaceElem::Field(i, _),
                    Value::Aggregate {
                        variant: None,
                        fields,
                    },
                ) => fields.into_iter().nth(*i)??,
                (
                    PlaceElem::Index(index),
                    Value::Aggregate {
                        variant: None,
                        fields,
                    },
                ) => {
                    let i = self.values[index.index()].as_ref().and_then(as_u128)?;
                    fields.into_iter().nth(usize::try_from(i).ok()?)??
                }
                _ => return None,
            };
        }
        Some(value)
    }

    /// The value of an operand, which rustc knows only for a scalar: a copy of a tuple,
    /// struct, array or enum is unknown, and so is a tuple constant.
    fn operand(&self, op: &Operand) -> Option<Value> {
        let value = match op {
            Operand::Copy(p) | Operand::Move(p) => self.place(p)?,
            Operand::Constant(c) => constant(&c.value)?,
        };
        matches!(value, Value::Scalar(_)).then_some(value)
    }

    // ----- right-hand sides -----

    fn rvalue(&mut self, rvalue: &Rvalue, span: Span) -> Option<Value> {
        for op in rvalue.operands() {
            self.check_operand_places(op, span);
        }
        match rvalue {
            Rvalue::Ref(_, p) | Rvalue::Discriminant(p) | Rvalue::Len(p) => {
                self.check_place(p, span);
            }
            _ => {}
        }
        match rvalue {
            Rvalue::Use(op) => self.operand(op),
            // rustc evaluates neither a repeat nor a pointer coercion.
            Rvalue::Ref(..) | Rvalue::PointerCoercion(..) | Rvalue::Repeat(..) => None,
            Rvalue::UnaryOp(op, operand) => self.unary(*op, operand, span),
            Rvalue::BinaryOp(op, l, r) => self.binary(*op, l, r, span),
            Rvalue::Cast(op, ty) => {
                let from = op.ty(self.body);
                // `K::X as usize` casts the variant just built (rustc casts the
                // discriminant's constant); a copied enum is unknown, as everywhere.
                let value = match op {
                    Operand::Copy(p) | Operand::Move(p) => self.place(p),
                    Operand::Constant(_) => self.operand(op),
                };
                let value = match value? {
                    Value::Scalar(v) => v,
                    Value::Aggregate {
                        variant: Some(v), ..
                    } => {
                        let Ty::Adt(def) = from else { return None };
                        let variant = self.typeck.items.adt(def).variant(v);
                        if variant.explicit_base && self.options.overflow_checks {
                            // rustc adds the offset to the explicit discriminant with an
                            // overflow check, which ends its block.
                            self.end_block();
                        }
                        ConstValue::Int(variant.discr)
                    }
                    Value::Aggregate { .. } => return None,
                };
                let to = ty.as_prim()?;
                let from = from.as_prim().or(Some(PrimTy::Isize));
                sabiko_types::consts::cast(&value, from, to).map(Value::Scalar)
            }
            Rvalue::Aggregate(kind, ops) => {
                let fields = ops.iter().map(|op| self.operand(op)).collect();
                let variant = match kind {
                    AggregateKind::Variant(_, v) => Some(*v),
                    AggregateKind::Adt(_) | AggregateKind::Tuple | AggregateKind::Array(_) => None,
                };
                Some(Value::Aggregate { variant, fields })
            }
            Rvalue::Discriminant(p) => match self.place(p)? {
                Value::Aggregate {
                    variant: Some(v), ..
                } => {
                    let Ty::Adt(def) = p.ty(self.body) else {
                        return None;
                    };
                    Some(Value::Scalar(ConstValue::Int(
                        self.typeck.items.adt(def).variant(v).discr,
                    )))
                }
                _ => None,
            },
            // Calls in rustc (`<[T]>::len`, `Index::index`): the end of a block.
            Rvalue::Len(_) | Rvalue::Subslice(..) => {
                self.end_block();
                None
            }
        }
    }

    fn unary(&mut self, op: UnOp, operand: &Operand, span: Span) -> Option<Value> {
        let ty = operand.ty(self.body);
        let prim = ty.as_prim();
        let value = self.operand(operand);
        let checked = op == UnOp::Neg && prim.is_some_and(PrimTy::is_signed_int);
        if checked {
            if let (Some(Value::Scalar(ConstValue::Int(v))), Some(p)) = (&value, prim) {
                let (min, _) = sabiko_types::consts::int_bounds(p);
                if *v == min {
                    let shown = render(&ConstValue::Int(*v), p);
                    self.report(
                        Lint::ArithmeticOverflow,
                        span,
                        format!("attempt to negate `{shown}`, which would overflow"),
                    );
                    if self.options.overflow_checks {
                        self.end_block();
                    }
                    return None;
                }
            }
            if self.options.overflow_checks {
                // The check comes first; the negation reads the operand again after it.
                self.end_block();
                return negate(&self.operand(operand)?);
            }
        }
        let Value::Scalar(v) = value? else {
            return None;
        };
        Some(Value::Scalar(match (op, v) {
            (UnOp::Neg, ConstValue::Int(v)) => ConstValue::Int(v.checked_neg()?),
            (UnOp::Neg, ConstValue::Float(f)) => ConstValue::Float(-f),
            (UnOp::Not, ConstValue::Bool(b)) => ConstValue::Bool(!b),
            (UnOp::Not, ConstValue::Int(v)) => ConstValue::Int(!v),
            (UnOp::Not, ConstValue::Uint(v)) => {
                let (_, max) = sabiko_types::consts::int_bounds(prim?);
                ConstValue::Uint(!v & max)
            }
            _ => return None,
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn binary(&mut self, op: BinOp, l: &Operand, r: &Operand, span: Span) -> Option<Value> {
        let lty = l.ty(self.body);
        let Some(prim) = lty.as_prim().filter(|p| *p != PrimTy::Str) else {
            // A comparison of references or strings: a call in rustc.
            self.end_block();
            return None;
        };
        let lv = scalar(self.operand(l));
        let rv = scalar(self.operand(r));
        let rprim = r.ty(self.body).as_prim();
        if !prim.is_integer() {
            let (Some(a), Some(b)) = (lv, rv) else {
                return None;
            };
            return float_or_bool(op, &a, &b, prim).map(Value::Scalar);
        }
        let shown = |v: &Option<ConstValue>, p: Option<PrimTy>| match (v, p) {
            (Some(v), Some(p)) => render(v, p),
            _ => "_".to_owned(),
        };
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let computed = match (&lv, &rv) {
                    (Some(lhs), Some(rhs)) => Some(arith(op, lhs, rhs, prim)),
                    _ => None,
                };
                let result = match computed {
                    Some(Some(v)) => Some(Value::Scalar(v)),
                    Some(None) => {
                        self.report(
                            Lint::ArithmeticOverflow,
                            span,
                            format!(
                                "attempt to compute `{} {} {}`, which would overflow",
                                shown(&lv, Some(prim)),
                                op.as_str(),
                                shown(&rv, rprim)
                            ),
                        );
                        None
                    }
                    None => None,
                };
                if self.options.overflow_checks {
                    self.end_block();
                }
                result
            }
            BinOp::Shl | BinOp::Shr => {
                let bits = prim.int_bits()?;
                let amount = rv.as_ref().and_then(|v| shift_bits(v, rprim));
                if amount.is_some_and(|a| a >= u128::from(bits)) {
                    let dir = if op == BinOp::Shl { "left" } else { "right" };
                    self.report(
                        Lint::ArithmeticOverflow,
                        span,
                        format!(
                            "attempt to shift {dir} by `{}`, which would overflow",
                            shown(&rv, rprim)
                        ),
                    );
                    if self.options.overflow_checks {
                        self.end_block();
                    }
                    return None;
                }
                if self.options.overflow_checks {
                    self.end_block();
                }
                let lv = scalar(self.operand(l))?;
                let amount = u32::try_from(shift_bits(&scalar(self.operand(r))?, rprim)?).ok()?;
                shift(op, &lv, amount, prim).map(Value::Scalar)
            }
            BinOp::Div | BinOp::Rem => {
                let zero = rv.as_ref().is_some_and(is_zero);
                if zero {
                    let text = if op == BinOp::Div {
                        format!("attempt to divide `{}` by zero", shown(&lv, Some(prim)))
                    } else {
                        format!(
                            "attempt to calculate the remainder of `{}` with a divisor of zero",
                            shown(&lv, Some(prim))
                        )
                    };
                    self.report(Lint::UnconditionalPanic, span, text);
                    self.end_block();
                    return None;
                }
                if prim.is_signed_int() {
                    let (min, _) = sabiko_types::consts::int_bounds(prim);
                    let overflow = matches!((&lv, &rv),
                        (Some(ConstValue::Int(a)), Some(ConstValue::Int(-1))) if *a == min);
                    if overflow {
                        self.report(
                            Lint::UnconditionalPanic,
                            span,
                            format!(
                                "attempt to compute `{} {} {}`, which would overflow",
                                shown(&lv, Some(prim)),
                                op.as_str(),
                                shown(&rv, rprim)
                            ),
                        );
                        self.end_block();
                        return None;
                    }
                }
                // The checks come first; the division reads its operands again after them.
                self.end_block();
                let a = scalar(self.operand(l))?;
                let b = scalar(self.operand(r))?;
                arith(op, &a, &b, prim).map(Value::Scalar)
            }
            _ => {
                let (Some(a), Some(b)) = (lv, rv) else {
                    return self.decided_by_facts(op, l, r);
                };
                bitwise_or_compare(op, &a, &b).map(Value::Scalar)
            }
        }
    }

    /// `x op k` for an `x` the walk does not know and a comparison `op`, which the edges
    /// taken may decide (a test a candidate repeats).
    fn decided_by_facts(&self, op: BinOp, l: &Operand, r: &Operand) -> Option<Value> {
        let x = l.place().filter(|p| p.projection.is_empty())?.local;
        let Operand::Constant(c) = r else {
            return None;
        };
        let set = Allowed::compared(op, key(&c.value)?)?;
        let known = self.facts.get(&x)?;
        let holds = if known.subset_of(&set) {
            true
        } else if known.intersect(&set).is_empty() {
            false
        } else {
            return None;
        };
        Some(Value::Scalar(ConstValue::Bool(holds)))
    }

    fn report(&mut self, lint: Lint, span: Span, label: String) {
        if self.options.allowed.contains(lint) || !self.reported.insert((span, lint)) {
            return;
        }
        self.found
            .push(Diagnostic::error(lint.code(), lint.message()).with_label(span, label));
    }
}

// ----- values -----

fn constant(value: &ConstValue) -> Option<Value> {
    match value {
        ConstValue::Str(_) | ConstValue::Tuple(_) => None,
        v => Some(Value::Scalar(v.clone())),
    }
}

fn scalar(value: Option<Value>) -> Option<ConstValue> {
    match value? {
        Value::Scalar(v) => Some(v),
        Value::Aggregate { .. } => None,
    }
}

fn as_u128(value: &Value) -> Option<u128> {
    match value {
        Value::Scalar(ConstValue::Uint(v)) => Some(*v),
        Value::Scalar(ConstValue::Int(v)) => u128::try_from(*v).ok(),
        _ => None,
    }
}

/// The value a `switchInt` compares with its targets: `0`/`1` for a `bool`, the 128-bit
/// two's complement pattern of an integer.
fn switch_bits(value: &Value) -> Option<u128> {
    #[allow(clippy::cast_sign_loss)]
    match value {
        Value::Scalar(ConstValue::Bool(b)) => Some(u128::from(*b)),
        Value::Scalar(ConstValue::Uint(v)) => Some(*v),
        Value::Scalar(ConstValue::Int(v)) => Some(*v as u128),
        _ => None,
    }
}

fn is_zero(v: &ConstValue) -> bool {
    matches!(v, ConstValue::Int(0) | ConstValue::Uint(0))
}

/// The shift amount as rustc checks it: the bits of the right operand, unsigned (so a
/// negative amount is huge).
fn shift_bits(v: &ConstValue, prim: Option<PrimTy>) -> Option<u128> {
    #[allow(clippy::cast_sign_loss)]
    match v {
        ConstValue::Uint(u) => Some(*u),
        ConstValue::Int(i) => {
            let bits = prim?.int_bits()?;
            let raw = *i as u128;
            Some(if bits == 128 {
                raw
            } else {
                raw & ((1u128 << bits) - 1)
            })
        }
        _ => None,
    }
}

fn negate(value: &Value) -> Option<Value> {
    match *value {
        Value::Scalar(ConstValue::Int(v)) => Some(Value::Scalar(ConstValue::Int(v.checked_neg()?))),
        _ => None,
    }
}

/// `a op b` for `+`, `-`, `*`, `/` and `%` on integers of type `prim`; `None` when it
/// overflows or divides by zero.
fn arith(op: BinOp, a: &ConstValue, b: &ConstValue, prim: PrimTy) -> Option<ConstValue> {
    let (min, max) = sabiko_types::consts::int_bounds(prim);
    match (a, b) {
        (ConstValue::Int(a), ConstValue::Int(b)) => {
            let v = match op {
                BinOp::Add => a.checked_add(*b),
                BinOp::Sub => a.checked_sub(*b),
                BinOp::Mul => a.checked_mul(*b),
                BinOp::Div => a.checked_div(*b),
                BinOp::Rem => a.checked_rem(*b),
                _ => None,
            }?;
            #[allow(clippy::cast_possible_wrap)]
            (v >= min && v <= max as i128).then_some(ConstValue::Int(v))
        }
        (ConstValue::Uint(a), ConstValue::Uint(b)) => {
            let v = match op {
                BinOp::Add => a.checked_add(*b),
                BinOp::Sub => a.checked_sub(*b),
                BinOp::Mul => a.checked_mul(*b),
                BinOp::Div => a.checked_div(*b),
                BinOp::Rem => a.checked_rem(*b),
                _ => None,
            }?;
            (v <= max).then_some(ConstValue::Uint(v))
        }
        _ => None,
    }
}

fn shift(op: BinOp, a: &ConstValue, amount: u32, prim: PrimTy) -> Option<ConstValue> {
    let bits = prim.int_bits()?;
    let (_, max) = sabiko_types::consts::int_bounds(prim);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    Some(match (op, a) {
        (BinOp::Shl, ConstValue::Int(v)) => {
            let mask = if bits == 128 {
                u128::MAX
            } else {
                (1u128 << bits) - 1
            };
            let raw = ((*v as u128) << amount) & mask;
            ConstValue::Int(sign_extend(raw, bits))
        }
        (BinOp::Shr, ConstValue::Int(v)) => ConstValue::Int(v >> amount),
        (BinOp::Shl, ConstValue::Uint(v)) => ConstValue::Uint((v << amount) & max),
        (BinOp::Shr, ConstValue::Uint(v)) => ConstValue::Uint(v >> amount),
        _ => return None,
    })
}

#[allow(clippy::cast_possible_wrap)]
fn sign_extend(raw: u128, bits: u32) -> i128 {
    if bits == 128 {
        return raw as i128;
    }
    let sign = 1u128 << (bits - 1);
    if raw & sign == 0 {
        raw as i128
    } else {
        (raw | !((sign << 1) - 1)) as i128
    }
}

fn bitwise_or_compare(op: BinOp, a: &ConstValue, b: &ConstValue) -> Option<ConstValue> {
    use ConstValue as V;
    Some(match (op, a, b) {
        (BinOp::BitAnd, V::Int(a), V::Int(b)) => V::Int(a & b),
        (BinOp::BitOr, V::Int(a), V::Int(b)) => V::Int(a | b),
        (BinOp::BitXor, V::Int(a), V::Int(b)) => V::Int(a ^ b),
        (BinOp::BitAnd, V::Uint(a), V::Uint(b)) => V::Uint(a & b),
        (BinOp::BitOr, V::Uint(a), V::Uint(b)) => V::Uint(a | b),
        (BinOp::BitXor, V::Uint(a), V::Uint(b)) => V::Uint(a ^ b),
        (op, a, b) => V::Bool(compare(op, a, b)?),
    })
}

fn float_or_bool(op: BinOp, a: &ConstValue, b: &ConstValue, prim: PrimTy) -> Option<ConstValue> {
    use ConstValue as V;
    let round = |f: f64| {
        if prim == PrimTy::F32 {
            #[allow(clippy::cast_possible_truncation)]
            f64::from(f as f32)
        } else {
            f
        }
    };
    Some(match (op, a, b) {
        (BinOp::Add, V::Float(a), V::Float(b)) => V::Float(round(a + b)),
        (BinOp::Sub, V::Float(a), V::Float(b)) => V::Float(round(a - b)),
        (BinOp::Mul, V::Float(a), V::Float(b)) => V::Float(round(a * b)),
        (BinOp::Div, V::Float(a), V::Float(b)) => V::Float(round(a / b)),
        (BinOp::Rem, V::Float(a), V::Float(b)) => V::Float(round(a % b)),
        (BinOp::BitAnd, V::Bool(a), V::Bool(b)) => V::Bool(a & b),
        (BinOp::BitOr, V::Bool(a), V::Bool(b)) => V::Bool(a | b),
        (BinOp::BitXor, V::Bool(a), V::Bool(b)) => V::Bool(a ^ b),
        (op, a, b) => V::Bool(compare(op, a, b)?),
    })
}

fn compare(op: BinOp, a: &ConstValue, b: &ConstValue) -> Option<bool> {
    use std::cmp::Ordering;
    let ord = match (a, b) {
        (ConstValue::Int(a), ConstValue::Int(b)) => a.cmp(b),
        (ConstValue::Uint(a), ConstValue::Uint(b)) => a.cmp(b),
        (ConstValue::Bool(a), ConstValue::Bool(b)) => a.cmp(b),
        (ConstValue::Char(a), ConstValue::Char(b)) => a.cmp(b),
        (ConstValue::Float(a), ConstValue::Float(b)) => match a.partial_cmp(b) {
            Some(o) => o,
            None => return Some(op == BinOp::Ne),
        },
        _ => return None,
    };
    Some(match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Ne => ord != Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        BinOp::Ge => ord != Ordering::Less,
        _ => return None,
    })
}

/// An integer as rustc's messages show it: `5_i32`, `i32::MAX`, `i8::MIN`.
fn render(v: &ConstValue, prim: PrimTy) -> String {
    let name = prim.name();
    match *v {
        ConstValue::Int(v) => {
            let (min, max) = sabiko_types::consts::int_bounds(prim);
            if v == min {
                format!("{name}::MIN")
            } else if u128::try_from(v).is_ok_and(|v| v == max) {
                format!("{name}::MAX")
            } else {
                format!("{v}_{name}")
            }
        }
        ConstValue::Uint(v) => {
            let (_, max) = sabiko_types::consts::int_bounds(prim);
            if v == max {
                format!("{name}::MAX")
            } else {
                format!("{v}_{name}")
            }
        }
        ref other => format!("{other:?}"),
    }
}
