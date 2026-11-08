//! CIL code generation: MIR to textual IL for `ilasm`.
//!
//! [`emit_il`] turns the MIR of one source file into one `.il` module. Structs become
//! `sequential` value types; functions become static methods of one static class named
//! after the module, and the functions of a struct's `impl` blocks (methods included)
//! static methods of the struct's own type, the receiver an ordinary first parameter
//! (decision 53); an executable gets an entry-point wrapper that runs `Main` and turns
//! an escaping exception into a Rust-style panic report with exit code 101.
//!
//! The lowering is a direct stack-machine walk over the MIR (see [`func`]): blocks in
//! reverse postorder, locals map to IL locals or arguments (single-use values stay on the
//! evaluation stack, decision 51), places are loaded and stored through their address.
//! Integer arithmetic follows Rust: checked in debug builds, wrapping in release builds.
//! Every check is explicit and its panic carries Rust's message and the source location
//! (decision 43), raised by a throw helper in `Sabiko.Runtime.Checks`.
//!
//! Drops (decision 55): a MIR `Drop` calls the type's `drop` or drop glue (see [`drops`]), and
//! each cleanup scope with something to drop is a protected region (`.try`) over its
//! blocks with a `fault` handler that drops it; functions without drops have no exception
//! handling at all.
//!
//! Verifiability: the output passes `ILVerify` except for `ReturnPtrToStack`, the ECMA-335
//! rule that forbids returning any managed pointer that may point into a frame. That rule
//! rejects every function returning `&T` (C# ref returns are rejected the same way), so
//! the gate ignores exactly that error and the borrow checker guarantees what the verifier
//! cannot: a returned reference never points at the returning frame.

mod drops;
mod enums;
mod func;
mod types;

use std::fmt::Write as _;

use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_mir::Body;
use sabiko_resolve::{DefKind, ResolveResults};
use sabiko_span::{SourceMap, Span};
use sabiko_types::{Ty, TypeckResults};

pub use types::module_class_name;

/// What kind of assembly the IL describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OutputKind {
    /// A library: no entry point. `Main`, if present, is an ordinary method.
    #[default]
    Library,
    /// An executable: requires `fn Main()`; emits the entry-point wrapper.
    Executable,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// The assembly (and module) name, usually the source file's stem.
    pub assembly_name: String,
    pub kind: OutputKind,
    /// Panic on integer overflow (debug) instead of wrapping (release).
    pub overflow_checks: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            assembly_name: "Sabiko".to_owned(),
            kind: OutputKind::Library,
            overflow_checks: true,
        }
    }
}

/// Name of the runtime assembly the output references.
pub const RUNTIME_ASSEMBLY: &str = "Sabiko.Runtime";

/// The exit code of a program whose `Main` panicked (Rust's).
pub const PANIC_EXIT_CODE: i32 = 101;

/// Emits the IL module for `bodies`. Returns `None` after reporting errors for
/// constructs the backend cannot lower yet (every such error is `E0500`/`E0501`).
/// `source_map` gives panics their `file:line:column` locations; the file is named as it
/// was registered (the path given on the command line, as rustc reports it).
#[must_use]
pub fn emit_il(
    bodies: &[Body],
    resolve: &ResolveResults,
    typeck: &TypeckResults,
    source_map: &SourceMap,
    options: &Options,
    sink: &mut DiagnosticSink,
) -> Option<String> {
    let struct_names: Vec<String> = resolve
        .defs
        .iter()
        .filter(|d| d.kind.is_type())
        .map(|d| d.name.clone())
        .collect();
    let class_name = module_class_name(&options.assembly_name, &struct_names);
    let mut cx = Cx {
        resolve,
        typeck,
        source_map,
        options,
        sink,
        class_name,
        had_error: false,
        byref_tuples: Vec::new(),
        inline_arrays: Vec::new(),
        drop_glues: Vec::new(),
        ref_wrappers: [false; 2],
        slices: [false; 2],
    };

    let mut out = String::new();
    cx.header(&mut out);
    cx.structs(bodies, &mut out);
    let _ = writeln!(
        out,
        ".class public abstract auto ansi sealed beforefieldinit {} extends [System.Runtime]System.Object\n{{",
        types::quote(&cx.class_name)
    );
    let mut main: Option<&Body> = None;
    for body in bodies {
        if resolve.def(body.def).parent.is_some() {
            // Emitted with its struct.
            continue;
        }
        if body.name == "Main" {
            main = Some(body);
        }
        func::emit_body(&mut cx, body, &mut out);
    }
    cx.drop_glues(&mut out);
    if options.kind == OutputKind::Executable {
        if let Some(body) = main {
            if body.arg_count != 0 || !body.return_ty().is_unit() {
                cx.error(
                    ErrorCode::INVALID_MAIN,
                    "`Main` must take no parameters and return `()`",
                    body.span,
                    "entry point defined here",
                );
            } else {
                cx.entry_point(&mut out);
            }
        } else if let Some(generic_main) = (0..resolve.defs.len())
            .map(|i| sabiko_resolve::DefId(u32::try_from(i).expect("fewer than 2^32 items")))
            .find(|&d| {
                let info = resolve.def(d);
                info.kind == DefKind::Fn && info.parent.is_none() && info.name == "Main"
            })
        {
            // A generic `Main` has no instance (rustc's E0131).
            cx.error(
                ErrorCode::INVALID_MAIN,
                "`Main` function is not allowed to have generic parameters",
                resolve.def(generic_main).span,
                "`Main` cannot have generic parameters",
            );
        } else {
            cx.sink.emit(Diagnostic::error(
                ErrorCode::INVALID_MAIN,
                "an executable needs a `Main` function",
            ));
            cx.had_error = true;
        }
    }
    out.push_str("}\n");
    cx.byref_tuple_structs(&mut out);
    cx.inline_array_structs(&mut out);
    cx.ref_wrapper_structs(&mut out);
    cx.slice_structs(&mut out);
    if cx.had_error {
        None
    } else {
        Some(out)
    }
}

/// Shared state of one emission.
pub(crate) struct Cx<'a> {
    pub(crate) resolve: &'a ResolveResults,
    pub(crate) typeck: &'a TypeckResults,
    pub(crate) source_map: &'a SourceMap,
    pub(crate) options: &'a Options,
    pub(crate) sink: &'a mut DiagnosticSink,
    /// The static class holding the functions.
    pub(crate) class_name: String,
    pub(crate) had_error: bool,
    /// Tuple types containing references, each lowered to a generated `ref struct`
    /// (decision 52), in order of first use, with their type names.
    byref_tuples: Vec<(Ty, String)>,
    /// Array types, each lowered to a generated inline-array value type (decision 61), in
    /// order of first use, with their type names.
    inline_arrays: Vec<(Ty, String)>,
    /// Types whose drop glue the module needs, in order of first use, with the glue
    /// method's name (decision 55).
    drop_glues: Vec<(Ty, String)>,
    /// Whether the module uses the ``Ref`1`` (index 0) and ``RefMut`1`` (index 1) wrappers
    /// (decision 64).
    ref_wrappers: [bool; 2],
    /// Whether the module uses the ``Slice`1`` (index 0) and ``SliceMut`1`` (index 1)
    /// views over elements that hold references (decision 77).
    slices: [bool; 2],
}

impl Cx<'_> {
    pub(crate) fn error(&mut self, code: ErrorCode, message: &str, span: Span, label: &str) {
        self.had_error = true;
        self.sink
            .emit(Diagnostic::error(code, message).with_label(span, label));
    }

    /// The panic location of `span`, as Rust's `Location` displays it: `file:line:column`,
    /// 1-based, the column being rustc's display column (`SourceFile::rust_column`: a tab
    /// is 4 columns, wide characters 2). `None` for a span without source text.
    pub(crate) fn location(&self, span: Span) -> Option<String> {
        if span.is_dummy() {
            return None;
        }
        let file = self.source_map.get(span.file);
        let line = file.line_col(span.start).line;
        let col = file.rust_column(span.start);
        Some(format!("{}:{line}:{col}", file.name))
    }

    /// Reports a construct the backend does not lower yet.
    pub(crate) fn unsupported(&mut self, what: &str, span: Span) {
        let message = format!("{what} is not yet supported by code generation");
        self.error(
            ErrorCode::UNSUPPORTED_CODEGEN,
            &message,
            span,
            "cannot be compiled yet",
        );
    }

    fn header(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "// Generated by sabikoc from module `{}`.",
            self.options.assembly_name
        );
        out.push_str(".assembly extern System.Runtime { .ver 10:0:0:0 .publickeytoken = (B0 3F 5F 7F 11 D5 0A 3A) }\n");
        let _ = writeln!(
            out,
            ".assembly extern {RUNTIME_ASSEMBLY} {{ .ver 0:1:0:0 }}"
        );
        let _ = writeln!(
            out,
            ".assembly {} {{ .ver 0:0:0:0 }}",
            types::quote(&self.options.assembly_name)
        );
        let _ = writeln!(
            out,
            ".module {}\n",
            types::quote(&format!("{}.dll", self.options.assembly_name))
        );
    }

    /// The name of the generated `ref struct` for `ty`, a tuple type containing a
    /// reference: the tuple type as Sabiko writes it (`(&mut i64, &i64)`), which no
    /// user type can be called. Registers the type for [`Cx::byref_tuple_structs`].
    ///
    /// On first use the element types are lowered at `span`, so an element the backend
    /// cannot lower (`(&&i32, u8)`) is reported where the tuple is used.
    pub(crate) fn byref_tuple(&mut self, ty: &Ty, span: Span) -> String {
        if let Some((_, name)) = self.byref_tuples.iter().find(|(t, _)| t == ty) {
            return name.clone();
        }
        let name = ty.display(self.resolve);
        self.byref_tuples.push((ty.clone(), name.clone()));
        if let Ty::Tuple(elems) = ty {
            for e in elems.iter() {
                types::cil_type(self, e, span);
            }
        }
        name
    }

    /// The name of the generated value type for `ty`, an array type (decision 61): the
    /// array type as Sabiko writes it (`[i32; 3]`), which no user type can be called.
    /// Registers the type for [`Cx::inline_array_structs`]. `None`, after reporting, for
    /// an array longer than an `int32` can count.
    pub(crate) fn inline_array(&mut self, ty: &Ty, span: Span) -> Option<String> {
        if let Some((_, name)) = self.inline_arrays.iter().find(|(t, _)| t == ty) {
            return Some(name.clone());
        }
        let Ty::Array(elem, n) = ty else {
            unreachable!("an inline array of a non-array type")
        };
        if i32::try_from(*n).is_err() {
            self.unsupported("an array of more than 2147483647 elements", span);
            return None;
        }
        let name = ty.display(self.resolve);
        self.inline_arrays.push((ty.clone(), name.clone()));
        types::stored_type(self, elem, span);
        Some(name)
    }

    /// Emits the value type of every array type the module used: `[T; N]` for `N > 0` is
    /// a `sequential` struct with one field of type `T`, `Element`, and
    /// `[InlineArray(N)]`, which makes the runtime lay out `N` of them (C# 12's inline
    /// arrays); `[T; 0]` is an empty struct, since the runtime rejects an inline array of
    /// length 0. An element that holds a reference makes the array a `ref struct`, whose
    /// element is a reference's `Ref<T>` wrapper, a span or another `ref struct`
    /// (decision 77). An element type may register another array, emitted in turn.
    fn inline_array_structs(&mut self, out: &mut String) {
        if self.had_error {
            return;
        }
        let mut next = 0;
        while next < self.inline_arrays.len() {
            let (ty, name) = self.inline_arrays[next].clone();
            next += 1;
            let Ty::Array(elem, n) = &ty else {
                continue;
            };
            let _ = writeln!(
                out,
                "\n.class public sequential ansi sealed beforefieldinit {} extends [System.Runtime]System.ValueType\n{{",
                types::quote(&name)
            );
            if elem.has_ref() {
                let _ = writeln!(out, "  {}", types::IS_BYREF_LIKE_ATTR);
            }
            if *n == 0 {
                out.push_str("  .pack 0\n  .size 1\n}\n");
                continue;
            }
            let bytes: Vec<String> = u32::try_from(*n)
                .expect("checked when registered")
                .to_le_bytes()
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            let _ = writeln!(
                out,
                "  .custom instance void [System.Runtime]System.Runtime.CompilerServices.InlineArrayAttribute::.ctor(int32) = ( 01 00 {} 00 00 )",
                bytes.join(" ")
            );
            let cil = types::stored_type(self, elem, Span::dummy());
            let _ = writeln!(
                out,
                "  .field public {cil} {}\n}}",
                types::ARRAY_ELEMENT_FIELD
            );
        }
    }

    /// Emits the generated `ref struct` of every tuple type with references that the
    /// module used: a field `Item1`.. per element (no `Rest`: the type is ours) and a
    /// constructor taking the elements in order, like `System.ValueTuple`'s, so a tuple is
    /// built the same way whichever type lowers it. An element that is a reference is
    /// held in its `Ref<T>` wrapper, so a borrow of it points to a wrapper as any `&&T`
    /// does (decision 64); the constructor takes the managed pointer and wraps it. An
    /// element type may register another such tuple, which is emitted in turn.
    fn byref_tuple_structs(&mut self, out: &mut String) {
        if self.had_error {
            // Nothing is emitted; an unsupported element was reported where it was used.
            return;
        }
        let mut next = 0;
        while next < self.byref_tuples.len() {
            let (ty, name) = self.byref_tuples[next].clone();
            next += 1;
            let Ty::Tuple(elems) = &ty else {
                continue;
            };
            let span = Span::dummy();
            let class = types::quote(&name);
            let field_tys: Vec<String> = elems
                .iter()
                .map(|e| types::stored_type(self, e, span))
                .collect();
            let param_tys: Vec<String> = elems
                .iter()
                .map(|e| types::cil_type(self, e, span))
                .collect();
            let _ = writeln!(
                out,
                "\n.class public sequential ansi sealed beforefieldinit {class} extends [System.Runtime]System.ValueType\n{{\n  {}",
                types::IS_BYREF_LIKE_ATTR
            );
            for (i, cil) in field_tys.iter().enumerate() {
                let _ = writeln!(out, "  .field public {cil} 'Item{}'", i + 1);
            }
            let params: Vec<String> = param_tys
                .iter()
                .enumerate()
                .map(|(i, cil)| format!("{cil} 'item{}'", i + 1))
                .collect();
            let _ = writeln!(
                out,
                "  .method public hidebysig specialname rtspecialname instance void .ctor({}) cil managed\n  {{\n    .maxstack 2",
                params.join(", ")
            );
            for (i, cil) in field_tys.iter().enumerate() {
                let _ = writeln!(out, "    ldarg.0\n    ldarg {}", i + 1);
                if Self::is_byref(&elems[i]) {
                    let ctor = types::wrapper_ctor(self, &elems[i], span);
                    let _ = writeln!(out, "    newobj {ctor}");
                }
                let _ = writeln!(out, "    stfld {cil} {class}::'Item{}'", i + 1);
            }
            out.push_str("    ret\n  }\n}\n");
        }
    }

    /// Every struct and enum, with the functions of its `impl` blocks as static methods.
    fn structs(&mut self, bodies: &[Body], out: &mut String) {
        let defs: Vec<_> = (0..self.resolve.defs.len())
            .map(|i| sabiko_resolve::DefId(u32::try_from(i).expect("fewer than 2^32 items")))
            .filter(|&d| self.resolve.def(d).kind.is_type())
            .collect();
        for def in defs {
            if self.resolve.def(def).kind == DefKind::Enum {
                self.enum_type(def, bodies, out);
                continue;
            }
            let adt = self.typeck.items.adt(def).clone();
            let _ = writeln!(
                out,
                ".class public sequential ansi sealed beforefieldinit {} extends [System.Runtime]System.ValueType\n{{",
                types::quote(&adt.name)
            );
            if adt.fields.is_empty() {
                // An empty sequential struct needs an explicit size, as C# emits it.
                out.push_str("  .pack 0\n  .size 1\n");
            }
            for field in &adt.fields {
                // A `()` field is a `Sabiko.Runtime.Unit` (decision 52).
                let ty = types::cil_type(self, &field.ty, field.span);
                let _ = writeln!(out, "  .field public {ty} {}", types::quote(&field.name));
            }
            for body in bodies {
                if self.resolve.def(body.def).parent == Some(def) {
                    func::emit_body(self, body, out);
                }
            }
            out.push_str("}\n\n");
        }
    }

    /// `Main` runs inside a `try`; an escaping exception is reported by the runtime and
    /// becomes the exit code. The handler is selected by an exception filter, which runs
    /// before any cleanup handler of the panic (the first pass of .NET's exception
    /// handling): a panic raised while a cleanup handler runs (a `drop` panicking during
    /// unwinding) aborts the process there, as rustc does (decision 55).
    fn entry_point(&self, out: &mut String) {
        let class = types::quote(&self.class_name);
        let _ = write!(
            out,
            "  .method private hidebysig static int32 '<EntryPoint>'() cil managed
  {{
    .entrypoint
    .maxstack 1
    .locals init ([0] int32 'code')
    .try
    {{
      call void {class}::'Main'()
      ldc.i4.0
      stloc.0
      leave.s DONE
    }}
    filter
    {{
      call int32 [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::Filter(object)
      endfilter
    }}
    {{
      castclass [System.Runtime]System.Exception
      call int32 [{RUNTIME_ASSEMBLY}]Sabiko.Runtime.Panic::Unhandled(class [System.Runtime]System.Exception)
      stloc.0
      leave.s DONE
    }}
  DONE:
    ldloc.0
    ret
  }}
"
        );
    }

    /// The class a function is a static method of: its struct's for a function of an
    /// `impl` block, the module class otherwise.
    pub(crate) fn owner_class(&self, def: sabiko_resolve::DefId) -> String {
        match self.resolve.def(def).parent {
            Some(adt) => types::quote(&self.typeck.items.adt(adt).name),
            None => types::quote(&self.class_name),
        }
    }

    /// Whether `ty` is lowered to a CIL managed pointer (`T&`). `&str` and `&[T]` are
    /// spans, not pointers; a reference to `()` is stateless, like `()` (decision 64), and
    /// `&!` has no values.
    pub(crate) fn is_byref(ty: &Ty) -> bool {
        match ty {
            Ty::Ref(_, inner) => {
                !types::is_str(inner) && !types::is_slice(inner) && !types::has_no_value(ty)
            }
            _ => false,
        }
    }

    /// The generated wrapper that holds a reference where the CLR cannot hold a managed
    /// pointer (decision 64): ``Ref`1`` for a shared reference, ``RefMut`1`` for a mutable
    /// one, generic over the referent's storage type. Registers it for
    /// [`Cx::ref_wrapper_structs`].
    pub(crate) fn ref_wrapper(&mut self, mutable: bool) -> &'static str {
        self.ref_wrappers[usize::from(mutable)] = true;
        if mutable {
            types::REF_MUT_CLASS
        } else {
            types::REF_CLASS
        }
    }

    /// Emits the wrappers [`Cx::ref_wrapper`] registered: a `readonly ref struct` with one
    /// `initonly` field, `Value`, a managed pointer to its type parameter (`ref readonly T`
    /// for ``Ref`1``, `ref T` for ``RefMut`1``), and a constructor taking that pointer. The
    /// type parameter is `byreflike` (C#'s `allows ref struct`), so a wrapper can wrap
    /// another (`&&&T`), or a tuple holding references. C# reads such a type but cannot
    /// declare it: a `ref` field to a type that may be a `ref struct` is CS9050. Nothing
    /// writes `Value` after construction; a write to the reference replaces the wrapper.
    fn ref_wrapper_structs(&self, out: &mut String) {
        if self.had_error {
            return;
        }
        for (mutable, class) in [(false, types::REF_CLASS), (true, types::REF_MUT_CLASS)] {
            if !self.ref_wrappers[usize::from(mutable)] {
                continue;
            }
            let readonly_field = if mutable {
                String::new()
            } else {
                format!("\n  {}", types::IS_READONLY_ATTR)
            };
            let _ = write!(
                out,
                "
.class public sequential ansi sealed beforefieldinit {class}<byreflike T> extends [System.Runtime]System.ValueType
{{
  {}
  {}
  .field public initonly !T& 'Value'{readonly_field}
  .method public hidebysig specialname rtspecialname instance void .ctor(!T& 'value') cil managed
  {{
    .maxstack 2
    ldarg.0
    ldarg.1
    stfld !0& valuetype {class}<!T>::'Value'
    ret
  }}
}}
",
                types::IS_BYREF_LIKE_ATTR,
                types::IS_READONLY_ATTR
            );
        }
    }

    /// The generated view over elements that hold references (decision 77): ``Slice`1``
    /// for a shared one, ``SliceMut`1`` for a mutable one, generic over the element's
    /// storage type. `Span<T>` cannot be one: its parameter does not allow `ref struct`s,
    /// and every element that holds a reference is stored as one (a `Ref<T>`, a span, a
    /// tuple or array with references). Registers it for [`Cx::slice_structs`].
    pub(crate) fn slice_view(&mut self, mutable: bool) -> &'static str {
        self.slices[usize::from(mutable)] = true;
        if mutable {
            types::SLICE_MUT_CLASS
        } else {
            types::SLICE_CLASS
        }
    }

    /// Emits the views [`Cx::slice_view`] registered: a `ref struct` with a managed
    /// pointer to the first element and a length, and the members of `Span<T>` that code
    /// generation calls, with the same signatures, so a slice of references is used
    /// exactly as any other span: `get_Length`, `get_Item` (bounds-checked, like the
    /// span's, though every index is checked against rustc's message first), `Slice`,
    /// `Fill`, and the conversion from the mutable view to the shared one. The elements
    /// are reached with `Unsafe.Add`, whose parameter allows `ref struct`s. ``SliceMut`1``
    /// is emitted with the conversion even when only it is used, so ``Slice`1`` is
    /// registered with it.
    fn slice_structs(&mut self, out: &mut String) {
        if self.had_error {
            return;
        }
        if self.slices[1] {
            self.slices[0] = true;
        }
        for (mutable, class) in [(false, types::SLICE_CLASS), (true, types::SLICE_MUT_CLASS)] {
            if self.slices[usize::from(mutable)] {
                out.push_str(&slice_view_class(class, mutable));
            }
        }
    }
}

/// The IL of a view of decision 77 ([`Cx::slice_structs`]): the shared ``Slice`1`` or,
/// `mutable`, ``SliceMut`1`` with [`slice_mut_members`].
fn slice_view_class(class: &str, mutable: bool) -> String {
    let item_ret = if mutable {
        "!T&".to_owned()
    } else {
        format!("!T& {}", types::IN_MODREQ)
    };
    let this = format!("valuetype {class}<!T>");
    let extra = if mutable {
        slice_mut_members(&this)
    } else {
        String::new()
    };
    format!(
        "
.class public sequential ansi sealed beforefieldinit {class}<byreflike T> extends [System.Runtime]System.ValueType
{{
  {byref_like}
  .field private initonly !T& 'start'
  .field private initonly int32 'length'
  .method public hidebysig specialname rtspecialname instance void .ctor(!T& 'start', int32 'length') cil managed
  {{
    .maxstack 2
    ldarg.0
    ldarg.1
    stfld !0& {this}::'start'
    ldarg.0
    ldarg.2
    stfld int32 {this}::'length'
    ret
  }}
  .method public hidebysig specialname instance int32 'get_Length'() cil managed
  {{
    .maxstack 1
    ldarg.0
    ldfld int32 {this}::'length'
    ret
  }}
  .method public hidebysig specialname instance {item_ret} 'get_Item'(int32 'index') cil managed
  {{
    .maxstack 2
    ldarg.1
    ldarg.0
    ldfld int32 {this}::'length'
    blt.un OK
    newobj instance void [System.Runtime]System.IndexOutOfRangeException::.ctor()
    throw
  OK:
    ldarg.0
    ldfld !0& {this}::'start'
    ldarg.1
    call !!0& {unsafe_class}::Add<!T>(!!0&, int32)
    ret
  }}
  .method public hidebysig instance valuetype {class}<!T> 'Slice'(int32 'start', int32 'length') cil managed
  {{
    .maxstack 3
    ldarg.1
    conv.u8
    ldarg.2
    conv.u8
    add
    ldarg.0
    ldfld int32 {this}::'length'
    conv.u8
    ble.un OK
    newobj instance void [System.Runtime]System.ArgumentOutOfRangeException::.ctor()
    throw
  OK:
    ldarg.0
    ldfld !0& {this}::'start'
    ldarg.1
    call !!0& {unsafe_class}::Add<!T>(!!0&, int32)
    ldarg.2
    newobj instance void {this}::.ctor(!0&, int32)
    ret
  }}{extra}
}}
",
        byref_like = types::IS_BYREF_LIKE_ATTR,
        unsafe_class = types::UNSAFE,
    )
}

/// The members only the mutable view has: `Fill(value)`, and the conversion to the shared
/// view (`Span<T>`'s implicit one to `ReadOnlySpan<T>`). `this` is the view's own type.
fn slice_mut_members(this: &str) -> String {
    format!(
        "
  .method public hidebysig instance void 'Fill'(!T 'value') cil managed
  {{
    .maxstack 3
    .locals init ([0] int32 'i')
    br COND
  LOOP:
    ldarg.0
    ldfld !0& {this}::'start'
    ldloc.0
    call !!0& {unsafe_class}::Add<!T>(!!0&, int32)
    ldarg.1
    stobj !T
    ldloc.0
    ldc.i4.1
    add
    stloc.0
  COND:
    ldloc.0
    ldarg.0
    ldfld int32 {this}::'length'
    blt LOOP
    ret
  }}
  .method public hidebysig specialname static valuetype {shared}<!T> 'op_Implicit'({this} 'span') cil managed
  {{
    .maxstack 2
    ldarga.s 0
    ldfld !0& {this}::'start'
    ldarga.s 0
    ldfld int32 {this}::'length'
    newobj instance void valuetype {shared}<!T>::.ctor(!0&, int32)
    ret
  }}",
        shared = types::SLICE_CLASS,
        unsafe_class = types::UNSAFE,
    )
}
