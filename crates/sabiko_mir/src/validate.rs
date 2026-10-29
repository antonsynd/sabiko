//! Structural and type well-formedness checks for MIR.
//!
//! The validator runs after lowering and after every transformation. It never panics on
//! malformed input: every violation becomes a message, so a bug in a pass is reported as
//! a complete list rather than the first symptom. The driver turns a non-empty list into
//! an internal compiler error.

use sabiko_resolve::{Builtin, PrimTy};
use sabiko_types::{is_printable, ConstValue, Mutability, Ty, TypeckResults};

use crate::{
    AggregateKind, BasicBlock, BinOp, Body, Constant, LocalKind, Operand, Place, PlaceElem, Rvalue,
    Statement, StatementKind, Terminator, TerminatorKind,
};

/// Checks one body, returning every violation found.
pub fn validate_body(body: &Body, typeck: &TypeckResults) -> Result<(), Vec<String>> {
    let mut v = Validator {
        body,
        typeck,
        errors: Vec::new(),
    };
    v.run();
    if v.errors.is_empty() {
        Ok(())
    } else {
        Err(v.errors)
    }
}

/// Checks every body; each message is prefixed with the body's name.
#[must_use]
pub fn validate_all(bodies: &[Body], typeck: &TypeckResults) -> Vec<String> {
    bodies
        .iter()
        .flat_map(|body| {
            validate_body(body, typeck)
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(move |m| format!("{}: {m}", body.name))
        })
        .collect()
}

/// Whether a value of type `from` may be written where `to` is expected: the same type, a
/// diverging value, a `&mut T` weakened to `&T`, or an error type on either side.
fn compatible(from: &Ty, to: &Ty) -> bool {
    match (from, to) {
        _ if from == to => true,
        (Ty::Never | Ty::Error, _) | (_, Ty::Error) => true,
        (Ty::Ref(Mutability::Mut, a), Ty::Ref(Mutability::Not, b)) => a == b,
        // A tuple aggregate's elements were coerced one by one (`(&mut a,)` built as an
        // `(&A,)`), and a diverging element leaves a `!` operand in dead code.
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| compatible(x, y))
        }
        _ => false,
    }
}

fn constant_matches(c: &Constant) -> bool {
    match (&c.value, &c.ty) {
        (ConstValue::Int(_), Ty::Prim(p)) => p.is_signed_int(),
        (ConstValue::Uint(_), Ty::Prim(p)) => p.is_unsigned_int(),
        (ConstValue::Float(_), Ty::Prim(p)) => p.is_float(),
        (_, Ty::Error)
        | (ConstValue::Bool(_), Ty::Prim(PrimTy::Bool))
        | (ConstValue::Char(_), Ty::Prim(PrimTy::Char))
        | (ConstValue::Unit, Ty::Unit) => true,
        (ConstValue::Str(_), Ty::Ref(Mutability::Not, inner)) => **inner == Ty::Prim(PrimTy::Str),
        _ => false,
    }
}

struct Validator<'a> {
    body: &'a Body,
    typeck: &'a TypeckResults,
    errors: Vec<String>,
}

impl Validator<'_> {
    fn err(&mut self, bb: BasicBlock, message: impl Into<String>) {
        self.errors.push(format!("bb{}: {}", bb.0, message.into()));
    }

    fn run(&mut self) {
        if self.body.blocks.is_empty() {
            self.errors.push("body has no blocks".to_owned());
        }
        if self.body.arg_count >= self.body.locals.len() {
            self.errors.push(format!(
                "{} locals cannot hold the return place and {} arguments",
                self.body.locals.len(),
                self.body.arg_count
            ));
        }
        for bb in self.body.block_ids() {
            let data = self.body.block(bb);
            for stmt in &data.statements {
                self.statement(bb, stmt);
            }
            match &data.terminator {
                Some(term) => self.terminator(bb, term),
                None => self.err(bb, "block has no terminator"),
            }
        }
        self.two_phase_borrows();
    }

    /// A two-phase borrow is assigned to a temporary that nothing but one call reads
    /// (`move _t` among its arguments): the borrow checker activates it at that use.
    fn two_phase_borrows(&mut self) {
        let mut uses = vec![0usize; self.body.locals.len()];
        let mut call_uses = vec![0usize; self.body.locals.len()];
        let mut reservations = Vec::new();
        let count = |place: &Place, uses: &mut Vec<usize>| {
            if let Some(n) = uses.get_mut(place.local.index()) {
                *n += 1;
            }
        };
        for bb in self.body.block_ids() {
            let data = self.body.block(bb);
            for stmt in &data.statements {
                let StatementKind::Assign(dest, rvalue) = &stmt.kind else {
                    continue;
                };
                if let Rvalue::Ref(kind, _) = rvalue {
                    if kind.is_two_phase() {
                        reservations.push((bb, dest.clone()));
                    }
                }
                if !dest.is_local() {
                    count(dest, &mut uses);
                }
                for place in rvalue_places(rvalue) {
                    count(place, &mut uses);
                }
            }
            let Some(term) = &data.terminator else {
                continue;
            };
            match &term.kind {
                TerminatorKind::Call {
                    args, destination, ..
                } => {
                    for arg in args {
                        if let Operand::Move(p) = arg {
                            if p.is_local() {
                                count(p, &mut call_uses);
                            }
                        }
                        if let Some(p) = arg.place() {
                            count(p, &mut uses);
                        }
                    }
                    if !destination.is_local() {
                        count(destination, &mut uses);
                    }
                }
                TerminatorKind::SwitchInt { discr, .. } => {
                    if let Some(p) = discr.place() {
                        count(p, &mut uses);
                    }
                }
                TerminatorKind::Goto { .. }
                | TerminatorKind::Return
                | TerminatorKind::Unreachable => {}
            }
        }
        for (bb, dest) in reservations {
            let local = dest.local.index();
            let is_temp = self
                .body
                .locals
                .get(local)
                .is_some_and(|d| d.kind == LocalKind::Temp);
            if !dest.is_local() || !is_temp {
                self.err(
                    bb,
                    format!("two-phase borrow into {dest:?}, not a temporary"),
                );
            } else if uses[local] != 1 || call_uses[local] != 1 {
                self.err(
                    bb,
                    format!(
                        "two-phase borrow into _{local}, which is used {} times, {} of them as a moved call argument",
                        uses[local], call_uses[local]
                    ),
                );
            }
        }
    }

    fn check_target(&mut self, bb: BasicBlock, target: BasicBlock) {
        if target.index() >= self.body.blocks.len() {
            self.err(bb, format!("successor bb{} does not exist", target.0));
        }
    }

    /// The type of a place, or `None` after reporting a malformed projection.
    fn place_ty(&mut self, bb: BasicBlock, place: &Place) -> Option<Ty> {
        let Some(decl) = self.body.locals.get(place.local.index()) else {
            self.err(bb, format!("local _{} does not exist", place.local.0));
            return None;
        };
        let mut ty = decl.ty.clone();
        for elem in &place.projection {
            ty = match (elem, ty) {
                (PlaceElem::Deref, Ty::Ref(_, inner)) => *inner,
                (PlaceElem::Deref, other) => {
                    self.err(bb, format!("deref of non-reference type {other:?}"));
                    return None;
                }
                (PlaceElem::Field(index, field_ty), Ty::Adt(def)) => {
                    let adt = self.typeck.items.adt(def);
                    match adt.fields.get(*index) {
                        Some(f) if f.ty == *field_ty => field_ty.clone(),
                        Some(f) => {
                            self.err(
                                bb,
                                format!(
                                    "field {index} of `{}` has type {:?}, projection says {field_ty:?}",
                                    adt.name, f.ty
                                ),
                            );
                            return None;
                        }
                        None => {
                            self.err(
                                bb,
                                format!("field index {index} out of range for `{}`", adt.name),
                            );
                            return None;
                        }
                    }
                }
                (PlaceElem::Field(index, field_ty), Ty::Tuple(elems)) => match elems.get(*index) {
                    Some(t) if t == field_ty => field_ty.clone(),
                    Some(t) => {
                        self.err(
                            bb,
                            format!(
                                "element {index} of a tuple has type {t:?}, projection says {field_ty:?}"
                            ),
                        );
                        return None;
                    }
                    None => {
                        self.err(
                            bb,
                            format!(
                                "element index {index} out of range for a tuple of {}",
                                elems.len()
                            ),
                        );
                        return None;
                    }
                },
                (PlaceElem::Field(index, _), other) => {
                    self.err(bb, format!("field {index} of non-struct type {other:?}"));
                    return None;
                }
            };
        }
        Some(ty)
    }

    fn operand_ty(&mut self, bb: BasicBlock, operand: &Operand) -> Option<Ty> {
        match operand {
            Operand::Copy(place) => {
                let ty = self.place_ty(bb, place)?;
                if !self.typeck.items.is_copy(&ty) {
                    self.err(bb, format!("copy of non-Copy type {ty:?}"));
                }
                Some(ty)
            }
            Operand::Move(place) => {
                let ty = self.place_ty(bb, place)?;
                if self.typeck.items.is_copy(&ty) {
                    self.err(bb, format!("move of Copy type {ty:?}"));
                }
                Some(ty)
            }
            Operand::Constant(c) => {
                if !constant_matches(c) {
                    self.err(
                        bb,
                        format!("constant {:?} does not match its type {:?}", c.value, c.ty),
                    );
                }
                Some(c.ty.clone())
            }
        }
    }

    fn rvalue_ty(&mut self, bb: BasicBlock, rvalue: &Rvalue) -> Option<Ty> {
        match rvalue {
            Rvalue::Use(op) | Rvalue::UnaryOp(_, op) => self.operand_ty(bb, op),
            Rvalue::Ref(kind, place) => {
                let ty = self.place_ty(bb, place)?;
                Some(Ty::Ref(kind.mutability(), Box::new(ty)))
            }
            Rvalue::BinaryOp(op, lhs, rhs) => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    self.err(bb, format!("{op:?} must be lowered to a branch"));
                }
                let lhs = self.operand_ty(bb, lhs);
                let rhs = self.operand_ty(bb, rhs);
                if op.is_comparison() {
                    // A comparison of references compares the referents: MIR building
                    // dereferences the operands, so a reference here would compare
                    // addresses.
                    let is_ref = |t: &Option<Ty>| matches!(t, Some(Ty::Ref(..)));
                    let diverges = |t: &Option<Ty>| matches!(t, Some(Ty::Never | Ty::Error));
                    if (is_ref(&lhs) || is_ref(&rhs)) && !(diverges(&lhs) || diverges(&rhs)) {
                        self.err(bb, format!("{op:?} of reference operands {lhs:?}, {rhs:?}"));
                    }
                    Some(Ty::BOOL)
                } else {
                    lhs
                }
            }
            Rvalue::Cast(op, ty) => {
                self.operand_ty(bb, op);
                Some(ty.clone())
            }
            Rvalue::Aggregate(AggregateKind::Tuple, operands) => {
                if operands.is_empty() {
                    self.err(bb, "tuple aggregate without elements (`()` is a constant)");
                }
                let mut tys = Vec::with_capacity(operands.len());
                for op in operands {
                    tys.push(self.operand_ty(bb, op).unwrap_or(Ty::Error));
                }
                Some(Ty::Tuple(tys.into()))
            }
            Rvalue::Aggregate(AggregateKind::Adt(def), operands) => {
                let adt = self.typeck.items.adt(*def);
                if operands.len() != adt.fields.len() {
                    self.err(
                        bb,
                        format!(
                            "aggregate of `{}` has {} operands for {} fields",
                            adt.name,
                            operands.len(),
                            adt.fields.len()
                        ),
                    );
                }
                let field_tys: Vec<Ty> = adt.fields.iter().map(|f| f.ty.clone()).collect();
                for (op, field_ty) in operands.iter().zip(&field_tys) {
                    if let Some(ty) = self.operand_ty(bb, op) {
                        if !compatible(&ty, field_ty) {
                            self.err(
                                bb,
                                format!(
                                    "aggregate field of type {field_ty:?} initialised with {ty:?}"
                                ),
                            );
                        }
                    }
                }
                Some(Ty::Adt(*def))
            }
        }
    }

    fn statement(&mut self, bb: BasicBlock, stmt: &Statement) {
        match &stmt.kind {
            StatementKind::Assign(place, rvalue) => {
                let place_ty = self.place_ty(bb, place);
                let rvalue_ty = self.rvalue_ty(bb, rvalue);
                if let (Some(p), Some(r)) = (place_ty, rvalue_ty) {
                    if !compatible(&r, &p) {
                        self.err(bb, format!("assignment of {r:?} to a place of type {p:?}"));
                    }
                }
            }
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                match self.body.locals.get(local.index()) {
                    Some(decl) if matches!(decl.kind, LocalKind::User | LocalKind::Temp) => {}
                    Some(decl) => self.err(
                        bb,
                        format!("storage marker on {:?} local _{}", decl.kind, local.0),
                    ),
                    None => self.err(bb, format!("local _{} does not exist", local.0)),
                }
            }
            StatementKind::Nop => {}
        }
    }

    fn terminator(&mut self, bb: BasicBlock, term: &Terminator) {
        match &term.kind {
            TerminatorKind::Goto { target } => self.check_target(bb, *target),
            TerminatorKind::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                if let Some(ty) = self.operand_ty(bb, discr) {
                    if !(ty.is_bool() || ty.is_integer() || ty.is_error()) {
                        self.err(bb, format!("switchInt on non-integer type {ty:?}"));
                    }
                }
                for (_, target) in targets {
                    self.check_target(bb, *target);
                }
                self.check_target(bb, *otherwise);
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
            } => {
                if let Some(builtin @ (Builtin::Print | Builtin::PrintLine)) =
                    self.typeck.items.builtin(*func)
                {
                    self.print_call(bb, builtin, args, destination, *target);
                    return;
                }
                let sig = self.typeck.items.fn_sig(*func).clone();
                if sig.inputs.len() != args.len() {
                    self.err(
                        bb,
                        format!(
                            "call passes {} arguments to a function taking {}",
                            args.len(),
                            sig.inputs.len()
                        ),
                    );
                }
                for (arg, input) in args.iter().zip(&sig.inputs) {
                    if let Some(ty) = self.operand_ty(bb, arg) {
                        if !compatible(&ty, input) {
                            self.err(
                                bb,
                                format!("call argument of type {ty:?} where {input:?} is expected"),
                            );
                        }
                    }
                }
                if let Some(dest_ty) = self.place_ty(bb, destination) {
                    if !compatible(&sig.output, &dest_ty) {
                        self.err(
                            bb,
                            format!(
                                "call returning {:?} into a place of type {dest_ty:?}",
                                sig.output
                            ),
                        );
                    }
                }
                if let Some(target) = target {
                    self.check_target(bb, *target);
                }
            }
            TerminatorKind::Return | TerminatorKind::Unreachable => {}
        }
    }

    /// `Print`/`PrintLine`: overloaded over the printable types, so there is no signature
    /// to compare with; one printable argument, a `()` destination.
    fn print_call(
        &mut self,
        bb: BasicBlock,
        builtin: Builtin,
        args: &[Operand],
        destination: &Place,
        target: Option<BasicBlock>,
    ) {
        if args.len() != 1 {
            self.err(
                bb,
                format!(
                    "call passes {} arguments to `{}`, which takes 1",
                    args.len(),
                    builtin.name()
                ),
            );
        }
        for arg in args {
            if let Some(ty) = self.operand_ty(bb, arg) {
                if !(is_printable(&ty) || ty.is_error() || ty.is_uninhabited()) {
                    self.err(
                        bb,
                        format!(
                            "`{}` called with a non-printable argument of type {ty:?}",
                            builtin.name()
                        ),
                    );
                }
            }
        }
        if let Some(dest_ty) = self.place_ty(bb, destination) {
            if !compatible(&Ty::Unit, &dest_ty) {
                self.err(
                    bb,
                    format!(
                        "`{}` returning () into a place of type {dest_ty:?}",
                        builtin.name()
                    ),
                );
            }
        }
        if let Some(target) = target {
            self.check_target(bb, target);
        }
    }
}

/// The places an rvalue reads or borrows.
fn rvalue_places(rvalue: &Rvalue) -> Vec<&Place> {
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast(op, _) => {
            op.place().into_iter().collect()
        }
        Rvalue::BinaryOp(_, l, r) => l.place().into_iter().chain(r.place()).collect(),
        Rvalue::Aggregate(_, ops) => ops.iter().filter_map(Operand::place).collect(),
        Rvalue::Ref(_, place) => vec![place],
    }
}
