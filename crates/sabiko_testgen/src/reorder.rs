//! Semantics-preserving reorderings of generated programs: adjacent statements that are
//! independent of each other are swapped, for the property that the borrow checker's
//! verdict does not depend on the order of independent statements
//! (`crates/sabikoc/tests/generated.rs`).
//!
//! The analysis runs on the program's syntax tree with name resolution and local types
//! (Sabiko's own front end, which every generated program passes). The harness checks
//! each reordered program against rustc as well, so a mistake here shows up as a
//! disagreement instead of being trusted.
//!
//! Two statements of the same block, on consecutive lines, each on a line of its own,
//! are independent when all of the following hold:
//! - neither contains control flow beyond a branch inside an expression (`(if c { a }
//!   else { b })`, `&&`, `||`): no `while` or `loop`, no block or `if` branch holding
//!   statements, no `break`, `continue` or `return`;
//! - they mention disjoint sets of locals (reads count: a by-value read of a non-`Copy`
//!   local is a move, and a reference local's use extends its loans);
//! - at most one of them has an observable effect or may panic: a call (`Print`,
//!   `PrintLine`, `Panic` or a user function, which may print, panic or write through
//!   its `&mut` arguments), arithmetic, a shift, a negation or a compound assignment;
//! - they do not both touch memory that may be aliased, unless both only read it. A
//!   statement touches aliased memory when it borrows (`&` reads, `&mut` writes),
//!   dereferences, mentions a reference local (`&mut` locals count as writes), or mentions
//!   a local whose address is taken anywhere in the function (a write, `&mut` borrow or
//!   move of it counts as a write).
//!
//! The last rule keeps the loans of the program as they were: swapping `*r = 1;` with
//! `let c = a;` when `r` borrows `a` would turn an accepted program into a rejected one
//! (and change its result), although the two statements name different locals.

use std::collections::{BTreeSet, HashSet};

use sabiko_ast::{BinOp, Block, Expr, ExprKind, ItemKind, PatKind, Stmt, StmtKind, UnOp};
use sabiko_diagnostics::DiagnosticSink;
use sabiko_resolve::{LocalId, Res, ResolveResults};
use sabiko_span::{FileId, Span};
use sabiko_types::{BodyTypes, Ty, TypeckResults};

use crate::rng::Rng;

/// A reordered program and the swaps that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reordering {
    pub program: String,
    /// The 1-based line numbers `(n, n + 1)` of every pair that was swapped.
    pub swaps: Vec<(usize, usize)>,
}

/// The 0-based line indices `i` such that lines `i` and `i + 1` hold independent
/// statements of the same block. `Err` when the program does not pass Sabiko's type
/// checker (every generated program does).
pub fn independent_pairs(program: &str) -> Result<Vec<usize>, String> {
    let mut sink = DiagnosticSink::new();
    let ast = sabiko_parser::parse_file(FileId(0), program, &mut sink);
    if sink.has_errors() {
        return Err(format!("parse errors: {:?}", sink.diagnostics()));
    }
    let resolve = sabiko_resolve::resolve(&ast, &mut sink);
    let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
    if sink.has_errors() {
        return Err(format!("front-end errors: {:?}", sink.diagnostics()));
    }
    let lines = Lines::new(program);
    let mut pairs = Vec::new();
    for item in &ast.items {
        let ItemKind::Fn(f) = &item.kind else {
            continue;
        };
        let def = resolve.def_of_item(item.id);
        let mut cx = FnCx {
            resolve: &resolve,
            typeck: &typeck,
            types: typeck.body(def),
            address_taken: HashSet::new(),
            lines: &lines,
            pairs: &mut pairs,
        };
        cx.collect_address_taken_block(&f.body);
        cx.visit_block(&f.body);
    }
    pairs.sort_unstable();
    pairs.dedup();
    Ok(pairs)
}

/// Swaps a random non-empty set of non-overlapping independent pairs of `program`.
/// `Ok(None)` when the program has no independent pair.
pub fn reorder(program: &str, seed: u64) -> Result<Option<Reordering>, String> {
    let pairs = independent_pairs(program)?;
    if pairs.is_empty() {
        return Ok(None);
    }
    let mut rng = Rng::new(seed ^ 0x0DE5_2E0D_E25E);
    let mut chosen: Vec<usize> = Vec::new();
    for &i in &pairs {
        // Pairs are sorted, so only the last chosen one can overlap.
        if chosen.last().is_none_or(|&j| j + 1 < i) && rng.chance(50) {
            chosen.push(i);
        }
    }
    if chosen.is_empty() {
        chosen.push(pairs[rng.index(pairs.len())]);
    }
    let mut lines: Vec<&str> = program.lines().collect();
    for &i in &chosen {
        lines.swap(i, i + 1);
    }
    let mut out = lines.join("\n");
    if program.ends_with('\n') {
        out.push('\n');
    }
    Ok(Some(Reordering {
        program: out,
        swaps: chosen.iter().map(|&i| (i + 1, i + 2)).collect(),
    }))
}

/// Line starts of the program, to find statements that sit alone on their line.
struct Lines<'a> {
    src: &'a str,
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(src: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        Lines { src, starts }
    }

    fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    fn text(&self, line: usize) -> &'a str {
        let start = self.starts[line];
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.src.len(), |&next| next - 1);
        &self.src[start..end]
    }

    /// The line `span` fills on its own (indentation aside), if any.
    fn sole_line(&self, span: Span) -> Option<usize> {
        let (start, end) = (span.start as usize, span.end as usize);
        let line = self.line_of(start);
        let stmt = self.src.get(start..end)?;
        (self.line_of(end.saturating_sub(1)) == line && self.text(line).trim() == stmt)
            .then_some(line)
    }
}

/// How a place is used.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Read,
    Write,
    BorrowShared,
    BorrowMut,
}

/// How a statement touches memory that a reference may alias.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Alias {
    #[default]
    None,
    Read,
    Write,
}

/// What one statement does, for the independence test.
#[derive(Default)]
struct Summary {
    locals: BTreeSet<LocalId>,
    alias: Alias,
    /// Prints, may panic, or calls a function (which may do either).
    effect: bool,
    control: bool,
}

impl Summary {
    fn touch(&mut self, alias: Alias) {
        self.alias = self.alias.max(alias);
    }

    fn independent_of(&self, other: &Summary) -> bool {
        if self.control || other.control || (self.effect && other.effect) {
            return false;
        }
        let conflict = |a: Alias, b: Alias| a == Alias::Write && b != Alias::None;
        self.locals.is_disjoint(&other.locals)
            && !conflict(self.alias, other.alias)
            && !conflict(other.alias, self.alias)
    }
}

struct FnCx<'a, 'p> {
    resolve: &'a ResolveResults,
    typeck: &'a TypeckResults,
    types: &'a BodyTypes,
    /// Locals borrowed anywhere in the function: any access to one may alias a reference.
    address_taken: HashSet<LocalId>,
    lines: &'a Lines<'a>,
    pairs: &'p mut Vec<usize>,
}

impl FnCx<'_, '_> {
    // ----- address-taken locals ----------------------------------------------------

    fn collect_address_taken_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let(l) => {
                    if let Some(init) = &l.init {
                        self.collect_address_taken(init);
                    }
                }
                StmtKind::Expr { expr, .. } => self.collect_address_taken(expr),
                StmtKind::Empty => {}
            }
        }
        if let Some(tail) = &block.tail {
            self.collect_address_taken(tail);
        }
    }

    fn collect_address_taken(&mut self, e: &Expr) {
        if let ExprKind::Ref { expr, .. } = &e.kind {
            if let Some(local) = self.root_local(expr) {
                self.address_taken.insert(local);
            }
        }
        for_each_child(e, |child| match child {
            Child::Expr(x) => self.collect_address_taken(x),
            Child::Block(b) => self.collect_address_taken_block(b),
        });
    }

    /// The local a place expression starts from, through fields and parentheses (and
    /// through a dereference: `&*r` borrows from what `r` points to, which `r`'s own
    /// uses already account for).
    fn root_local(&self, e: &Expr) -> Option<LocalId> {
        match &e.kind {
            ExprKind::Path(_) => match self.resolve.res(e.id) {
                Res::Local(l) => Some(l),
                _ => None,
            },
            ExprKind::Paren(x) | ExprKind::Field(x, _) | ExprKind::Deref(x) => self.root_local(x),
            _ => None,
        }
    }

    // ----- statements --------------------------------------------------------------

    fn visit_block(&mut self, block: &Block) {
        let mut prev: Option<(usize, Summary)> = None;
        for stmt in &block.stmts {
            let summary = self.summarize(stmt);
            let line = self.lines.sole_line(stmt.span);
            if let (Some((prev_line, prev_summary)), Some(line)) = (&prev, line) {
                if line == prev_line + 1 && prev_summary.independent_of(&summary) {
                    self.pairs.push(*prev_line);
                }
            }
            prev = line.map(|l| (l, summary));
            self.visit_nested_blocks(stmt);
        }
        if let Some(tail) = &block.tail {
            self.visit_blocks_in(tail);
        }
    }

    /// Statements inside `if`, `while`, `loop` and block statements are blocks of their
    /// own.
    fn visit_nested_blocks(&mut self, stmt: &Stmt) {
        let expr = match &stmt.kind {
            StmtKind::Let(l) => l.init.as_ref(),
            StmtKind::Expr { expr, .. } => Some(expr),
            StmtKind::Empty => None,
        };
        if let Some(e) = expr {
            self.visit_blocks_in(e);
        }
    }

    fn visit_blocks_in(&mut self, e: &Expr) {
        for_each_child(e, |child| match child {
            Child::Expr(x) => self.visit_blocks_in(x),
            Child::Block(b) => self.visit_block(b),
        });
    }

    fn summarize(&self, stmt: &Stmt) -> Summary {
        let mut s = Summary::default();
        match &stmt.kind {
            StmtKind::Let(l) => {
                if let PatKind::Ident { .. } = l.pat.kind {
                    if let Some(local) = self.resolve.local_of_pat(l.pat.id) {
                        self.local(local, Mode::Write, &mut s);
                    }
                }
                if let Some(init) = &l.init {
                    self.expr(init, Mode::Read, &mut s);
                }
            }
            StmtKind::Expr { expr, .. } => self.expr(expr, Mode::Read, &mut s),
            StmtKind::Empty => {}
        }
        s
    }

    fn local(&self, local: LocalId, mode: Mode, s: &mut Summary) {
        s.locals.insert(local);
        let ty = self.types.local_ty(local);
        if let Ty::Ref(m, _) = ty {
            s.touch(if m.is_mut() {
                Alias::Write
            } else {
                Alias::Read
            });
        }
        if self.address_taken.contains(&local) {
            let copy = self.typeck.items.is_copy(ty);
            s.touch(match mode {
                Mode::BorrowShared => Alias::Read,
                Mode::Read if copy => Alias::Read,
                // A write, a `&mut` borrow, a move, or a read of a field of a non-`Copy`
                // struct (maybe a move).
                Mode::Write | Mode::BorrowMut | Mode::Read => Alias::Write,
            });
        }
    }

    fn expr(&self, e: &Expr, mode: Mode, s: &mut Summary) {
        match &e.kind {
            ExprKind::Lit(_) | ExprKind::Unit => {}
            ExprKind::Path(_) => {
                if let Res::Local(local) = self.resolve.res(e.id) {
                    self.local(local, mode, s);
                }
            }
            ExprKind::Paren(x) | ExprKind::Field(x, _) => self.expr(x, mode, s),
            ExprKind::Unary(op, x) => {
                if *op == UnOp::Neg {
                    s.effect = true;
                }
                self.expr(x, Mode::Read, s);
            }
            ExprKind::Binary(op, a, b) => {
                if may_panic(*op) {
                    s.effect = true;
                }
                self.expr(a, Mode::Read, s);
                self.expr(b, Mode::Read, s);
            }
            ExprKind::Assign(lhs, rhs) => {
                self.expr(lhs, Mode::Write, s);
                self.expr(rhs, Mode::Read, s);
            }
            ExprKind::AssignOp(_, lhs, rhs) => {
                s.effect = true;
                self.expr(lhs, Mode::Write, s);
                self.expr(rhs, Mode::Read, s);
            }
            ExprKind::Call { callee, args } => {
                s.effect = true;
                self.expr(callee, Mode::Read, s);
                for a in args {
                    self.expr(a, Mode::Read, s);
                }
            }
            ExprKind::Ref { mutable, expr } => {
                if *mutable {
                    s.touch(Alias::Write);
                    self.expr(expr, Mode::BorrowMut, s);
                } else {
                    s.touch(Alias::Read);
                    self.expr(expr, Mode::BorrowShared, s);
                }
            }
            ExprKind::Deref(x) => {
                s.touch(match mode {
                    Mode::Write | Mode::BorrowMut => Alias::Write,
                    Mode::Read | Mode::BorrowShared => Alias::Read,
                });
                self.expr(x, Mode::Read, s);
            }
            ExprKind::Cast(x, _) => self.expr(x, Mode::Read, s),
            ExprKind::Struct { fields, .. } => {
                for f in fields {
                    self.expr(&f.value, Mode::Read, s);
                }
            }
            // A branch inside an expression stays inside the statement; its blocks may
            // only hold a value.
            ExprKind::If { cond, then, els } => {
                self.expr(cond, Mode::Read, s);
                self.value_block(then, s);
                if let Some(els) = els {
                    self.expr(els, Mode::Read, s);
                }
            }
            ExprKind::Block(b) => self.value_block(b, s),
            ExprKind::While { .. }
            | ExprKind::Loop { .. }
            | ExprKind::Break(_)
            | ExprKind::Continue
            | ExprKind::Return(_)
            | ExprKind::MethodCall { .. }
            | ExprKind::Index(..) => s.control = true,
        }
    }

    fn value_block(&self, b: &Block, s: &mut Summary) {
        if !b.stmts.is_empty() {
            s.control = true;
        }
        if let Some(tail) = &b.tail {
            self.expr(tail, Mode::Read, s);
        }
    }
}

/// Operators that can panic at run time (overflow, division by zero, over-wide shifts).
fn may_panic(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Shl | BinOp::Shr
    )
}

enum Child<'e> {
    Expr(&'e Expr),
    Block(&'e Block),
}

/// Calls `f` on the direct sub-expressions and blocks of `e`.
fn for_each_child<'e>(e: &'e Expr, mut f: impl FnMut(Child<'e>)) {
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Path(_) | ExprKind::Unit | ExprKind::Continue => {}
        ExprKind::Paren(x)
        | ExprKind::Unary(_, x)
        | ExprKind::Field(x, _)
        | ExprKind::Ref { expr: x, .. }
        | ExprKind::Deref(x)
        | ExprKind::Cast(x, _) => f(Child::Expr(x)),
        ExprKind::Binary(_, a, b)
        | ExprKind::Assign(a, b)
        | ExprKind::AssignOp(_, a, b)
        | ExprKind::Index(a, b) => {
            f(Child::Expr(a));
            f(Child::Expr(b));
        }
        ExprKind::Call {
            callee: first,
            args,
        }
        | ExprKind::MethodCall {
            receiver: first,
            args,
            ..
        } => {
            f(Child::Expr(first));
            for a in args {
                f(Child::Expr(a));
            }
        }
        ExprKind::Block(b) | ExprKind::Loop { body: b } => f(Child::Block(b)),
        ExprKind::If { cond, then, els } => {
            f(Child::Expr(cond));
            f(Child::Block(then));
            if let Some(els) = els {
                f(Child::Expr(els));
            }
        }
        ExprKind::While { cond, body } => {
            f(Child::Expr(cond));
            f(Child::Block(body));
        }
        ExprKind::Break(x) | ExprKind::Return(x) => {
            if let Some(x) = x {
                f(Child::Expr(x));
            }
        }
        ExprKind::Struct { fields, .. } => {
            for field in fields {
                f(Child::Expr(&field.value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(src: &str) -> Vec<usize> {
        independent_pairs(src).unwrap()
    }

    #[test]
    fn disjoint_plain_statements_swap() {
        let src = "fn Main() {\n    let a = 1i32;\n    let b = 2i32;\n    PrintLine(a);\n}\n";
        // `let a` / `let b`: disjoint; `let b` / `PrintLine(a)`: disjoint, one effect.
        assert_eq!(pairs(src), vec![1, 2]);
        let r = reorder(src, 3).unwrap().unwrap();
        assert!(!r.swaps.is_empty());
        assert_ne!(r.program, src);
    }

    #[test]
    fn shared_locals_and_two_effects_do_not_swap() {
        let src = "fn Main() {\n    let a = 1i32;\n    let b = a;\n    PrintLine(b);\n    PrintLine(7i32);\n    let c = (b + 1i32);\n}\n";
        // a/b share `a`, b/Print share `b`, two prints, print/arith both effects.
        assert_eq!(pairs(src), Vec::<usize>::new());
    }

    #[test]
    fn aliasing_through_a_reference_blocks_the_swap() {
        // `*r = 5;` writes `a` through `r` and `let d = a;` reads `a`: they name different
        // locals but must not swap (the loan would then be live at the read).
        let src = "fn Main() {\n    let mut a = 1i32;\n    let b = 2i32;\n    let r = &mut a;\n    *r = 5i32;\n    let d = a;\n    let c = b;\n    PrintLine(d);\n}\n";
        assert_eq!(pairs(src), vec![1, 2, 5, 6]);
    }

    #[test]
    fn reads_of_a_shared_borrow_may_swap_but_writes_may_not() {
        // `let x = *r;` / `let y = a;` both only read the borrowed `a`.
        let src = "fn Main() {\n    let mut a = 1i32;\n    let r = &a;\n    let x = *r;\n    let y = a;\n    a = 3i32;\n    PrintLine(x);\n}\n";
        assert_eq!(pairs(src), vec![3, 5]);
        let src = "fn Main() {\n    let mut a = 1i32;\n    let r = &a;\n    let x = *r;\n    a = 3i32;\n    PrintLine(x);\n}\n";
        assert_eq!(pairs(src), vec![4]);
    }

    #[test]
    fn moves_of_borrowed_locals_count_as_writes() {
        let src = "struct S {\n    A: i32,\n}\n\nfn Main() {\n    let s = S { A: 1i32 };\n    let r = &s;\n    let x = (*r).A;\n    let t = s;\n}\n";
        // `let x = (*r).A` reads through `r`; `let t = s` moves the borrowed `s`.
        assert_eq!(pairs(src), Vec::<usize>::new());
    }

    #[test]
    fn control_flow_and_multi_line_statements_never_swap() {
        let src = "fn Main() {\n    let a = 1i32;\n    if true { return; }\n    let b = 2i32;\n    if a > 0i32 {\n        PrintLine(a);\n    }\n    let c = 3i32;\n}\n";
        assert_eq!(pairs(src), Vec::<usize>::new());
    }

    #[test]
    fn statements_inside_nested_blocks_swap() {
        let src =
            "fn Main() {\n    if true {\n        let a = 1i32;\n        let b = 2i32;\n    }\n}\n";
        assert_eq!(pairs(src), vec![2]);
    }
}
