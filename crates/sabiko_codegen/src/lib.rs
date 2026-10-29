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
//! Verifiability: the output passes `ILVerify` except for `ReturnPtrToStack`, the ECMA-335
//! rule that forbids returning any managed pointer that may point into a frame. That rule
//! rejects every function returning `&T` (C# ref returns are rejected the same way), so
//! the gate ignores exactly that error and the borrow checker guarantees what the verifier
//! cannot: a returned reference never points at the returning frame.

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
        .defs_of_kind(DefKind::Struct)
        .map(|d| resolve.def(d).name.clone())
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

    /// Emits the generated `ref struct` of every tuple type with references that the
    /// module used: a field `Item1`.. per element (no `Rest`: the type is ours) and a
    /// constructor taking the elements in order, like `System.ValueTuple`'s, so a tuple is
    /// built the same way whichever type lowers it. An element type may register another
    /// such tuple, which is emitted in turn.
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
            let params: Vec<String> = field_tys
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
                let _ = writeln!(
                    out,
                    "    ldarg.0\n    ldarg {}\n    stfld {cil} {class}::'Item{}'",
                    i + 1,
                    i + 1
                );
            }
            out.push_str("    ret\n  }\n}\n");
        }
    }

    /// Every struct, with the functions of its `impl` blocks as static methods.
    fn structs(&mut self, bodies: &[Body], out: &mut String) {
        let defs: Vec<_> = self.resolve.defs_of_kind(DefKind::Struct).collect();
        for def in defs {
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
    /// becomes the exit code.
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
    catch [System.Runtime]System.Exception
    {{
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

    /// Whether `ty` is lowered to a CIL managed pointer (`T&`). `&str` is a span, not a
    /// pointer.
    pub(crate) fn is_byref(ty: &Ty) -> bool {
        match ty {
            Ty::Ref(_, inner) => !types::is_str(inner),
            _ => false,
        }
    }
}
