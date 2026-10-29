//! Drop glue (decision 55): one static method per type with drop glue, `'<Drop>T'(T&)` in
//! the module class, which drops a value in place. A struct whose only glue is its own
//! `drop` (no field needs dropping) gets none: dropping it calls that `drop` directly.
//!
//! The glue of a struct with its own `impl Drop` first calls that `drop` (the method
//! `'Drop.drop'` of the struct, a `&mut self` method like any other), with the fields
//! intact, then drops the fields with drop glue in declaration order; a struct without
//! one, or a tuple, only drops its fields (elements) in order. Each step but the last is
//! a protected region whose `fault` handler drops the fields after it, so a panic in one
//! step (a `drop` that panics) still drops the rest, as rustc's drop glue does. Like every
//! cleanup handler, it first calls `Sabiko.Runtime.Panic::BeginCleanup`: a panic while a
//! handler runs aborts the process (see `Panic.Filter`).

use std::fmt::Write as _;

use sabiko_span::Span;
use sabiko_types::Ty;

use crate::{types, Cx, RUNTIME_ASSEMBLY};

/// The name the `drop` method of an `impl Drop` gets in its struct: not `drop`, so that it
/// never collides with an inherent method of that name, and not callable from C# by name.
pub(crate) const DROP_METHOD: &str = "Drop.drop";

impl Cx<'_> {
    /// The `call` instruction that drops a value of type `ty` (which must have drop glue)
    /// in place, given its address: its own `drop` when nothing else needs dropping, else
    /// its glue, registered for [`Cx::drop_glues`].
    pub(crate) fn drop_glue_call(&mut self, ty: &Ty, span: Span) -> String {
        if let Ty::Adt(def) = ty {
            let items = &self.typeck.items;
            if items.drop_impl(*def).is_some()
                && !items
                    .adt(*def)
                    .fields
                    .iter()
                    .any(|f| items.needs_drop(&f.ty))
            {
                return self.own_drop_call(*def, ty, span);
            }
        }
        let name = format!("<Drop>{}", ty.display(self.resolve));
        if !self.drop_glues.iter().any(|(t, _)| t == ty) {
            self.drop_glues.push((ty.clone(), name.clone()));
        }
        let cil = types::cil_type(self, ty, span);
        format!(
            "call void {}::{}({cil}&)",
            types::quote(&self.class_name),
            types::quote(&name)
        )
    }

    /// Emits the glue of every type registered so far, and of the types their glue
    /// registers in turn.
    pub(crate) fn drop_glues(&mut self, out: &mut String) {
        let mut next = 0;
        while next < self.drop_glues.len() {
            let (ty, name) = self.drop_glues[next].clone();
            next += 1;
            self.drop_glue(&ty, &name, out);
        }
    }

    fn drop_glue(&mut self, ty: &Ty, name: &str, out: &mut String) {
        let span = Span::dummy();
        let cil = types::cil_type(self, ty, span);
        // The own `drop`, then each field with drop glue, as instruction sequences.
        let mut steps: Vec<Vec<String>> = Vec::new();
        if let Ty::Adt(def) = ty {
            if self.typeck.items.drop_impl(*def).is_some() {
                steps.push(vec![
                    "ldarg.0".to_owned(),
                    self.own_drop_call(*def, ty, span),
                ]);
            }
        }
        let own = steps.len();
        let field_tys: Vec<Ty> = match ty {
            Ty::Adt(def) => self
                .typeck
                .items
                .adt(*def)
                .fields
                .iter()
                .map(|f| f.ty.clone())
                .collect(),
            Ty::Tuple(elems) => elems.to_vec(),
            _ => Vec::new(),
        };
        for (index, field_ty) in field_tys.iter().enumerate() {
            if !self.typeck.items.needs_drop(field_ty) {
                continue;
            }
            let mut step = vec!["ldarg.0".to_owned()];
            let (hops, field) = self.field_path(ty, index, field_ty, span);
            for hop in hops {
                step.push(format!("ldflda {hop}"));
            }
            step.push(format!("ldflda {field}"));
            step.push(self.drop_glue_call(field_ty, span));
            steps.push(step);
        }
        let _ = writeln!(
            out,
            "  .method public hidebysig static void {}({cil}& 'self') cil managed\n  {{\n    .maxstack 2",
            types::quote(name)
        );
        for (k, step) in steps.iter().enumerate() {
            // The fields after this step: all of them after the own `drop`.
            let rest = &steps[(k + 1).max(own)..];
            if rest.is_empty() {
                for line in step {
                    let _ = writeln!(out, "    {line}");
                }
                continue;
            }
            out.push_str("    .try\n    {\n");
            for line in step {
                let _ = writeln!(out, "      {line}");
            }
            let _ = writeln!(out, "      leave STEP{k}\n    }}\n    fault\n    {{");
            let _ = writeln!(
                out,
                "      call void [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::BeginCleanup()"
            );
            for later in rest {
                for line in later {
                    let _ = writeln!(out, "      {line}");
                }
            }
            let _ = writeln!(out, "      endfault\n    }}\n  STEP{k}:");
        }
        out.push_str("    ret\n  }\n\n");
    }

    /// The `call` instruction of the `drop` of the `impl Drop` of the struct `def`.
    fn own_drop_call(&mut self, def: sabiko_resolve::DefId, ty: &Ty, span: Span) -> String {
        let cil = types::cil_type(self, ty, span);
        format!(
            "call void {}::{}({cil}&)",
            types::quote(&self.typeck.items.adt(def).name),
            types::quote(DROP_METHOD)
        )
    }

    /// The field reference of field (element) `index` of a value of type `base_ty`, after
    /// the `Rest` fields a tuple element past the seventh goes through first.
    pub(crate) fn field_path(
        &mut self,
        base_ty: &Ty,
        index: usize,
        field_ty: &Ty,
        span: Span,
    ) -> (Vec<String>, String) {
        match base_ty {
            Ty::Tuple(_) => self.tuple_field_ref(base_ty, index, span),
            _ => (
                Vec::new(),
                self.struct_field_ref(base_ty, index, field_ty, span),
            ),
        }
    }

    /// The struct a field projection applies to, and the field's name and CIL type.
    pub(crate) fn struct_field_ref(
        &mut self,
        base_ty: &Ty,
        index: usize,
        field_ty: &Ty,
        span: Span,
    ) -> String {
        let Ty::Adt(def) = base_ty else {
            panic!("field projection on non-struct {base_ty:?}");
        };
        let adt = self.typeck.items.adt(*def);
        let struct_name = types::quote(&adt.name);
        let field_name = types::quote(&adt.fields[index].name);
        let cil = types::cil_type(self, field_ty, span);
        format!("{cil} {struct_name}::{field_name}")
    }

    /// The field reference of element `index` of a tuple of type `base_ty`, after the
    /// `Rest` fields to go through first (a `System.ValueTuple` holds seven elements; `t.7`
    /// is `t.Rest.Item1`, `t.15` is `t.Rest.Rest.Item2`). The fields of a generic
    /// instantiation are referenced by their declared type, the type parameter (`!0`).
    pub(crate) fn tuple_field_ref(
        &mut self,
        base_ty: &Ty,
        index: usize,
        span: Span,
    ) -> (Vec<String>, String) {
        let Ty::Tuple(elems) = base_ty else {
            panic!("element projection on non-tuple {base_ty:?}");
        };
        if types::is_byref_tuple(base_ty) {
            let class = types::quote(&self.byref_tuple(base_ty, span));
            let cil = types::cil_type(self, &elems[index], span);
            return (Vec::new(), format!("{cil} {class}::'Item{}'", index + 1));
        }
        let mut rest = Vec::new();
        let mut elems: &[Ty] = elems;
        let mut index = index;
        while index >= types::VALUE_TUPLE_DIRECT {
            let class = types::value_tuple_type(self, elems, span);
            rest.push(format!("!{} {class}::Rest", types::VALUE_TUPLE_DIRECT));
            elems = &elems[types::VALUE_TUPLE_DIRECT..];
            index -= types::VALUE_TUPLE_DIRECT;
        }
        let class = types::value_tuple_type(self, elems, span);
        (rest, format!("!{index} {class}::Item{}", index + 1))
    }
}
