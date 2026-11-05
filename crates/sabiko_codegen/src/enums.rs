//! Enum layout (decision 15): an enum is a `sequential` value type with a tag field holding
//! the variant's discriminant, `int32` unless a discriminant needs `int64`, followed by one
//! field per variant with fields: a nested `sequential` value type of the variant's name
//! holding its fields (a struct variant's by name, a tuple variant's as `Item1`, `Item2`,
//! ... as in a `System.ValueTuple`). Payloads do not overlap yet. So C# reads a
//! `Shape::Rect { W, H }` as `s.Tag` and `s.Rect.W`.

use std::fmt::Write as _;

use sabiko_resolve::{DefId, PrimTy};
use sabiko_span::Span;
use sabiko_types::{AdtDef, Ty, VariantKind};

use crate::{func, types, Cx};

impl Cx<'_> {
    /// The integer type of an enum's tag: `int32`, or `int64` when a discriminant does not
    /// fit in 32 bits.
    pub(crate) fn tag_type(&self, def: DefId) -> PrimTy {
        let adt = self.typeck.items.adt(def);
        if adt
            .variants()
            .iter()
            .all(|v| i32::try_from(v.discr).is_ok())
        {
            PrimTy::I32
        } else {
            PrimTy::I64
        }
    }

    /// The field reference of an enum's tag.
    pub(crate) fn tag_field_ref(&self, def: DefId) -> String {
        let adt = self.typeck.items.adt(def);
        let tag = types::prim_type(self.tag_type(def)).expect("an integer type");
        format!(
            "{tag} {}::{}",
            types::quote(&adt.name),
            types::quote(tag_name(adt))
        )
    }

    /// The field reference of variant `variant`'s payload in its enum.
    pub(crate) fn payload_field_ref(&self, def: DefId, variant: usize) -> String {
        let adt = self.typeck.items.adt(def);
        let v = adt.variant(variant);
        format!(
            "valuetype {} {}::{}",
            payload_class(adt, variant),
            types::quote(&adt.name),
            types::quote(&v.name)
        )
    }

    /// The field reference of field `index` of variant `variant`'s payload.
    pub(crate) fn variant_field_ref(
        &mut self,
        def: DefId,
        variant: usize,
        index: usize,
        field_ty: &Ty,
        span: Span,
    ) -> String {
        let adt = self.typeck.items.adt(def).clone();
        let cil = types::cil_type(self, field_ty, span);
        format!(
            "{cil} {}::{}",
            payload_class(&adt, variant),
            types::quote(&payload_field_name(&adt, variant, index))
        )
    }

    /// The value type of an enum, its payloads nested in it, and the functions of its
    /// `impl` blocks.
    pub(crate) fn enum_type(&mut self, def: DefId, bodies: &[sabiko_mir::Body], out: &mut String) {
        let adt = self.typeck.items.adt(def).clone();
        let _ = writeln!(
            out,
            ".class public sequential ansi sealed beforefieldinit {} extends [System.Runtime]System.ValueType\n{{",
            types::quote(&adt.name)
        );
        for (index, v) in adt.variants().iter().enumerate() {
            if v.fields.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "  .class nested public sequential ansi sealed beforefieldinit {} extends [System.Runtime]System.ValueType\n  {{",
                types::quote(&v.name)
            );
            for (i, f) in v.fields.iter().enumerate() {
                let ty = types::cil_type(self, &f.ty, f.span);
                let _ = writeln!(
                    out,
                    "    .field public {ty} {}",
                    types::quote(&payload_field_name(&adt, index, i))
                );
            }
            out.push_str("  }\n");
        }
        let tag = types::prim_type(self.tag_type(def)).expect("an integer type");
        let _ = writeln!(
            out,
            "  .field public {tag} {}",
            types::quote(tag_name(&adt))
        );
        for (index, v) in adt.variants().iter().enumerate() {
            if v.fields.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "  .field public valuetype {} {}",
                payload_class(&adt, index),
                types::quote(&v.name)
            );
        }
        for body in bodies {
            if self.resolve.def(body.def).parent == Some(def) {
                func::emit_body(self, body, out);
            }
        }
        out.push_str("}\n\n");
    }
}

/// The tag field's name: `Tag`, or `Tag_` when a variant is called `Tag`.
fn tag_name(adt: &AdtDef) -> &'static str {
    if adt.variants().iter().any(|v| v.name == "Tag") {
        "Tag_"
    } else {
        "Tag"
    }
}

/// The nested value type of a variant's payload: `'E'/'V'`.
fn payload_class(adt: &AdtDef, variant: usize) -> String {
    format!(
        "{}/{}",
        types::quote(&adt.name),
        types::quote(&adt.variant(variant).name)
    )
}

/// A payload field's name: a struct variant's own, `Item1`, `Item2`, ... for a tuple
/// variant.
fn payload_field_name(adt: &AdtDef, variant: usize, index: usize) -> String {
    let v = adt.variant(variant);
    match v.kind {
        VariantKind::Tuple => format!("Item{}", index + 1),
        VariantKind::Named | VariantKind::Unit => v.fields[index].name.clone(),
    }
}
