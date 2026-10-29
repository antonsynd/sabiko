//! Human-readable MIR, in the style of rustc's `--emit=mir`.
//!
//! ```text
//! fn Add(_1: i32, _2: i32) -> i32 {
//!     let mut _0: i32;
//!     let _3: i32;                         // sum
//!
//!     bb0: {
//!         StorageLive(_3);
//!         _3 = Add(copy _1, copy _2);
//!         _0 = copy _3;
//!         StorageDead(_3);
//!         return;
//!     }
//! }
//! ```

use std::fmt::Write as _;

use sabiko_resolve::ResolveResults;
use sabiko_types::TypeckResults;

use crate::{
    AggregateKind, Body, Local, Operand, Place, PlaceElem, Rvalue, StatementKind, TerminatorKind,
};

/// Prints every body, separated by blank lines.
#[must_use]
pub fn print_bodies(bodies: &[Body], resolve: &ResolveResults, typeck: &TypeckResults) -> String {
    let mut out = String::new();
    for (i, body) in bodies.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        print_body(body, resolve, typeck, &mut out);
    }
    out
}

pub fn print_body(body: &Body, resolve: &ResolveResults, typeck: &TypeckResults, out: &mut String) {
    let p = Printer {
        body,
        resolve,
        typeck,
    };
    let params: Vec<String> = body
        .args()
        .map(|l| format!("_{}: {}", l.0, p.ty(&body.local_decl(l).ty)))
        .collect();
    let ret = body.return_ty();
    let _ = write!(
        out,
        "fn {}({})",
        resolve.qualified_name(body.def),
        params.join(", ")
    );
    if !ret.is_unit() {
        let _ = write!(out, " -> {}", p.ty(ret));
    }
    out.push_str(" {\n");
    // The return place is declared first (rustc order), then the non-argument locals.
    let mut decls: Vec<(usize, &crate::LocalDecl)> = vec![(0, &body.locals[0])];
    decls.extend(body.locals.iter().enumerate().skip(body.arg_count + 1));
    for (i, decl) in decls {
        let line = format!(
            "    let {}_{i}: {};",
            decl.mutability.prefix_str(),
            p.ty(&decl.ty)
        );
        match &decl.name {
            Some(name) => {
                let _ = writeln!(out, "{line:<40} // {name}");
            }
            None => {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    for bb in body.block_ids() {
        let data = body.block(bb);
        let _ = write!(out, "\n    bb{}: {{\n", bb.0);
        for stmt in &data.statements {
            let _ = writeln!(out, "        {};", p.stmt(&stmt.kind));
        }
        if let Some(term) = &data.terminator {
            let _ = writeln!(out, "        {};", p.terminator(&term.kind));
        } else {
            out.push_str("        <no terminator>;\n");
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
}

struct Printer<'a> {
    body: &'a Body,
    resolve: &'a ResolveResults,
    typeck: &'a TypeckResults,
}

impl Printer<'_> {
    fn ty(&self, ty: &sabiko_types::Ty) -> String {
        ty.display(self.resolve)
    }

    fn place(&self, place: &Place) -> String {
        let mut s = format!("_{}", place.local.0);
        let mut ty = self.body.local_decl(place.local).ty.clone();
        for elem in &place.projection {
            match elem {
                PlaceElem::Deref => {
                    s = format!("(*{s})");
                    ty = match ty {
                        sabiko_types::Ty::Ref(_, inner) => *inner,
                        other => other,
                    };
                }
                PlaceElem::Field(index, field_ty) => {
                    let name = match &ty {
                        sabiko_types::Ty::Adt(def) => self
                            .typeck
                            .items
                            .adt(*def)
                            .fields
                            .get(*index)
                            .map_or_else(|| index.to_string(), |f| f.name.clone()),
                        _ => index.to_string(),
                    };
                    let _ = write!(s, ".{name}");
                    ty = field_ty.clone();
                }
            }
        }
        s
    }

    fn operand(&self, op: &Operand) -> String {
        match op {
            Operand::Copy(p) => format!("copy {}", self.place(p)),
            Operand::Move(p) => format!("move {}", self.place(p)),
            Operand::Constant(c) => format!("const {}", c.value.render(&c.ty, self.resolve)),
        }
    }

    fn rvalue(&self, rv: &Rvalue) -> String {
        match rv {
            Rvalue::Use(op) => self.operand(op),
            Rvalue::Ref(kind, place) => {
                let two_phase = if kind.is_two_phase() {
                    "two_phase "
                } else {
                    ""
                };
                format!(
                    "&{two_phase}{}{}",
                    kind.mutability().prefix_str(),
                    self.place(place)
                )
            }
            Rvalue::UnaryOp(op, operand) => {
                format!("{op:?}({})", self.operand(operand))
            }
            Rvalue::BinaryOp(op, lhs, rhs) => {
                format!("{op:?}({}, {})", self.operand(lhs), self.operand(rhs))
            }
            Rvalue::Cast(op, ty) => format!("{} as {}", self.operand(op), self.ty(ty)),
            Rvalue::Aggregate(AggregateKind::Tuple, elems) => {
                let ops: Vec<String> = elems.iter().map(|op| self.operand(op)).collect();
                if ops.len() == 1 {
                    format!("({},)", ops[0])
                } else {
                    format!("({})", ops.join(", "))
                }
            }
            Rvalue::Aggregate(AggregateKind::Adt(def), fields) => {
                let adt = self.typeck.items.adt(*def);
                if fields.is_empty() {
                    return adt.name.clone();
                }
                let inits: Vec<String> = fields
                    .iter()
                    .enumerate()
                    .map(|(i, op)| {
                        let name = adt
                            .fields
                            .get(i)
                            .map_or_else(|| i.to_string(), |f| f.name.clone());
                        format!("{name}: {}", self.operand(op))
                    })
                    .collect();
                format!("{} {{ {} }}", adt.name, inits.join(", "))
            }
        }
    }

    fn stmt(&self, kind: &StatementKind) -> String {
        match kind {
            StatementKind::Assign(place, rv) => {
                format!("{} = {}", self.place(place), self.rvalue(rv))
            }
            StatementKind::StorageLive(l) => format!("StorageLive(_{})", l.0),
            StatementKind::StorageDead(l) => format!("StorageDead(_{})", l.0),
            StatementKind::Nop => "nop".to_owned(),
        }
    }

    fn terminator(&self, kind: &TerminatorKind) -> String {
        match kind {
            TerminatorKind::Goto { target } => format!("goto -> bb{}", target.0),
            TerminatorKind::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                let mut arms: Vec<String> = targets
                    .iter()
                    .map(|(v, bb)| format!("{v}: bb{}", bb.0))
                    .collect();
                arms.push(format!("otherwise: bb{}", otherwise.0));
                format!(
                    "switchInt({}) -> [{}]",
                    self.operand(discr),
                    arms.join(", ")
                )
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
            } => {
                let args: Vec<String> = args.iter().map(|a| self.operand(a)).collect();
                let target = match target {
                    Some(bb) => format!("bb{}", bb.0),
                    None => "unreachable".to_owned(),
                };
                format!(
                    "{} = {}({}) -> {target}",
                    self.place(destination),
                    self.resolve.qualified_name(*func),
                    args.join(", ")
                )
            }
            TerminatorKind::Return => "return".to_owned(),
            TerminatorKind::Unreachable => "unreachable".to_owned(),
        }
    }
}

/// `_N` for a local, as used in diagnostics.
#[must_use]
pub fn local_name(local: Local) -> String {
    format!("_{}", local.0)
}
