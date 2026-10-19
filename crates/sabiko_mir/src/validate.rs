//! Structural and type well-formedness checks for MIR.
//!
//! The validator runs after lowering and after every transformation. It never panics on
//! malformed input: every violation becomes a message, so a bug in a pass is reported as
//! a complete list rather than the first symptom. The driver turns a non-empty list into
//! an internal compiler error.

use sabiko_resolve::{Builtin, PrimTy};
use sabiko_types::{is_printable, ConstValue, Mutability, Ty, TypeckResults};

use crate::{
    BasicBlock, BinOp, Body, Constant, LocalKind, Operand, Place, PlaceElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind,
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
            Rvalue::Ref(m, place) => {
                let ty = self.place_ty(bb, place)?;
                Some(Ty::Ref(*m, Box::new(ty)))
            }
            Rvalue::BinaryOp(op, lhs, rhs) => {
                if matches!(op, BinOp::And | BinOp::Or) {
                    self.err(bb, format!("{op:?} must be lowered to a branch"));
                }
                let lhs = self.operand_ty(bb, lhs);
                let _rhs = self.operand_ty(bb, rhs);
                if op.is_comparison() {
                    Some(Ty::BOOL)
                } else {
                    lhs
                }
            }
            Rvalue::Cast(op, ty) => {
                self.operand_ty(bb, op);
                Some(ty.clone())
            }
            Rvalue::Aggregate(def, operands) => {
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
                if !(is_printable(&ty) || ty.is_error() || ty.is_never()) {
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
