//! Lowering of one MIR body to an IL method.
//!
//! Every MIR statement and terminator is lowered to a short instruction sequence that
//! leaves the evaluation stack empty, so block labels never carry stack state. Places are
//! accessed through their address: `ldloca`/`ldarga` for locals and arguments, `ldflda`
//! for fields, the pointer value itself for `*r`. Locals of type `()` or `!` get no
//! storage: assignments to them vanish and reads push nothing.

use std::fmt::Write as _;

use sabiko_mir::{
    BasicBlock, BinOp, Body, ConstValue, Constant, Local, LocalKind, Operand, Place, PlaceElem,
    Rvalue, Statement, StatementKind, Terminator, TerminatorKind, Ty, UnOp,
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
        max_depth: 0,
        next_label: 0,
        scratch: Vec::new(),
    };
    f.assign_slots();
    for bb in body.block_ids() {
        f.block(bb);
    }
    f.write_method(out);
}

struct FnCx<'a, 'b> {
    cx: &'a mut Cx<'b>,
    body: &'a Body,
    slots: Vec<Slot>,
    /// `.locals init` entries, one per IL local slot.
    locals: Vec<String>,
    code: Vec<String>,
    depth: usize,
    max_depth: usize,
    next_label: u32,
    /// Compiler-introduced IL locals that stage struct literals, one per struct type.
    scratch: Vec<(Ty, u16)>,
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
            let elided = decl.ty.is_unit() || decl.ty.is_never();
            let slot = match decl.kind {
                LocalKind::Arg => {
                    if elided {
                        self.cx.unsupported("a parameter of type `()`", decl.span);
                    }
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
        self.locals
            .push(format!("[{index}] {cil} {}", types::quote(name)));
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
        let ret = types::sig_type(self.cx, body.return_ty(), body.span);
        let mut params = Vec::new();
        let mut readonly_params = Vec::new();
        if Cx::is_byref(body.return_ty()) && !is_mut_ref(body.return_ty()) {
            readonly_params.push(0);
        }
        for (i, local) in body.args().enumerate() {
            let decl = body.local_decl(local);
            let ty = types::sig_type(self.cx, &decl.ty, decl.span);
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
        let _ = writeln!(out, "    .maxstack {}", self.max_depth.max(1));
        if !self.locals.is_empty() {
            out.push_str("    .locals init (\n");
            for (i, l) in self.locals.iter().enumerate() {
                let sep = if i + 1 == self.locals.len() { "" } else { "," };
                let _ = writeln!(out, "      {l}{sep}");
            }
            out.push_str("    )\n");
        }
        for line in &self.code {
            if line.ends_with(':') {
                let _ = writeln!(out, "  {line}");
            } else {
                let _ = writeln!(out, "    {line}");
            }
        }
        out.push_str("  }\n\n");
    }

    // ----------------------------------------------------------------------------------
    // Instruction stream
    // ----------------------------------------------------------------------------------

    /// Emits one instruction with its stack effect.
    fn op(&mut self, text: impl Into<String>, pops: usize, pushes: usize) {
        debug_assert!(
            self.depth >= pops,
            "stack underflow lowering {}",
            self.body.name
        );
        self.depth = self.depth.saturating_sub(pops) + pushes;
        self.max_depth = self.max_depth.max(self.depth);
        self.code.push(text.into());
    }

    fn label(&mut self, name: &str) {
        self.code.push(format!("{name}:"));
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
        }
    }

    /// Pushes zero of the integer type `p`.
    fn zero(&mut self, p: PrimTy) {
        self.int_const(0, p);
    }

    fn unreachable(&mut self) {
        self.panic("internal error: entered unreachable code");
    }

    // ----------------------------------------------------------------------------------
    // Blocks
    // ----------------------------------------------------------------------------------

    fn block(&mut self, bb: BasicBlock) {
        self.label(&format!("BB{}", bb.0));
        let data = self.body.block(bb);
        for stmt in &data.statements {
            self.stmt(stmt);
            debug_assert_eq!(self.depth, 0, "statement left values on the stack");
        }
        self.terminator(data.terminator(), bb);
        debug_assert_eq!(self.depth, 0, "terminator left values on the stack");
    }

    fn stmt(&mut self, stmt: &Statement) {
        match &stmt.kind {
            StatementKind::Assign(place, rvalue) => self.assign(place, rvalue, stmt.span),
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
        }
    }

    fn assign(&mut self, place: &Place, rvalue: &Rvalue, span: Span) {
        let ty = place.ty(self.body);
        if ty.is_unit() || ty.is_never() {
            // Rvalues have no side effects; a unit store is nothing.
            return;
        }
        if let Rvalue::Aggregate(def, operands) = rvalue {
            self.aggregate(place, *def, operands, span);
            return;
        }
        self.store(place, span, |this| this.rvalue(rvalue, &ty, span));
    }

    /// Branches to `target` unless it is the block emitted next.
    fn jump(&mut self, target: BasicBlock, from: BasicBlock) {
        if target.0 != from.0 + 1 {
            self.op(format!("br BB{}", target.0), 0, 0);
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
                    self.op(format!("brfalse BB{}", zero_target.0), 1, 0);
                } else {
                    for (value, target) in targets {
                        self.operand(discr, term.span);
                        match discr_ty.as_prim() {
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
                match target {
                    Some(target) => self.jump(*target, bb),
                    None => self.unreachable(),
                }
            }
            TerminatorKind::Return => {
                if let Slot::Local(i) = self.slots[Local::RETURN.index()] {
                    self.ldloc(i);
                }
                self.op("ret", usize::from(self.depth > 0), 0);
            }
            TerminatorKind::Unreachable => self.unreachable(),
        }
    }

    // ----------------------------------------------------------------------------------
    // Places
    // ----------------------------------------------------------------------------------

    fn ldloc(&mut self, i: u16) {
        let text = match i {
            0..=3 => format!("ldloc.{i}"),
            4..=255 => format!("ldloc.s {i}"),
            _ => format!("ldloc {i}"),
        };
        self.op(text, 0, 1);
    }

    fn stloc(&mut self, i: u16) {
        let text = match i {
            0..=3 => format!("stloc.{i}"),
            4..=255 => format!("stloc.s {i}"),
            _ => format!("stloc {i}"),
        };
        self.op(text, 1, 0);
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

    fn parent(place: &Place) -> Place {
        Place {
            local: place.local,
            projection: place.projection[..place.projection.len() - 1].to_vec(),
        }
    }

    /// Pushes the value of `place`.
    fn load(&mut self, place: &Place, span: Span) {
        let ty = place.ty(self.body);
        if ty.is_unit() || ty.is_never() {
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
                let base_ty = parent.ty(self.body);
                self.addr(&parent, span);
                let field = self.field_ref(&base_ty, *index, field_ty, span);
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
                Slot::Local(i) => self.short("ldloca", i, 0, 1),
                Slot::None => {
                    self.cx.unsupported("a borrow of a `()` value", span);
                    self.ldc_i4(0);
                }
            },
            Some(PlaceElem::Field(index, field_ty)) => {
                let parent = Self::parent(place);
                let base_ty = parent.ty(self.body);
                self.addr(&parent, span);
                let field = self.field_ref(&base_ty, *index, field_ty, span);
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
                    Slot::Local(i) => self.stloc(i),
                    Slot::None => {}
                }
            }
            Some(PlaceElem::Field(index, field_ty)) => {
                let parent = Self::parent(place);
                let base_ty = parent.ty(self.body);
                self.addr(&parent, span);
                push(self);
                let field = self.field_ref(&base_ty, *index, field_ty, span);
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

    fn constant(&mut self, c: &Constant) {
        match &c.value {
            ConstValue::Unit => {}
            ConstValue::Bool(b) => self.ldc_i4(i32::from(*b)),
            ConstValue::Char(ch) => {
                let code = u32::from(*ch);
                if code > 0xFFFF {
                    self.cx.unsupported(
                        "a `char` literal outside the UTF-16 code unit range",
                        c.span,
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
            Some(p) if p.is_integer() && types::prim_type(p).is_some() => self.int_const(bits, p),
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
            Rvalue::Aggregate(..) => {
                // Handled by `assign`; an aggregate elsewhere would need staging.
                let _ = result_ty;
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
        self.short("ldloca", target, 0, 1);
        self.op(format!("initobj valuetype {struct_name}"), 1, 0);
        for (index, operand) in operands.iter().enumerate() {
            let field = &adt.fields[index];
            if field.ty.is_unit() || field.ty.is_never() {
                continue;
            }
            self.short("ldloca", target, 0, 1);
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

    /// Re-narrows a stack value to a narrow integer type after arithmetic: trapping on
    /// overflow when `checked`, truncating otherwise.
    fn narrow(&mut self, p: PrimTy, checked: bool) {
        if types::stack_width(p) == StackWidth::Narrow {
            let ovf = if checked { ".ovf" } else { "" };
            self.op(format!("conv{ovf}.{}", types::conv_suffix(p)), 1, 1);
        }
    }

    fn unary(&mut self, op: UnOp, operand: &Operand, span: Span) {
        let ty = operand.ty(self.body);
        let Some(p) = ty.as_prim() else {
            self.cx.unsupported("this unary operation", span);
            self.ldc_i4(0);
            return;
        };
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
                // `0 - x`, so that negating the minimum value traps like Rust's debug mode.
                self.zero(p);
                self.operand(operand, span);
                if types::stack_width(p) == StackWidth::Narrow {
                    self.op("sub", 2, 1);
                    self.narrow(p, true);
                } else {
                    self.op("sub.ovf", 2, 1);
                }
            }
            UnOp::Neg => {
                self.operand(operand, span);
                self.op("neg", 1, 1);
                self.narrow(p, false);
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
                    self.narrow(p, false);
                }
            }
        }
    }

    fn binary(&mut self, op: BinOp, lhs: &Operand, rhs: &Operand, span: Span) {
        let lty = lhs.ty(self.body);
        let rty = rhs.ty(self.body);
        let (Some(lp), Some(rp)) = (lty.as_prim(), rty.as_prim()) else {
            self.cx.unsupported("this binary operation", span);
            self.ldc_i4(0);
            return;
        };
        if matches!(op, BinOp::Div | BinOp::Rem) {
            self.div_rem(op, lp, lhs, rhs, span);
            return;
        }
        self.operand(lhs, span);
        self.operand(rhs, span);
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => self.arith(op, lp),
            BinOp::Div | BinOp::Rem => unreachable!(),
            BinOp::BitAnd => self.op("and", 2, 1),
            BinOp::BitOr => self.op("or", 2, 1),
            BinOp::BitXor => self.op("xor", 2, 1),
            BinOp::Shl | BinOp::Shr => self.shift(op, lp, rp),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.compare(op, lp);
            }
            BinOp::And | BinOp::Or => {
                unreachable!("short-circuit operators are lowered to control flow")
            }
        }
    }

    fn arith(&mut self, op: BinOp, p: PrimTy) {
        let name = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            _ => unreachable!(),
        };
        if p.is_float() {
            self.op(name, 2, 1);
            return;
        }
        let checked = self.checks();
        match types::stack_width(p) {
            StackWidth::Narrow => {
                // Computed at `int32` width; the result fits, so overflow of the narrow
                // type is caught (or wrapped) by the conversion.
                self.op(name, 2, 1);
                self.narrow(p, checked);
            }
            _ if checked => {
                let un = if p.is_unsigned_int() { ".un" } else { "" };
                self.op(format!("{name}.ovf{un}"), 2, 1);
            }
            _ => self.op(name, 2, 1),
        }
    }

    /// Throws a `PanicException` with `message`; `throw` empties the stack, so the
    /// continuation after the panic sees whatever it had before.
    fn panic(&mut self, message: &str) {
        self.op(format!("ldstr \"{message}\""), 0, 1);
        self.op(
            format!("newobj instance void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.PanicException::.ctor(string)"),
            1,
            1,
        );
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
        let (name, zero_msg, overflow_msg) = if op == BinOp::Div {
            (
                "div",
                "attempt to divide by zero",
                "attempt to divide with overflow",
            )
        } else {
            (
                "rem",
                "attempt to calculate the remainder with a divisor of zero",
                "attempt to calculate the remainder with overflow",
            )
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
                self.panic(zero_msg);
                // Unreachable, but the store that follows expects a value.
                self.ldc_i4(0);
                return;
            }
            Some(_) => {}
            None => {
                let ok = self.fresh_label();
                self.operand(rhs, span);
                self.op(format!("brtrue {ok}"), 1, 0);
                self.panic(zero_msg);
                self.label(&ok);
            }
        }
        let min = p
            .int_bits()
            .map_or(i128::MIN, |bits| -(1i128 << (bits - 1)));
        let may_overflow = p.is_signed_int()
            && rhs_const.is_none_or(|r| r == -1)
            && lhs_const.is_none_or(|l| l == min);
        if may_overflow {
            let ok = self.fresh_label();
            if rhs_const.is_none() {
                self.operand(rhs, span);
                self.int_const(u128::MAX, p);
                self.op(format!("bne.un {ok}"), 2, 0);
            }
            if lhs_const.is_none() {
                self.operand(lhs, span);
                #[allow(clippy::cast_sign_loss)]
                self.int_const(min as u128, p);
                self.op(format!("bne.un {ok}"), 2, 0);
            }
            self.panic(overflow_msg);
            self.label(&ok);
        }
        self.operand(lhs, span);
        self.operand(rhs, span);
        let un = if p.is_unsigned_int() { ".un" } else { "" };
        self.op(format!("{name}{un}"), 2, 1);
    }

    fn shift(&mut self, op: BinOp, lp: PrimTy, rp: PrimTy) {
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
            }
            self.op(format!("blt.un {ok}"), 2, 0);
            let dir = if op == BinOp::Shl { "left" } else { "right" };
            self.panic(&format!("attempt to shift {dir} with overflow"));
            self.label(&ok);
            if matches!(rwidth, StackWidth::I64 | StackWidth::Native) {
                self.op("conv.i4", 1, 1);
            }
        } else {
            if matches!(rwidth, StackWidth::I64 | StackWidth::Native) {
                self.op("conv.i4", 1, 1);
            }
            #[allow(clippy::cast_possible_wrap)]
            self.ldc_i4((bits - 1) as i32);
            self.op("and", 2, 1);
        }
        match op {
            BinOp::Shl => {
                self.op("shl", 2, 1);
                // Shifting left never panics on the result; it truncates.
                self.narrow(lp, false);
            }
            _ if lp.is_unsigned_int() => self.op("shr.un", 2, 1),
            _ => self.op("shr", 2, 1),
        }
    }

    fn compare(&mut self, op: BinOp, p: PrimTy) {
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
        if types::prim_type(fp).is_none() || types::prim_type(tp).is_none() {
            self.cx
                .unsupported("a cast involving 128-bit integers", span);
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
                self.op(
                    format!("call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::Fail({STR_SPAN})"),
                    1,
                    0,
                );
            }
            None => {
                let sig = self.cx.typeck.items.fn_sig(func).clone();
                let name = self.cx.resolve.def(func).name.clone();
                let ret = types::sig_type(self.cx, &sig.output, span);
                let params: Vec<String> = sig
                    .inputs
                    .iter()
                    .map(|t| types::sig_type(self.cx, t, span))
                    .collect();
                let callee = format!(
                    "call {ret} {}::{}({})",
                    types::quote(&self.cx.class_name),
                    types::quote(&name),
                    params.join(", ")
                );
                let returns_value = !(sig.output.is_unit() || sig.output.is_never());
                if returns_value {
                    self.store(destination, span, |this| {
                        for a in args {
                            this.operand(a, span);
                        }
                        this.op(callee, args.len(), 1);
                    });
                } else {
                    for a in args {
                        self.operand(a, span);
                    }
                    self.op(callee, args.len(), 0);
                }
            }
        }
    }
}

fn is_mut_ref(ty: &Ty) -> bool {
    matches!(ty, Ty::Ref(m, _) if m.is_mut())
}
