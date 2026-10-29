//! Lowering of one MIR body to an IL method.
//!
//! Every MIR statement and terminator is lowered to a short instruction sequence. Places
//! are accessed through their address: `ldloca`/`ldarga` for locals and arguments,
//! `ldflda` for fields (through `Rest` for the elements of a `System.ValueTuple` past the
//! seventh), the pointer value itself for `*r`. Locals of type `()` or `!` get no storage:
//! assignments to them vanish and reads push nothing. A `()` value is stateless, so no
//! `()` is ever loaded or stored, even where it has storage (a parameter, a struct field, a
//! tuple element); where the IL needs one (a call argument, a tuple constructor argument)
//! a default `Sabiko.Runtime.Unit` is pushed instead (decision 52).
//!
//! Single-use locals stay on the evaluation stack (decision 51): a local assigned once
//! and read once (a temporary, usually) is not stored when it is assigned; its value
//! stays on the stack, and when the statement that reads it starts by loading it, the
//! load is dropped and the value is used where it already is. Each such definition is an
//! [`Instr::Def`] placeholder, decided once the method is lowered: nothing (the value was
//! used in place), `stloc` (the reader did not start with it, so it is stored after all,
//! exactly as without the optimisation) or `dup; stloc` (the reader loaded it a second
//! time). Values on the stack are taken in last-in first-out order: `a * b + c * d` keeps
//! both products on the stack for the addition. The stack is otherwise empty between
//! statements, and at a block label unless the block is entered only by falling through
//! from its single predecessor (a call's continuation, say).

use std::fmt::Write as _;

use sabiko_diagnostics::ErrorCode;
use sabiko_mir::{
    AggregateKind, BasicBlock, BinOp, Body, ConstValue, Constant, Local, LocalKind, Operand, Place,
    PlaceElem, Rvalue, Statement, StatementKind, Terminator, TerminatorKind, Ty, UnOp,
};
use sabiko_resolve::{Builtin, DefId, PrimTy};
use sabiko_span::Span;

use crate::types::{self, StackWidth, IS_READONLY_ATTR, STR_SPAN};
use crate::{Cx, RUNTIME_ASSEMBLY};

/// Where a MIR local lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Arg(u16),
    Local(u16),
    /// `()` and `!`: no storage.
    None,
}

pub(crate) fn emit_body(cx: &mut Cx<'_>, body: &Body, out: &mut String) {
    let mut f = FnCx {
        cx,
        body,
        slots: Vec::new(),
        locals: Vec::new(),
        code: Vec::new(),
        depth: 0,
        next_label: 0,
        scratch: Vec::new(),
        layout: Layout::new(body),
        stackable: stackable_locals(body),
        defs: Vec::new(),
        pending: Vec::new(),
        consumed: Vec::new(),
        matching: None,
        stmt_start: 0,
        def_returns: false,
    };
    f.assign_slots();
    for i in 0..f.layout.order.len() {
        let bb = f.layout.order[i];
        if f.layout.emitted(bb) {
            f.block(bb);
        }
    }
    f.write_method(out);
}

/// The order blocks are emitted in, and which blocks need a label of their own.
///
/// Blocks are laid out in reverse postorder of a depth-first walk from `bb0`, so every
/// branch goes forward except the back edges of loops: the JIT treats a method with a
/// backward branch as a loop, which sends it to instrumented Tier0 instead of plain Tier0.
/// The walk visits a block's successors in terminator order (a `switchInt`'s `otherwise`
/// last), which places the last unvisited successor right after the block, where it is
/// reached by falling through. A jump to a block that only returns is emitted as the
/// return itself, so such a block is emitted only when a `switchInt` branches to it.
struct Layout {
    order: Vec<BasicBlock>,
    /// The block emitted after each block (indexed by block), if any.
    next: Vec<Option<BasicBlock>>,
    /// Blocks with no statements of their own that end in `return`.
    return_only: Vec<bool>,
    /// Blocks that some `switchInt` branches to.
    switch_target: Vec<bool>,
    /// The number of control-flow edges into each block.
    preds: Vec<usize>,
}

impl Layout {
    fn new(body: &Body) -> Self {
        let n = body.blocks.len();
        let mut visited = vec![false; n];
        let mut post = Vec::with_capacity(n);
        let mut stack: Vec<(BasicBlock, Vec<BasicBlock>, usize)> = Vec::new();
        if n > 0 {
            visited[0] = true;
            stack.push((
                BasicBlock::START,
                Self::successors(body, BasicBlock::START),
                0,
            ));
        }
        while let Some((bb, succs, i)) = stack.last_mut() {
            if let Some(&s) = succs.get(*i) {
                *i += 1;
                if !visited[s.index()] {
                    visited[s.index()] = true;
                    let next = Self::successors(body, s);
                    stack.push((s, next, 0));
                }
            } else {
                post.push(*bb);
                stack.pop();
            }
        }
        let mut order: Vec<BasicBlock> = post.into_iter().rev().collect();
        // Simplification leaves no unreachable blocks; keep any anyway, at the end.
        order.extend(body.block_ids().filter(|bb| !visited[bb.index()]));
        let return_only: Vec<bool> = body
            .blocks
            .iter()
            .map(|data| {
                matches!(data.terminator().kind, TerminatorKind::Return)
                    && data.statements.iter().all(|s| {
                        matches!(
                            s.kind,
                            StatementKind::StorageLive(_)
                                | StatementKind::StorageDead(_)
                                | StatementKind::PlaceMention(_)
                                | StatementKind::Nop
                        )
                    })
            })
            .collect();
        let mut switch_target = vec![false; n];
        let mut preds = vec![0; n];
        for data in &body.blocks {
            for s in data.terminator().kind.successors() {
                preds[s.index()] += 1;
            }
            if let TerminatorKind::SwitchInt { .. } = &data.terminator().kind {
                for s in data.terminator().kind.successors() {
                    switch_target[s.index()] = true;
                }
            }
        }
        let mut layout = Self {
            order: Vec::new(),
            next: vec![None; n],
            return_only,
            switch_target,
            preds,
        };
        // Blocks that are never emitted do not separate their neighbours.
        let emitted: Vec<BasicBlock> = order.into_iter().filter(|&bb| layout.emitted(bb)).collect();
        for pair in emitted.windows(2) {
            layout.next[pair[0].index()] = Some(pair[1]);
        }
        layout.order = emitted;
        layout
    }

    fn successors(body: &Body, bb: BasicBlock) -> Vec<BasicBlock> {
        body.block(bb).terminator().kind.successors()
    }

    /// Whether `bb` gets code of its own: a block that only returns is inlined into the
    /// jumps to it, unless it is the entry or a `switchInt` target.
    fn emitted(&self, bb: BasicBlock) -> bool {
        !self.return_only[bb.index()] || bb == BasicBlock::START || self.switch_target[bb.index()]
    }

    fn is_next(&self, from: BasicBlock, to: BasicBlock) -> bool {
        self.next[from.index()] == Some(to)
    }
}

struct FnCx<'a, 'b> {
    cx: &'a mut Cx<'b>,
    body: &'a Body,
    slots: Vec<Slot>,
    /// IL locals (CIL type and name), numbered in `.locals init` once the unused ones are
    /// known.
    locals: Vec<(String, String)>,
    code: Vec<Instr>,
    /// The evaluation stack's depth while lowering (values kept on the stack included).
    depth: usize,
    next_label: u32,
    /// Compiler-introduced IL locals that stage struct literals, one per struct type.
    scratch: Vec<(Ty, u16)>,
    layout: Layout,
    /// MIR locals assigned once and read once, by a whole-local operand: their value may
    /// stay on the stack.
    stackable: Vec<bool>,
    /// Every definition kept on the stack, and what became of it.
    defs: Vec<Def>,
    /// Values on the evaluation stack that are still waiting for their reader, bottom to
    /// top; nothing else is under the current statement's own values.
    pending: Vec<Pending>,
    /// For each IL local, the definition whose value its reader took from the stack: a
    /// second load of the local must find it stored.
    consumed: Vec<Option<usize>>,
    /// While the current statement is taking pending values in order: the index in
    /// `pending` of the first one taken and of the one expected next.
    matching: Option<(usize, usize)>,
    /// `code.len()` when the current statement (or terminator) started; `usize::MAX` once
    /// it has taken values from the stack up to the top, after which it may take no more.
    stmt_start: usize,
    /// Whether an assignment to the return place in the current statement is followed only
    /// by the return, which reads it at once.
    def_returns: bool,
}

/// One emitted instruction, or a placeholder resolved when the method is written.
#[derive(Debug)]
enum Instr {
    /// An instruction and its stack effect.
    Op {
        text: String,
        pops: usize,
        pushes: usize,
    },
    Label(String),
    /// `ldloc`, `stloc` or `ldloca` of an IL local, numbered when the method is written.
    Local(LocalOp, u16),
    /// The definition `defs[i]` of a stackable local.
    Def(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalOp {
    Load,
    Store,
    Address,
}

/// A stackable local's definition: its IL local and what its placeholder becomes.
#[derive(Clone, Copy, Debug)]
struct Def {
    local: u16,
    mode: DefMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DefMode {
    /// The value stays on the stack for its reader: nothing is emitted.
    Stack,
    /// `stloc`: the reader loads it from the local.
    Spill,
    /// `dup; stloc`: the reader takes it from the stack and loads it again later.
    DupSpill,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    local: u16,
    def: usize,
}

/// MIR locals whose value may stay on the evaluation stack: user locals and temporaries
/// with storage, assigned exactly once as a whole (not by a struct literal, which is built
/// in place; a tuple is built on the stack by its constructor) and read exactly once by a
/// whole-local operand, never borrowed or projected.
fn stackable_locals(body: &Body) -> Vec<bool> {
    let n = body.locals.len();
    let (mut defs, mut uses, mut other) = (vec![0usize; n], vec![0usize; n], vec![0usize; n]);
    let operand = |op: &Operand, uses: &mut Vec<usize>, other: &mut Vec<usize>| {
        if let Some(p) = op.place() {
            if p.is_local() {
                uses[p.local.index()] += 1;
            } else {
                other[p.local.index()] += 1;
            }
        }
    };
    for data in &body.blocks {
        for stmt in &data.statements {
            let StatementKind::Assign(place, rvalue) = &stmt.kind else {
                continue;
            };
            if place.is_local() && !matches!(rvalue, Rvalue::Aggregate(AggregateKind::Adt(_), _)) {
                defs[place.local.index()] += 1;
            } else {
                other[place.local.index()] += 1;
            }
            match rvalue {
                Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => {
                    operand(op, &mut uses, &mut other);
                }
                Rvalue::BinaryOp(_, l, r) => {
                    operand(l, &mut uses, &mut other);
                    operand(r, &mut uses, &mut other);
                }
                Rvalue::Aggregate(_, ops) => {
                    for op in ops {
                        operand(op, &mut uses, &mut other);
                    }
                }
                Rvalue::Ref(_, p) => other[p.local.index()] += 1,
            }
        }
        match &data.terminator().kind {
            TerminatorKind::SwitchInt { discr, .. } => operand(discr, &mut uses, &mut other),
            TerminatorKind::Call {
                args, destination, ..
            } => {
                for a in args {
                    operand(a, &mut uses, &mut other);
                }
                if destination.is_local() {
                    defs[destination.local.index()] += 1;
                } else {
                    other[destination.local.index()] += 1;
                }
            }
            TerminatorKind::Goto { .. } | TerminatorKind::Return | TerminatorKind::Unreachable => {}
        }
    }
    (0..n)
        .map(|i| {
            let decl = &body.locals[i];
            matches!(decl.kind, LocalKind::User | LocalKind::Temp)
                && !(decl.ty.is_unit() || decl.ty.is_uninhabited())
                && defs[i] == 1
                && uses[i] == 1
                && other[i] == 0
        })
        .collect()
}

impl FnCx<'_, '_> {
    // ----------------------------------------------------------------------------------
    // Setup and output
    // ----------------------------------------------------------------------------------

    fn assign_slots(&mut self) {
        let body = self.body;
        let mut name_counts = std::collections::HashMap::new();
        for decl in &body.locals {
            if let Some(n) = &decl.name {
                *name_counts.entry(n.clone()).or_insert(0usize) += 1;
            }
        }
        for (i, decl) in body.locals.iter().enumerate() {
            // `()`, `!` and `&!` (decisions 32 and 44) have no values to store.
            let elided = decl.ty.is_unit() || decl.ty.is_uninhabited();
            let slot = match decl.kind {
                // A `()` parameter is a `Sabiko.Runtime.Unit` (decision 52); nothing reads it.
                LocalKind::Arg => {
                    Slot::Arg(u16::try_from(i - 1).expect("fewer than 2^16 parameters"))
                }
                _ if elided => Slot::None,
                LocalKind::Return | LocalKind::User | LocalKind::Temp => {
                    let name = match &decl.name {
                        Some(n) if name_counts[n] == 1 => n.clone(),
                        _ => format!("_{i}"),
                    };
                    self.new_local(&decl.ty, &name, decl.span)
                }
            };
            self.slots.push(slot);
        }
    }

    fn new_local(&mut self, ty: &Ty, name: &str, span: Span) -> Slot {
        let index = u16::try_from(self.locals.len()).expect("fewer than 2^16 locals");
        let cil = types::cil_type(self.cx, ty, span);
        self.locals.push((cil, types::quote(name)));
        self.consumed.push(None);
        Slot::Local(index)
    }

    /// A staging local for struct literals of type `ty`, shared by all of them.
    fn scratch(&mut self, ty: &Ty, span: Span) -> u16 {
        if let Some((_, slot)) = self.scratch.iter().find(|(t, _)| t == ty) {
            return *slot;
        }
        let name = format!("$tmp{}", self.scratch.len());
        let Slot::Local(slot) = self.new_local(ty, &name, span) else {
            unreachable!()
        };
        self.scratch.push((ty.clone(), slot));
        slot
    }

    fn write_method(&mut self, out: &mut String) {
        let body = self.body;
        let ret = types::ret_type(self.cx, body.return_ty(), body.span);
        let mut params = Vec::new();
        let mut readonly_params = Vec::new();
        if Cx::is_byref(body.return_ty()) && !is_mut_ref(body.return_ty()) {
            readonly_params.push(0);
        }
        for (i, local) in body.args().enumerate() {
            let decl = body.local_decl(local);
            let ty = types::param_type(self.cx, &decl.ty, decl.span);
            let name = decl.name.clone().unwrap_or_else(|| format!("_{}", local.0));
            if Cx::is_byref(&decl.ty) && !is_mut_ref(&decl.ty) {
                readonly_params.push(i + 1);
                params.push(format!("[in] {ty} {}", types::quote(&name)));
            } else {
                params.push(format!("{ty} {}", types::quote(&name)));
            }
        }
        let _ = writeln!(
            out,
            "  .method public hidebysig static {ret} {}({}) cil managed\n  {{",
            types::quote(&body.name),
            params.join(", ")
        );
        for p in readonly_params {
            let _ = writeln!(out, "    .param [{p}]\n    {IS_READONLY_ATTR}");
        }
        // Locals nothing loads, stores or borrows (single-use values that stayed on the
        // stack) are left out; the others are numbered in order.
        let mut used = vec![false; self.locals.len()];
        for instr in &self.code {
            match instr {
                Instr::Local(_, i) => used[usize::from(*i)] = true,
                Instr::Def(d) if self.defs[*d].mode != DefMode::Stack => {
                    used[usize::from(self.defs[*d].local)] = true;
                }
                _ => {}
            }
        }
        let mut index = vec![0u16; self.locals.len()];
        let mut next = 0u16;
        for (i, u) in used.iter().enumerate() {
            if *u {
                index[i] = next;
                next += 1;
            }
        }
        let _ = writeln!(out, "    .maxstack {}", self.max_stack().max(1));
        if next > 0 {
            out.push_str("    .locals init (\n");
            for (i, (cil, name)) in self.locals.iter().enumerate() {
                if !used[i] {
                    continue;
                }
                let sep = if index[i] + 1 == next { "" } else { "," };
                let _ = writeln!(out, "      [{}] {cil} {name}{sep}", index[i]);
            }
            out.push_str("    )\n");
        }
        for instr in &self.code {
            match instr {
                Instr::Op { text, .. } => {
                    let _ = writeln!(out, "    {text}");
                }
                Instr::Label(name) => {
                    let _ = writeln!(out, "  {name}:");
                }
                Instr::Local(op, i) => {
                    let _ = writeln!(out, "    {}", local_instr(*op, index[usize::from(*i)]));
                }
                Instr::Def(d) => {
                    let def = self.defs[*d];
                    let store = local_instr(LocalOp::Store, index[usize::from(def.local)]);
                    match def.mode {
                        DefMode::Stack => {}
                        DefMode::Spill => {
                            let _ = writeln!(out, "    {store}");
                        }
                        DefMode::DupSpill => {
                            let _ = writeln!(out, "    dup\n    {store}");
                        }
                    }
                }
            }
        }
        out.push_str("  }\n\n");
    }

    /// `.maxstack`: the deepest the stack gets in the final instruction stream. Every
    /// label is reached with the depth the instructions before it leave (a `throw` pops
    /// only its exception, as if control went on past it), so one linear pass is exact.
    fn max_stack(&self) -> usize {
        let mut depth = 0usize;
        let mut max = 0usize;
        let mut step = |pops: usize, pushes: usize| {
            debug_assert!(depth >= pops, "stack underflow in the final stream");
            depth = depth.saturating_sub(pops) + pushes;
            max = max.max(depth);
        };
        for instr in &self.code {
            match instr {
                Instr::Op { pops, pushes, .. } => step(*pops, *pushes),
                Instr::Label(_) => {}
                Instr::Local(LocalOp::Load | LocalOp::Address, _) => step(0, 1),
                Instr::Local(LocalOp::Store, _) => step(1, 0),
                Instr::Def(d) => match self.defs[*d].mode {
                    DefMode::Stack => {}
                    DefMode::Spill => step(1, 0),
                    DefMode::DupSpill => {
                        step(1, 2);
                        step(1, 0);
                    }
                },
            }
        }
        max
    }

    // ----------------------------------------------------------------------------------
    // Instruction stream
    // ----------------------------------------------------------------------------------

    /// Emits one instruction with its stack effect.
    fn op(&mut self, text: impl Into<String>, pops: usize, pushes: usize) {
        self.settle();
        debug_assert!(
            self.depth >= pops,
            "stack underflow lowering {}",
            self.body.name
        );
        self.depth = self.depth.saturating_sub(pops) + pushes;
        self.code.push(Instr::Op {
            text: text.into(),
            pops,
            pushes,
        });
    }

    fn label(&mut self, name: &str) {
        self.settle();
        self.code.push(Instr::Label(name.to_owned()));
    }

    // ----------------------------------------------------------------------------------
    // Values kept on the stack
    // ----------------------------------------------------------------------------------

    /// Stores pending value `pending[at]` after all: its placeholder becomes `stloc`.
    fn spill(&mut self, at: usize) {
        let p = self.pending.remove(at);
        self.defs[p.def].mode = DefMode::Spill;
        self.depth -= 1;
    }

    /// Called before anything is emitted: a statement that took some pending values in
    /// order and then does something else did not take the ones above them, which are
    /// stored instead.
    fn settle(&mut self) {
        if let Some((start, next)) = self.matching.take() {
            while self.pending.len() > next {
                self.spill(next);
            }
            self.pending.truncate(start);
        }
    }

    /// Stores every value still waiting on the stack (at a block end, or before control
    /// leaves the method).
    fn spill_all(&mut self) {
        self.settle();
        while !self.pending.is_empty() {
            self.spill(0);
        }
    }

    /// Defines IL local `i` with the value on top of the stack, which may stay there.
    fn define(&mut self, i: u16) {
        self.settle();
        let def = self.defs.len();
        self.defs.push(Def {
            local: i,
            mode: DefMode::Stack,
        });
        self.code.push(Instr::Def(def));
        self.pending.push(Pending { local: i, def });
        self.consumed[usize::from(i)] = None;
    }

    /// Whether a whole-local assignment to `local` may leave its value on the stack.
    fn stackable_def(&self, local: Local) -> bool {
        self.stackable[local.index()] || (local == Local::RETURN && self.def_returns)
    }

    fn fresh_label(&mut self) -> String {
        self.next_label += 1;
        format!("L{}", self.next_label)
    }

    fn ldc_i4(&mut self, v: i32) {
        let text = match v {
            -1 => "ldc.i4.m1".to_owned(),
            0..=8 => format!("ldc.i4.{v}"),
            -128..=127 => format!("ldc.i4.s {v}"),
            _ => format!("ldc.i4 {v}"),
        };
        self.op(text, 0, 1);
    }

    fn ldc_i8(&mut self, v: i64) {
        self.op(format!("ldc.i8 {v}"), 0, 1);
    }

    /// Pushes an integer constant as a value of the integer type `p`.
    fn int_const(&mut self, bits: u128, p: PrimTy) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let low32 = bits as u32 as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let low64 = bits as u64 as i64;
        match types::stack_width(p) {
            StackWidth::Narrow | StackWidth::I32 => self.ldc_i4(low32),
            StackWidth::I64 => self.ldc_i8(low64),
            StackWidth::Native => {
                let signed = p.is_signed_int();
                let fits = if signed {
                    i32::try_from(low64).is_ok()
                } else {
                    u32::try_from(low64).is_ok() && low32 >= 0
                };
                if fits {
                    self.ldc_i4(low32);
                } else {
                    self.ldc_i8(low64);
                }
                self.op(if signed { "conv.i" } else { "conv.u" }, 1, 1);
            }
            StackWidth::Wide => {
                // `new Int128(upper, lower)`: the two 64-bit halves of the bit pattern.
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let upper = (bits >> 64) as u64 as i64;
                self.ldc_i8(upper);
                self.ldc_i8(low64);
                self.op(
                    format!(
                        "newobj instance void {}::.ctor(uint64, uint64)",
                        types::wide_class(p)
                    ),
                    2,
                    1,
                );
            }
        }
    }

    /// Calls the static method `name` of the 128-bit integer type `p` (an operator such as
    /// `op_Addition`), which takes `params` and returns `ret`.
    fn wide_call(&mut self, p: PrimTy, ret: &str, name: &str, params: &[&str]) {
        let class = types::wide_class(p);
        self.op(
            format!("call {ret} {class}::{name}({})", params.join(", ")),
            params.len(),
            1,
        );
    }

    /// A binary operator method of the 128-bit type `p` (`T op(T, T)`).
    fn wide_binary(&mut self, p: PrimTy, name: &str) {
        let ty = types::prim_type(p).expect("128-bit types are lowered");
        self.wide_call(p, ty, name, &[ty, ty]);
    }

    /// A comparison operator method of the 128-bit type `p` (`bool op(T, T)`).
    fn wide_compare(&mut self, p: PrimTy, name: &str) {
        let ty = types::prim_type(p).expect("128-bit types are lowered");
        self.wide_call(p, "bool", name, &[ty, ty]);
    }

    /// Pops two values of the integer type `p` and branches to `label` when they differ.
    fn branch_ne(&mut self, p: PrimTy, label: &str) {
        if types::stack_width(p) == StackWidth::Wide {
            self.wide_compare(p, "op_Inequality");
            self.op(format!("brtrue {label}"), 1, 0);
        } else {
            self.op(format!("bne.un {label}"), 2, 0);
        }
    }

    /// Pushes zero of the integer type `p`.
    fn zero(&mut self, p: PrimTy) {
        self.int_const(0, p);
    }

    /// Code control cannot reach (after a call that never returns, MIR's `unreachable`):
    /// panics with Rust's message, without a location.
    fn unreachable(&mut self) {
        self.op(
            format!("call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Checks::Unreachable()"),
            0,
            0,
        );
        self.throw_after_helper();
    }

    // ----------------------------------------------------------------------------------
    // Blocks
    // ----------------------------------------------------------------------------------

    fn block(&mut self, bb: BasicBlock) {
        self.label(&format!("BB{}", bb.0));
        let data = self.body.block(bb);
        let term = data.terminator();
        let term_returns = match &term.kind {
            TerminatorKind::Return => true,
            TerminatorKind::Goto { target } => self.layout.return_only[target.index()],
            _ => false,
        };
        for (k, stmt) in data.statements.iter().enumerate() {
            // `_0 = ...` followed by nothing but the return: the return reads it at once.
            self.def_returns = term_returns
                && data.statements[k + 1..].iter().all(|s| {
                    matches!(
                        s.kind,
                        StatementKind::StorageLive(_)
                            | StatementKind::StorageDead(_)
                            | StatementKind::PlaceMention(_)
                            | StatementKind::Nop
                    )
                });
            self.stmt_start = self.code.len();
            self.stmt(stmt);
            self.settle();
            debug_assert_eq!(
                self.depth,
                self.pending.len(),
                "statement left values on the stack"
            );
        }
        self.def_returns = match &term.kind {
            TerminatorKind::Call {
                target: Some(target),
                ..
            } => self.layout.return_only[target.index()],
            _ => false,
        };
        self.stmt_start = self.code.len();
        self.terminator(term, bb);
        self.settle();
        // Values may stay on the stack into the next block only when it is entered by
        // falling through from this one alone.
        let carry = match &term.kind {
            TerminatorKind::Goto { target }
            | TerminatorKind::Call {
                target: Some(target),
                ..
            } => {
                self.layout.is_next(bb, *target)
                    && self.layout.preds[target.index()] == 1
                    && !self.layout.return_only[target.index()]
            }
            _ => false,
        };
        if !carry {
            self.spill_all();
        }
        debug_assert_eq!(
            self.depth,
            self.pending.len(),
            "terminator left values on the stack"
        );
    }

    fn stmt(&mut self, stmt: &Statement) {
        match &stmt.kind {
            StatementKind::Assign(place, rvalue) => self.assign(place, rvalue, stmt.span),
            // A mention reads nothing (`let _ = place;`).
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::PlaceMention(_)
            | StatementKind::Nop => {}
        }
    }

    fn assign(&mut self, place: &Place, rvalue: &Rvalue, span: Span) {
        let ty = place.ty(self.body);
        if ty.is_unit() || ty.is_uninhabited() {
            // Rvalues have no side effects; a unit store is nothing.
            return;
        }
        if let Rvalue::Aggregate(AggregateKind::Adt(def), operands) = rvalue {
            self.aggregate(place, *def, operands, span);
            return;
        }
        self.store(place, span, |this| this.rvalue(rvalue, &ty, span));
    }

    /// Branches to `target` unless it is the block emitted next; a jump to a block that
    /// only returns is the return itself.
    fn jump(&mut self, target: BasicBlock, from: BasicBlock) {
        if self.layout.return_only[target.index()] {
            self.ret();
        } else if !self.layout.is_next(from, target) {
            self.op(format!("br BB{}", target.0), 0, 0);
        }
    }

    /// Loads the return place, if it has storage, and returns.
    fn ret(&mut self) {
        if let Slot::Local(i) = self.slots[Local::RETURN.index()] {
            self.ldloc(i);
            // Control leaves: a later load of the return place is on another path.
            self.consumed[usize::from(i)] = None;
            self.op("ret", 1, 0);
        } else {
            self.op("ret", 0, 0);
        }
    }

    fn terminator(&mut self, term: &Terminator, bb: BasicBlock) {
        match &term.kind {
            TerminatorKind::Goto { target } => self.jump(*target, bb),
            TerminatorKind::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                let discr_ty = discr.ty(self.body);
                if let [(0, zero_target)] = targets[..] {
                    self.operand(discr, term.span);
                    if self.layout.is_next(bb, zero_target) && !self.layout.is_next(bb, *otherwise)
                    {
                        // Fall through to the zero target instead of jumping to it.
                        self.op(format!("brtrue BB{}", otherwise.0), 1, 0);
                        return;
                    }
                    self.op(format!("brfalse BB{}", zero_target.0), 1, 0);
                } else {
                    for (value, target) in targets {
                        self.operand(discr, term.span);
                        match discr_ty.as_prim() {
                            Some(p) if types::stack_width(p) == StackWidth::Wide => {
                                self.int_const(*value, p);
                                self.wide_compare(p, "op_Equality");
                                self.op(format!("brtrue BB{}", target.0), 1, 0);
                                continue;
                            }
                            Some(p) if p.is_integer() => self.int_const(*value, p),
                            _ => {
                                #[allow(
                                    clippy::cast_possible_truncation,
                                    clippy::cast_possible_wrap
                                )]
                                self.ldc_i4(*value as u32 as i32);
                            }
                        }
                        self.op(format!("beq BB{}", target.0), 2, 0);
                    }
                }
                self.jump(*otherwise, bb);
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
            } => {
                self.call(*func, args, destination, term.span);
                // A return the jump inlines may read the call's result from the stack.
                self.stmt_start = self.code.len();
                match target {
                    Some(target) => self.jump(*target, bb),
                    None => self.unreachable(),
                }
            }
            TerminatorKind::Return => self.ret(),
            TerminatorKind::Unreachable => self.unreachable(),
        }
    }

    // ----------------------------------------------------------------------------------
    // Places
    // ----------------------------------------------------------------------------------

    /// Loads IL local `i`, or leaves out the load when its value is already on top of the
    /// stack, waiting for this statement to take it.
    fn ldloc(&mut self, i: u16) {
        if let Some(def) = self.consumed[usize::from(i)].take() {
            // Read a second time (a division's operand, a `switchInt` discriminant): the
            // value taken from the stack must also be stored.
            self.defs[def].mode = DefMode::DupSpill;
        } else if let Some(at) = self.pending.iter().position(|p| p.local == i) {
            let fresh = self.code.len() == self.stmt_start;
            let in_order = self.matching.is_none_or(|(_, next)| next == at);
            if fresh && in_order {
                let start = self.matching.map_or(at, |(start, _)| start);
                self.consumed[usize::from(i)] = Some(self.pending[at].def);
                if at + 1 == self.pending.len() {
                    self.pending.truncate(start);
                    self.matching = None;
                    // The statement has taken every value down to `start`; a value below
                    // it is under the ones taken, so a later load must not take it too.
                    self.stmt_start = usize::MAX;
                } else {
                    self.matching = Some((start, at + 1));
                }
                return;
            }
            self.settle();
            if let Some(at) = self.pending.iter().position(|p| p.local == i) {
                self.spill(at);
            }
        }
        self.settle();
        self.depth += 1;
        self.code.push(Instr::Local(LocalOp::Load, i));
    }

    fn stloc(&mut self, i: u16) {
        self.settle();
        debug_assert!(
            self.depth >= 1,
            "stack underflow lowering {}",
            self.body.name
        );
        self.depth -= 1;
        self.code.push(Instr::Local(LocalOp::Store, i));
    }

    fn ldloca(&mut self, i: u16) {
        self.settle();
        self.depth += 1;
        self.code.push(Instr::Local(LocalOp::Address, i));
    }

    fn ldarg(&mut self, i: u16) {
        let text = match i {
            0..=3 => format!("ldarg.{i}"),
            4..=255 => format!("ldarg.s {i}"),
            _ => format!("ldarg {i}"),
        };
        self.op(text, 0, 1);
    }

    fn short(&mut self, name: &str, i: u16, pops: usize, pushes: usize) {
        let text = if i <= 255 {
            format!("{name}.s {i}")
        } else {
            format!("{name} {i}")
        };
        self.op(text, pops, pushes);
    }

    /// The struct a field projection applies to, and the field's name and CIL type.
    fn field_ref(&mut self, base_ty: &Ty, index: usize, field_ty: &Ty, span: Span) -> String {
        let Ty::Adt(def) = base_ty else {
            panic!("field projection on non-struct {base_ty:?}");
        };
        let adt = self.cx.typeck.items.adt(*def);
        let struct_name = types::quote(&adt.name);
        let field_name = types::quote(&adt.fields[index].name);
        let cil = types::cil_type(self.cx, field_ty, span);
        format!("{cil} {struct_name}::{field_name}")
    }

    /// The field reference of element `index` of a tuple of type `base_ty`, after the
    /// `Rest` fields to go through first (a `System.ValueTuple` holds seven elements; `t.7`
    /// is `t.Rest.Item1`, `t.15` is `t.Rest.Rest.Item2`). The fields of a generic
    /// instantiation are referenced by their declared type, the type parameter (`!0`).
    fn tuple_field_ref(&mut self, base_ty: &Ty, index: usize, span: Span) -> (Vec<String>, String) {
        let Ty::Tuple(elems) = base_ty else {
            panic!("element projection on non-tuple {base_ty:?}");
        };
        if types::is_byref_tuple(base_ty) {
            let class = types::quote(&self.cx.byref_tuple(base_ty, span));
            let cil = types::cil_type(self.cx, &elems[index], span);
            return (Vec::new(), format!("{cil} {class}::'Item{}'", index + 1));
        }
        let mut rest = Vec::new();
        let mut elems: &[Ty] = elems;
        let mut index = index;
        while index >= types::VALUE_TUPLE_DIRECT {
            let class = types::value_tuple_type(self.cx, elems, span);
            rest.push(format!("!{} {class}::Rest", types::VALUE_TUPLE_DIRECT));
            elems = &elems[types::VALUE_TUPLE_DIRECT..];
            index -= types::VALUE_TUPLE_DIRECT;
        }
        let class = types::value_tuple_type(self.cx, elems, span);
        (rest, format!("!{index} {class}::Item{}", index + 1))
    }

    /// Pushes the address of `parent` and goes to the field `index` of it: returns the
    /// field reference for the final `ldfld`/`ldflda`/`stfld`.
    fn field_base(&mut self, parent: &Place, index: usize, field_ty: &Ty, span: Span) -> String {
        let base_ty = parent.ty(self.body);
        self.addr(parent, span);
        if let Ty::Tuple(_) = base_ty {
            let (rest, field) = self.tuple_field_ref(&base_ty, index, span);
            for hop in rest {
                self.op(format!("ldflda {hop}"), 1, 1);
            }
            field
        } else {
            self.field_ref(&base_ty, index, field_ty, span)
        }
    }

    fn parent(place: &Place) -> Place {
        Place {
            local: place.local,
            projection: place.projection[..place.projection.len() - 1].to_vec(),
        }
    }

    /// Pushes the value of `place`.
    fn load(&mut self, place: &Place, span: Span) {
        let ty = place.ty(self.body);
        if ty.is_unit() || ty.is_uninhabited() {
            return;
        }
        match place.projection.last() {
            None => match self.slots[place.local.index()] {
                Slot::Arg(i) => self.ldarg(i),
                Slot::Local(i) => self.ldloc(i),
                Slot::None => {}
            },
            Some(PlaceElem::Field(index, field_ty)) => {
                let parent = Self::parent(place);
                let field = self.field_base(&parent, *index, field_ty, span);
                self.op(format!("ldfld {field}"), 1, 1);
            }
            Some(PlaceElem::Deref) => {
                let parent = Self::parent(place);
                self.load(&parent, span);
                self.load_indirect(&ty, span);
            }
        }
    }

    fn load_indirect(&mut self, ty: &Ty, span: Span) {
        if let Some(s) = types::ind_suffix(ty) {
            self.op(format!("ldind.{s}"), 1, 1);
        } else {
            let cil = types::cil_type(self.cx, ty, span);
            self.op(format!("ldobj {cil}"), 1, 1);
        }
    }

    fn store_indirect(&mut self, ty: &Ty, span: Span) {
        if let Some(s) = types::ind_suffix(ty) {
            // CIL stores come in signed widths only (`stind.i1`, never `stind.u1`).
            let s = match s {
                "u1" => "i1",
                "u2" => "i2",
                "u4" => "i4",
                other => other,
            };
            self.op(format!("stind.{s}"), 2, 0);
        } else {
            let cil = types::cil_type(self.cx, ty, span);
            self.op(format!("stobj {cil}"), 2, 0);
        }
    }

    /// Pushes the address of `place`.
    fn addr(&mut self, place: &Place, span: Span) {
        match place.projection.last() {
            None => match self.slots[place.local.index()] {
                Slot::Arg(i) => self.short("ldarga", i, 0, 1),
                Slot::Local(i) => self.ldloca(i),
                Slot::None => {
                    self.cx.unsupported("a borrow of a `()` value", span);
                    self.ldc_i4(0);
                }
            },
            Some(PlaceElem::Field(index, field_ty)) => {
                let parent = Self::parent(place);
                let field = self.field_base(&parent, *index, field_ty, span);
                self.op(format!("ldflda {field}"), 1, 1);
            }
            Some(PlaceElem::Deref) => {
                let parent = Self::parent(place);
                self.load(&parent, span);
            }
        }
    }

    /// Stores the value pushed by `push` into `place`. For projected places the base
    /// address is computed first, so `push` runs with it on the stack.
    fn store(&mut self, place: &Place, span: Span, push: impl FnOnce(&mut Self)) {
        let ty = place.ty(self.body);
        match place.projection.last() {
            None => {
                push(self);
                match self.slots[place.local.index()] {
                    Slot::Arg(i) => self.short("starg", i, 1, 0),
                    Slot::Local(i) if self.stackable_def(place.local) => self.define(i),
                    Slot::Local(i) => self.stloc(i),
                    Slot::None => {}
                }
            }
            Some(PlaceElem::Field(index, field_ty)) => {
                let parent = Self::parent(place);
                let field = self.field_base(&parent, *index, field_ty, span);
                push(self);
                self.op(format!("stfld {field}"), 2, 0);
            }
            Some(PlaceElem::Deref) => {
                let parent = Self::parent(place);
                self.load(&parent, span);
                push(self);
                self.store_indirect(&ty, span);
            }
        }
    }

    // ----------------------------------------------------------------------------------
    // Operands and rvalues
    // ----------------------------------------------------------------------------------

    fn operand(&mut self, operand: &Operand, span: Span) {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.load(place, span),
            Operand::Constant(c) => self.constant(c),
        }
    }

    /// Like [`Self::operand`], for a position that needs a value even for `()` (a call
    /// argument, a tuple element): a `()` operand pushes a default `Sabiko.Runtime.Unit`
    /// instead of nothing. Reading a `()` place has no effect, so none is read.
    fn operand_value(&mut self, operand: &Operand, span: Span) {
        if operand.ty(self.body).is_unit() {
            let unit = self.scratch(&Ty::Unit, span);
            self.ldloc(unit);
        } else {
            self.operand(operand, span);
        }
    }

    /// Pushes a tuple of type `ty` built from `operands` (left to right) by its
    /// constructor: `System.ValueTuple`'s, whose eighth argument is the tuple of the
    /// remaining elements, built first; or the generated `ref struct`'s for a tuple with
    /// references (decision 52). Every operand is read before the tuple is stored, so an
    /// operand may read the destination (`p = (p.1, p.0)`).
    fn tuple(&mut self, ty: &Ty, operands: &[Operand], span: Span) {
        let Ty::Tuple(elems) = ty else {
            panic!("tuple aggregate of non-tuple type {ty:?}");
        };
        if types::is_byref_tuple(ty) {
            let class = types::quote(&self.cx.byref_tuple(ty, span));
            let params: Vec<String> = elems
                .iter()
                .map(|e| types::cil_type(self.cx, e, span))
                .collect();
            for op in operands {
                self.operand_value(op, span);
            }
            self.op(
                format!("newobj instance void {class}::.ctor({})", params.join(", ")),
                operands.len(),
                1,
            );
            return;
        }
        let direct = operands.len().min(types::VALUE_TUPLE_DIRECT);
        for op in &operands[..direct] {
            self.operand_value(op, span);
        }
        let mut arity = direct;
        if operands.len() > types::VALUE_TUPLE_DIRECT {
            let rest = Ty::Tuple(elems[types::VALUE_TUPLE_DIRECT..].to_vec().into());
            self.tuple(&rest, &operands[types::VALUE_TUPLE_DIRECT..], span);
            arity += 1;
        }
        let class = types::value_tuple_type(self.cx, elems, span);
        let params: Vec<String> = (0..arity).map(|i| format!("!{i}")).collect();
        self.op(
            format!("newobj instance void {class}::.ctor({})", params.join(", ")),
            arity,
            1,
        );
    }

    fn constant(&mut self, c: &Constant) {
        match &c.value {
            ConstValue::Unit => {}
            ConstValue::Bool(b) => self.ldc_i4(i32::from(*b)),
            ConstValue::Char(ch) => {
                let code = u32::from(*ch);
                if code > 0xFFFF {
                    // The type checker rejects these (E0318): a `char` is one UTF-16 code
                    // unit (decision 31). Reaching here is a compiler bug, never a
                    // truncated constant.
                    self.cx.error(
                        ErrorCode::INTERNAL_ERROR,
                        "internal error: a `char` constant outside the Basic Multilingual Plane reached code generation",
                        c.span,
                        "the type checker should have rejected this (E0318)",
                    );
                }
                #[allow(clippy::cast_possible_wrap)]
                self.ldc_i4((code & 0xFFFF) as i32);
            }
            ConstValue::Int(i) => {
                #[allow(clippy::cast_sign_loss)]
                let bits = *i as u128;
                self.typed_int_const(bits, &c.ty, c.span);
            }
            ConstValue::Uint(u) => self.typed_int_const(*u, &c.ty, c.span),
            ConstValue::Float(f) => self.float_const(*f, c.ty.as_prim() == Some(PrimTy::F32)),
            ConstValue::Str(s) => {
                self.op(format!("ldstr {}", types::il_string(s)), 0, 1);
                self.op(
                    format!("call {STR_SPAN} [System.Runtime]System.String::op_Implicit(string)"),
                    1,
                    1,
                );
            }
        }
    }

    fn float_const(&mut self, f: f64, single: bool) {
        let text = if single {
            #[allow(clippy::cast_possible_truncation)]
            let v = f as f32;
            if v.is_finite() {
                format!("ldc.r4 {v:?}")
            } else {
                format!("ldc.r4 float32(0x{:08X})", v.to_bits())
            }
        } else if f.is_finite() {
            format!("ldc.r8 {f:?}")
        } else {
            format!("ldc.r8 float64(0x{:016X})", f.to_bits())
        };
        self.op(text, 0, 1);
    }

    fn typed_int_const(&mut self, bits: u128, ty: &Ty, span: Span) {
        match ty.as_prim() {
            Some(p) if p.is_integer() => self.int_const(bits, p),
            Some(PrimTy::Char | PrimTy::Bool) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                self.ldc_i4(bits as u32 as i32);
            }
            _ => {
                types::cil_type(self.cx, ty, span);
                self.ldc_i4(0);
            }
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue, result_ty: &Ty, span: Span) {
        match rvalue {
            Rvalue::Use(op) => self.operand(op, span),
            Rvalue::Ref(_, place) => {
                let pointee = place.ty(self.body);
                if pointee.is_unit() || pointee.is_never() {
                    self.cx.unsupported("a reference to `()`", span);
                    self.ldc_i4(0);
                } else {
                    self.addr(place, span);
                }
            }
            Rvalue::UnaryOp(op, operand) => self.unary(*op, operand, span),
            Rvalue::BinaryOp(op, lhs, rhs) => self.binary(*op, lhs, rhs, span),
            Rvalue::Cast(operand, to) => {
                let from = operand.ty(self.body);
                self.operand(operand, span);
                self.cast(&from, to, span);
            }
            Rvalue::Aggregate(AggregateKind::Tuple, operands) => {
                self.tuple(result_ty, operands, span);
            }
            Rvalue::Aggregate(AggregateKind::Adt(_), _) => {
                // Handled by `assign`; an aggregate elsewhere would need staging.
                self.cx
                    .unsupported("a struct literal in this position", span);
                self.ldc_i4(0);
            }
        }
    }

    fn aggregate(&mut self, place: &Place, def: DefId, operands: &[Operand], span: Span) {
        let adt = self.cx.typeck.items.adt(def).clone();
        let struct_name = types::quote(&adt.name);
        let ty = Ty::Adt(def);
        // Build directly in the destination local unless an operand reads from it.
        let reads_dest = operands
            .iter()
            .any(|o| o.place().is_some_and(|p| p.local == place.local));
        let in_place = match (place.is_local(), self.slots[place.local.index()]) {
            (true, Slot::Local(i)) if !reads_dest => Some(i),
            _ => None,
        };
        let target = in_place.unwrap_or_else(|| self.scratch(&ty, span));
        self.ldloca(target);
        self.op(format!("initobj valuetype {struct_name}"), 1, 0);
        for (index, operand) in operands.iter().enumerate() {
            let field = &adt.fields[index];
            if field.ty.is_unit() || field.ty.is_never() {
                continue;
            }
            self.ldloca(target);
            self.operand(operand, span);
            let field_ref = self.field_ref(&ty, index, &field.ty, span);
            self.op(format!("stfld {field_ref}"), 2, 0);
        }
        if in_place.is_none() {
            self.store(place, span, |this| this.ldloc(target));
        }
    }

    // ----------------------------------------------------------------------------------
    // Arithmetic
    // ----------------------------------------------------------------------------------

    fn checks(&self) -> bool {
        self.cx.options.overflow_checks
    }

    /// Truncates a stack value to a narrow integer type after wrapping arithmetic.
    fn narrow(&mut self, p: PrimTy) {
        if types::stack_width(p) == StackWidth::Narrow {
            self.op(format!("conv.{}", types::conv_suffix(p)), 1, 1);
        }
    }

    fn unary(&mut self, op: UnOp, operand: &Operand, span: Span) {
        let ty = operand.ty(self.body);
        let Some(p) = ty.as_prim() else {
            self.cx.unsupported("this unary operation", span);
            self.ldc_i4(0);
            return;
        };
        let wide = types::stack_width(p) == StackWidth::Wide;
        match op {
            UnOp::Neg if p.is_float() => {
                self.operand(operand, span);
                self.op("neg", 1, 1);
            }
            UnOp::Neg if matches!(operand, Operand::Constant(_)) => {
                // `-literal`: fold instead of emitting `0 - literal`.
                let Operand::Constant(c) = operand else {
                    unreachable!()
                };
                match &c.value {
                    ConstValue::Int(i) => {
                        #[allow(clippy::cast_sign_loss)]
                        let bits = i.wrapping_neg() as u128;
                        self.typed_int_const(bits, &c.ty, c.span);
                    }
                    ConstValue::Uint(u) => self.typed_int_const(u.wrapping_neg(), &c.ty, c.span),
                    _ => {
                        self.constant(c);
                        self.op("neg", 1, 1);
                    }
                }
            }
            UnOp::Neg if self.checks() => {
                // `0 - x`, checked like a subtraction: negating the minimum value panics,
                // as in Rust's debug builds.
                self.checked_arith(BinOp::Sub, p, None, operand, Check::NegateOverflow, span);
            }
            UnOp::Neg if wide => {
                self.operand(operand, span);
                let ty = types::prim_type(p).expect("128-bit types are lowered");
                self.wide_call(p, ty, "op_UnaryNegation", &[ty]);
            }
            UnOp::Neg => {
                self.operand(operand, span);
                self.op("neg", 1, 1);
                self.narrow(p);
            }
            UnOp::Not if wide => {
                self.operand(operand, span);
                let ty = types::prim_type(p).expect("128-bit types are lowered");
                self.wide_call(p, ty, "op_OnesComplement", &[ty]);
            }
            UnOp::Not if p == PrimTy::Bool => {
                self.operand(operand, span);
                self.ldc_i4(0);
                self.op("ceq", 2, 1);
            }
            UnOp::Not => {
                self.operand(operand, span);
                self.op("not", 1, 1);
                // The complement of a zero-extended value has high bits set.
                if p.is_unsigned_int() {
                    self.narrow(p);
                }
            }
        }
    }

    fn binary(&mut self, op: BinOp, lhs: &Operand, rhs: &Operand, span: Span) {
        let lty = lhs.ty(self.body);
        let rty = rhs.ty(self.body);
        if lty.is_unit() && rty.is_unit() && op.is_comparison() {
            // `()` has one value: `==`, `<=`, `>=` hold, `!=`, `<`, `>` do not. Reading a
            // `()` operand has no effect, so neither is read.
            self.ldc_i4(i32::from(matches!(op, BinOp::Eq | BinOp::Le | BinOp::Ge)));
            return;
        }
        let (Some(lp), Some(rp)) = (lty.as_prim(), rty.as_prim()) else {
            self.cx.unsupported("this binary operation", span);
            self.ldc_i4(0);
            return;
        };
        if matches!(op, BinOp::Div | BinOp::Rem) {
            self.div_rem(op, lp, lhs, rhs, span);
            return;
        }
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) && lp.is_integer() && self.checks() {
            let check = match op {
                BinOp::Add => Check::AddOverflow,
                BinOp::Sub => Check::SubtractOverflow,
                _ => Check::MultiplyOverflow,
            };
            self.checked_arith(op, lp, Some(lhs), rhs, check, span);
            return;
        }
        self.operand(lhs, span);
        self.operand(rhs, span);
        let wide = types::stack_width(lp) == StackWidth::Wide;
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => self.arith(op, lp),
            BinOp::Div | BinOp::Rem => unreachable!(),
            BinOp::BitAnd if wide => self.wide_binary(lp, "op_BitwiseAnd"),
            BinOp::BitOr if wide => self.wide_binary(lp, "op_BitwiseOr"),
            BinOp::BitXor if wide => self.wide_binary(lp, "op_ExclusiveOr"),
            BinOp::BitAnd => self.op("and", 2, 1),
            BinOp::BitOr => self.op("or", 2, 1),
            BinOp::BitXor => self.op("xor", 2, 1),
            BinOp::Shl | BinOp::Shr => self.shift(op, lp, rp, span),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.compare(op, lp);
            }
            BinOp::And | BinOp::Or => {
                unreachable!("short-circuit operators are lowered to control flow")
            }
        }
    }

    /// `+`, `-`, `*` on floats, and on integers in release builds: wrapping.
    fn arith(&mut self, op: BinOp, p: PrimTy) {
        let (name, method) = match op {
            BinOp::Add => ("add", "op_Addition"),
            BinOp::Sub => ("sub", "op_Subtraction"),
            BinOp::Mul => ("mul", "op_Multiply"),
            _ => unreachable!(),
        };
        if types::stack_width(p) == StackWidth::Wide {
            self.wide_binary(p, method);
        } else {
            // Narrow types compute at `int32` width and truncate.
            self.op(name, 2, 1);
            self.narrow(p);
        }
    }

    /// `lhs op rhs` on an integer type in a debug build: fails `check` at `span`
    /// when the exact result does not fit `p` (decision 43). `lhs` is `None` for negation,
    /// which is `0 - rhs`. Narrow types compute exactly at `int32` width and 32-bit types
    /// at `int64` width, then check that the result survives truncation; 64- and 128-bit
    /// types call `Sabiko.Runtime.Arithmetic`'s `AddOverflows`/`SubOverflows`/
    /// `MulOverflows`, which return the wrapped result through a scratch local.
    fn checked_arith(
        &mut self,
        op: BinOp,
        p: PrimTy,
        lhs: Option<&Operand>,
        rhs: &Operand,
        check: Check,
        span: Span,
    ) {
        let (name, helper) = match op {
            BinOp::Add => ("add", "AddOverflows"),
            BinOp::Sub => ("sub", "SubOverflows"),
            BinOp::Mul => ("mul", "MulOverflows"),
            _ => unreachable!(),
        };
        match types::stack_width(p) {
            StackWidth::Narrow => {
                match lhs {
                    Some(l) => self.operand(l, span),
                    None => self.ldc_i4(0),
                }
                self.operand(rhs, span);
                self.op(name, 2, 1);
                self.check_fits(p, check, span);
            }
            StackWidth::I32 => {
                let widen = if p.is_signed_int() {
                    "conv.i8"
                } else {
                    "conv.u8"
                };
                match lhs {
                    Some(l) => {
                        self.operand(l, span);
                        self.op(widen, 1, 1);
                    }
                    None => self.ldc_i8(0),
                }
                self.operand(rhs, span);
                self.op(widen, 1, 1);
                self.op(name, 2, 1);
                self.check_fits(p, check, span);
            }
            StackWidth::I64 | StackWidth::Native | StackWidth::Wide => {
                match lhs {
                    Some(l) => self.operand(l, span),
                    None => self.zero(p),
                }
                self.operand(rhs, span);
                let result = self.scratch(&Ty::Prim(p), span);
                self.ldloca(result);
                let ty = types::prim_type(p).expect("integer types are lowered");
                self.op(
                    format!(
                        "call bool [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Arithmetic::{helper}({ty}, {ty}, {ty}&)"
                    ),
                    3,
                    1,
                );
                let ok = self.fresh_label();
                self.op(format!("brfalse {ok}"), 1, 0);
                self.panic_at(check, span);
                self.label(&ok);
                self.ldloc(result);
            }
        }
    }

    /// The debug-build check after an exact computation in a wider stack type (`int32` for
    /// the narrow types, `int64` for the 32-bit ones): panics unless truncating the result
    /// to `p` and extending it back gives the same value, then leaves it as a `p` value.
    fn check_fits(&mut self, p: PrimTy, check: Check, span: Span) {
        let suffix = types::conv_suffix(p);
        let wide_stack = types::stack_width(p) == StackWidth::I32;
        let ok = self.fresh_label();
        self.op("dup", 1, 2);
        self.op("dup", 1, 2);
        self.op(format!("conv.{suffix}"), 1, 1);
        if wide_stack {
            self.op(
                if p.is_signed_int() {
                    "conv.i8"
                } else {
                    "conv.u8"
                },
                1,
                1,
            );
        }
        self.op(format!("beq {ok}"), 2, 0);
        self.panic_at(check, span);
        self.label(&ok);
        if wide_stack {
            self.op(format!("conv.{suffix}"), 1, 1);
        }
    }

    /// The failure of `check` at `span`: calls its throw helper in `Sabiko.Runtime.Checks`
    /// with the location (`file:line:column`), which raises the `PanicException` with
    /// Rust's message. A span without source text (never produced for checks today) falls
    /// back to `Panic::Fail(message)`, without a location.
    fn panic_at(&mut self, check: Check, span: Span) {
        if let Some(location) = self.cx.location(span) {
            self.op(format!("ldstr {}", types::il_string(&location)), 0, 1);
            self.op(
                format!(
                    "call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Checks::{}(string)",
                    check.helper()
                ),
                1,
                0,
            );
        } else {
            self.op(format!("ldstr {}", types::il_string(check.message())), 0, 1);
            self.op(
                format!("call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::Fail(string)"),
                1,
                0,
            );
        }
        self.throw_after_helper();
    }

    /// Ends the block after a call to a throw helper, which never returns. The `throw` is
    /// never reached; it tells the verifier that control does not fall through and the
    /// JIT that the block is cold. `throw` empties the stack, so the continuation after the
    /// panic sees whatever it had before.
    fn throw_after_helper(&mut self) {
        self.op("ldnull", 0, 1);
        self.op("throw", 1, 0);
    }

    /// The integer value of a constant operand, if it is one.
    fn const_int(operand: &Operand) -> Option<i128> {
        match operand {
            Operand::Constant(c) => match c.value {
                ConstValue::Int(i) => Some(i),
                ConstValue::Uint(u) => i128::try_from(u).ok(),
                _ => None,
            },
            _ => None,
        }
    }

    /// `/` and `%` with Rust's checks and messages in every build mode: a zero divisor
    /// panics, and for signed types so does `MIN / -1` (`MIN % -1`). The CLR traps both for
    /// `int32` and wider, but with its own messages, and computes narrow types at `int32`
    /// width where `MIN % -1` is simply 0; so the checks are emitted explicitly, and folded
    /// away when a constant operand rules them out. Operands are pure (places and
    /// constants), so each check pushes them again.
    fn div_rem(&mut self, op: BinOp, p: PrimTy, lhs: &Operand, rhs: &Operand, span: Span) {
        let (name, zero_check, overflow_check) = if op == BinOp::Div {
            ("div", Check::DivideByZero, Check::DivideOverflow)
        } else {
            ("rem", Check::RemainderByZero, Check::RemainderOverflow)
        };
        if p.is_float() {
            self.operand(lhs, span);
            self.operand(rhs, span);
            self.op(name, 2, 1);
            return;
        }
        let rhs_const = Self::const_int(rhs);
        let lhs_const = Self::const_int(lhs);
        match rhs_const {
            Some(0) => {
                self.panic_at(zero_check, span);
                // Unreachable, but the store that follows expects a value.
                self.ldc_i4(0);
                return;
            }
            Some(_) => {}
            None => {
                let ok = self.fresh_label();
                self.operand(rhs, span);
                if types::stack_width(p) == StackWidth::Wide {
                    self.zero(p);
                    self.branch_ne(p, &ok);
                } else {
                    self.op(format!("brtrue {ok}"), 1, 0);
                }
                self.panic_at(zero_check, span);
                self.label(&ok);
            }
        }
        // `MIN` of the signed type, by sign extension (`-(1 << 127)` would overflow).
        let min = p
            .int_bits()
            .map_or(i128::MIN, |bits| i128::MIN >> (128 - bits));
        let may_overflow = p.is_signed_int()
            && rhs_const.is_none_or(|r| r == -1)
            && lhs_const.is_none_or(|l| l == min);
        if may_overflow {
            let ok = self.fresh_label();
            if rhs_const.is_none() {
                self.operand(rhs, span);
                self.int_const(u128::MAX, p);
                self.branch_ne(p, &ok);
            }
            if lhs_const.is_none() {
                self.operand(lhs, span);
                #[allow(clippy::cast_sign_loss)]
                self.int_const(min as u128, p);
                self.branch_ne(p, &ok);
            }
            self.panic_at(overflow_check, span);
            self.label(&ok);
        }
        self.operand(lhs, span);
        self.operand(rhs, span);
        if types::stack_width(p) == StackWidth::Wide {
            let method = if op == BinOp::Div {
                "op_Division"
            } else {
                "op_Modulus"
            };
            self.wide_binary(p, method);
        } else {
            let un = if p.is_unsigned_int() { ".un" } else { "" };
            self.op(format!("{name}{un}"), 2, 1);
        }
    }

    fn shift(&mut self, op: BinOp, lp: PrimTy, rp: PrimTy, span: Span) {
        let bits = lp.int_bits().unwrap_or(32);
        let rwidth = types::stack_width(rp);
        if self.checks() {
            // Rust panics when the shift amount is not below the bit width; negative
            // amounts compare as huge unsigned values and panic too.
            let ok = self.fresh_label();
            self.op("dup", 1, 2);
            #[allow(clippy::cast_possible_wrap)]
            match rwidth {
                StackWidth::I64 => self.ldc_i8(i64::from(bits)),
                StackWidth::Native => {
                    self.ldc_i4(bits as i32);
                    self.op("conv.i", 1, 1);
                }
                StackWidth::Narrow | StackWidth::I32 => self.ldc_i4(bits as i32),
                StackWidth::Wide => {
                    // Compare as `u128`: reinterpret a signed amount first.
                    if rp == PrimTy::I128 {
                        self.wide_call(rp, types::UINT128, "op_Explicit", &[types::INT128]);
                    }
                    self.int_const(u128::from(bits), PrimTy::U128);
                    self.wide_compare(PrimTy::U128, "op_LessThan");
                }
            }
            if rwidth == StackWidth::Wide {
                self.op(format!("brtrue {ok}"), 1, 0);
            } else {
                self.op(format!("blt.un {ok}"), 2, 0);
            }
            let check = if op == BinOp::Shl {
                Check::ShiftLeftOverflow
            } else {
                Check::ShiftRightOverflow
            };
            self.panic_at(check, span);
            self.label(&ok);
            self.shift_amount_to_i4(rp);
        } else {
            self.shift_amount_to_i4(rp);
            #[allow(clippy::cast_possible_wrap)]
            self.ldc_i4((bits - 1) as i32);
            self.op("and", 2, 1);
        }
        if types::stack_width(lp) == StackWidth::Wide {
            // The operator methods mask the amount with 127 themselves; it is in range.
            let ty = types::prim_type(lp).expect("128-bit types are lowered");
            let method = if op == BinOp::Shl {
                "op_LeftShift"
            } else {
                // Arithmetic for `Int128`, logical for `UInt128`.
                "op_RightShift"
            };
            self.wide_call(lp, ty, method, &[ty, "int32"]);
            return;
        }
        match op {
            BinOp::Shl => {
                self.op("shl", 2, 1);
                // Shifting left never panics on the result; it truncates.
                self.narrow(lp);
            }
            _ if lp.is_unsigned_int() => self.op("shr.un", 2, 1),
            _ => self.op("shr", 2, 1),
        }
    }

    /// Truncates a shift amount of type `rp` on the stack to `int32`.
    fn shift_amount_to_i4(&mut self, rp: PrimTy) {
        match types::stack_width(rp) {
            StackWidth::I64 | StackWidth::Native => self.op("conv.i4", 1, 1),
            StackWidth::Wide => {
                let ty = types::prim_type(rp).expect("128-bit types are lowered");
                self.wide_call(rp, "int32", "op_Explicit", &[ty]);
            }
            StackWidth::Narrow | StackWidth::I32 => {}
        }
    }

    fn compare(&mut self, op: BinOp, p: PrimTy) {
        if types::stack_width(p) == StackWidth::Wide {
            let method = match op {
                BinOp::Eq => "op_Equality",
                BinOp::Ne => "op_Inequality",
                BinOp::Lt => "op_LessThan",
                BinOp::Le => "op_LessThanOrEqual",
                BinOp::Gt => "op_GreaterThan",
                BinOp::Ge => "op_GreaterThanOrEqual",
                _ => unreachable!(),
            };
            self.wide_compare(p, method);
            return;
        }
        let float = p.is_float();
        // Unsigned comparisons for unsigned integers, `bool` and `char`; the `.un` forms
        // on floats are the "unordered" ones, which make `<=`/`>=` false for NaN.
        let un = !p.is_signed_int() && !float;
        let suffix = |base: &str, unsigned: bool| {
            if unsigned {
                format!("{base}.un")
            } else {
                base.to_owned()
            }
        };
        match op {
            BinOp::Eq => self.op("ceq", 2, 1),
            BinOp::Ne => {
                self.op("ceq", 2, 1);
                self.ldc_i4(0);
                self.op("ceq", 2, 1);
            }
            BinOp::Lt => self.op(suffix("clt", un), 2, 1),
            BinOp::Gt => self.op(suffix("cgt", un), 2, 1),
            BinOp::Le => {
                self.op(suffix("cgt", un || float), 2, 1);
                self.ldc_i4(0);
                self.op("ceq", 2, 1);
            }
            BinOp::Ge => {
                self.op(suffix("clt", un || float), 2, 1);
                self.ldc_i4(0);
                self.op("ceq", 2, 1);
            }
            _ => unreachable!(),
        }
    }

    /// Converts the stack value from `from` to `to` with Rust `as` semantics.
    fn cast(&mut self, from: &Ty, to: &Ty, span: Span) {
        if from == to {
            return;
        }
        let (Some(fp), Some(tp)) = (from.as_prim(), to.as_prim()) else {
            self.cx.unsupported("this cast", span);
            return;
        };
        if types::stack_width(fp) == StackWidth::Wide || types::stack_width(tp) == StackWidth::Wide
        {
            self.wide_cast(fp, tp);
            return;
        }
        // `bool` and `char` behave as unsigned integers on the stack.
        let from_signed = fp.is_signed_int();
        match (fp.is_float(), tp.is_float()) {
            (false, false) => {
                let suffix = match tp {
                    PrimTy::I64 | PrimTy::U64 => {
                        if from_signed {
                            "i8"
                        } else {
                            "u8"
                        }
                    }
                    PrimTy::Isize | PrimTy::Usize => {
                        if from_signed {
                            "i"
                        } else {
                            "u"
                        }
                    }
                    _ => types::conv_suffix(tp),
                };
                self.op(format!("conv.{suffix}"), 1, 1);
            }
            (false, true) => {
                if !from_signed {
                    self.op("conv.r.un", 1, 1);
                }
                self.op(format!("conv.{}", types::conv_suffix(tp)), 1, 1);
            }
            (true, true) => self.op(format!("conv.{}", types::conv_suffix(tp)), 1, 1),
            (true, false) => {
                if types::stack_width(tp) == StackWidth::Narrow {
                    // The CLR saturates to `int32` only; clamp to the narrow range first.
                    let (min, max): (i64, i64) = match tp {
                        PrimTy::I8 => (i64::from(i8::MIN), i64::from(i8::MAX)),
                        PrimTy::I16 => (i64::from(i16::MIN), i64::from(i16::MAX)),
                        PrimTy::U8 => (0, i64::from(u8::MAX)),
                        _ => (0, i64::from(u16::MAX)),
                    };
                    self.op("conv.r8", 1, 1);
                    self.op(format!("ldc.r8 {min}."), 0, 1);
                    self.op(format!("ldc.r8 {max}."), 0, 1);
                    self.op(
                        "call float64 [System.Runtime]System.Math::Clamp(float64, float64, float64)",
                        3,
                        1,
                    );
                    // NaN clamps to NaN and converts to 0, as in Rust.
                    self.op("conv.i4", 1, 1);
                } else {
                    self.op(format!("conv.{}", types::conv_suffix(tp)), 1, 1);
                }
            }
        }
    }

    /// A cast from or to `i128`/`u128`, through the conversion operators of
    /// `System.Int128`/`System.UInt128`. Integer conversions follow Rust: widening
    /// sign-extends a signed source (`op_Implicit` into `Int128`, `op_Explicit` into
    /// `UInt128`) and zero-extends an unsigned one, narrowing truncates; float to 128-bit
    /// saturates with NaN as 0, as the BCL operators do. 128-bit to float goes through
    /// `Sabiko.Runtime.Arithmetic`, because the BCL rounds twice (via `double`) on the way
    /// to `float`.
    fn wide_cast(&mut self, fp: PrimTy, tp: PrimTy) {
        // `bool` sits on the stack as `int32` 0/1; pass it as a `uint8`.
        let from_ty = if fp == PrimTy::Bool {
            "uint8"
        } else {
            types::prim_type(fp).expect("primitive casts are lowered")
        };
        let to_ty = types::prim_type(tp).expect("primitive casts are lowered");
        if types::stack_width(fp) == StackWidth::Wide && types::stack_width(tp) == StackWidth::Wide
        {
            // `Int128` declares the conversion to `UInt128` and vice versa: a reinterpretation.
            self.wide_call(fp, to_ty, "op_Explicit", &[from_ty]);
        } else if types::stack_width(tp) == StackWidth::Wide {
            let method = if fp.is_float() || (tp == PrimTy::U128 && fp.is_signed_int()) {
                "op_Explicit"
            } else {
                "op_Implicit"
            };
            self.wide_call(tp, to_ty, method, &[from_ty]);
        } else if tp.is_float() {
            let method = if tp == PrimTy::F32 {
                "ToSingle"
            } else {
                "ToDouble"
            };
            self.op(
                format!(
                    "call {to_ty} [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Arithmetic::{method}({from_ty})"
                ),
                1,
                1,
            );
        } else {
            self.wide_call(fp, to_ty, "op_Explicit", &[from_ty]);
        }
    }

    // ----------------------------------------------------------------------------------
    // Calls
    // ----------------------------------------------------------------------------------

    fn call(&mut self, func: DefId, args: &[Operand], destination: &Place, span: Span) {
        match self.cx.resolve.builtin(func) {
            Some(builtin @ (Builtin::Print | Builtin::PrintLine)) => {
                let arg_ty = args[0].ty(self.body);
                let overload = types::print_overload(&arg_ty).unwrap_or_else(|| {
                    self.cx.unsupported("printing a value of this type", span);
                    "int32"
                });
                self.operand(&args[0], span);
                self.op(
                    format!(
                        "call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Console::{}({overload})",
                        builtin.name()
                    ),
                    1,
                    0,
                );
            }
            Some(Builtin::Panic) => {
                self.operand(&args[0], span);
                if let Some(location) = self.cx.location(span) {
                    self.op(format!("ldstr {}", types::il_string(&location)), 0, 1);
                    self.op(
                        format!(
                            "call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::Fail({STR_SPAN}, string)"
                        ),
                        2,
                        0,
                    );
                } else {
                    self.op(
                        format!(
                            "call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::Fail({STR_SPAN})"
                        ),
                        1,
                        0,
                    );
                }
            }
            None => {
                let sig = self.cx.typeck.items.fn_sig(func).clone();
                let name = self.cx.resolve.def(func).name.clone();
                let ret = types::ret_type(self.cx, &sig.output, span);
                let params: Vec<String> = sig
                    .inputs
                    .iter()
                    .map(|t| types::param_type(self.cx, t, span))
                    .collect();
                let callee = format!(
                    "call {ret} {}::{}({})",
                    self.cx.owner_class(func),
                    types::quote(&name),
                    params.join(", ")
                );
                let returns_value = !(sig.output.is_unit() || sig.output.is_never());
                if returns_value {
                    self.store(destination, span, |this| {
                        for a in args {
                            this.operand_value(a, span);
                        }
                        this.op(callee, args.len(), 1);
                    });
                } else {
                    for a in args {
                        self.operand_value(a, span);
                    }
                    self.op(callee, args.len(), 0);
                }
            }
        }
    }
}

/// A run-time check whose failure panics with Rust's message (decision 43). Each has a
/// throw helper in `Sabiko.Runtime.Checks` that takes the location and supplies the
/// message, so a failing check costs one `ldstr` and a `call` at its site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    AddOverflow,
    SubtractOverflow,
    MultiplyOverflow,
    NegateOverflow,
    DivideByZero,
    DivideOverflow,
    RemainderByZero,
    RemainderOverflow,
    ShiftLeftOverflow,
    ShiftRightOverflow,
}

impl Check {
    #[cfg(test)]
    pub(crate) const ALL: [Check; 10] = [
        Check::AddOverflow,
        Check::SubtractOverflow,
        Check::MultiplyOverflow,
        Check::NegateOverflow,
        Check::DivideByZero,
        Check::DivideOverflow,
        Check::RemainderByZero,
        Check::RemainderOverflow,
        Check::ShiftLeftOverflow,
        Check::ShiftRightOverflow,
    ];

    /// The method of `Sabiko.Runtime.Checks` that raises the panic.
    pub(crate) fn helper(self) -> &'static str {
        match self {
            Check::AddOverflow => "AddOverflow",
            Check::SubtractOverflow => "SubtractOverflow",
            Check::MultiplyOverflow => "MultiplyOverflow",
            Check::NegateOverflow => "NegateOverflow",
            Check::DivideByZero => "DivideByZero",
            Check::DivideOverflow => "DivideOverflow",
            Check::RemainderByZero => "RemainderByZero",
            Check::RemainderOverflow => "RemainderOverflow",
            Check::ShiftLeftOverflow => "ShiftLeftOverflow",
            Check::ShiftRightOverflow => "ShiftRightOverflow",
        }
    }

    /// Rust's panic message, which the helper raises.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Check::AddOverflow => "attempt to add with overflow",
            Check::SubtractOverflow => "attempt to subtract with overflow",
            Check::MultiplyOverflow => "attempt to multiply with overflow",
            Check::NegateOverflow => "attempt to negate with overflow",
            Check::DivideByZero => "attempt to divide by zero",
            Check::DivideOverflow => "attempt to divide with overflow",
            Check::RemainderByZero => "attempt to calculate the remainder with a divisor of zero",
            Check::RemainderOverflow => "attempt to calculate the remainder with overflow",
            Check::ShiftLeftOverflow => "attempt to shift left with overflow",
            Check::ShiftRightOverflow => "attempt to shift right with overflow",
        }
    }
}

/// `ldloc`, `stloc` or `ldloca` of local `i`, in its shortest form.
fn local_instr(op: LocalOp, i: u16) -> String {
    let name = match op {
        LocalOp::Load => "ldloc",
        LocalOp::Store => "stloc",
        LocalOp::Address => "ldloca",
    };
    match i {
        0..=3 if op != LocalOp::Address => format!("{name}.{i}"),
        0..=255 => format!("{name}.s {i}"),
        _ => format!("{name} {i}"),
    }
}

fn is_mut_ref(ty: &Ty) -> bool {
    matches!(ty, Ty::Ref(m, _) if m.is_mut())
}

#[cfg(test)]
mod tests {
    use super::Check;

    /// Every throw helper exists in the runtime's source and raises exactly the message
    /// the check stands for (the run-time differential tests compare the messages with
    /// rustc's).
    #[test]
    fn check_helpers_raise_rusts_messages() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../runtime/Sabiko.Runtime/Checks.cs"
        );
        let src = std::fs::read_to_string(path).expect("runtime/Sabiko.Runtime/Checks.cs");
        for check in Check::ALL {
            let decl = format!("public static void {}(string location) =>", check.helper());
            let at = src
                .find(&decl)
                .unwrap_or_else(|| panic!("Checks.cs declares no `{decl}`"));
            let body = src[at + decl.len()..].trim_start();
            let expected = format!(
                "throw new PanicException(\"{}\", location);",
                check.message()
            );
            assert!(
                body.starts_with(&expected),
                "Checks.{} should be `{expected}`, is `{}`",
                check.helper(),
                body.lines().next().unwrap_or_default()
            );
        }
        assert!(src.contains(
            "public static void Unreachable() =>\n        throw new PanicException(\"internal error: entered unreachable code\");"
        ));
    }
}
