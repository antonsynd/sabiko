//! Type mapping from Sabiko types to CIL type syntax, and identifier helpers.

use std::fmt::Write as _;

use sabiko_resolve::PrimTy;
use sabiko_span::Span;
use sabiko_types::Ty;

use crate::Cx;

/// `ReadOnlySpan<char>`, the lowering of `&str`.
pub(crate) const STR_SPAN: &str = "valuetype [System.Runtime]System.ReadOnlySpan`1<char>";

/// `Span<T>` and `ReadOnlySpan<T>` as classes, the lowering of `&mut [T]` and `&[T]`.
pub(crate) const SPAN_CLASS: &str = "[System.Runtime]System.Span`1";
pub(crate) const READ_ONLY_SPAN_CLASS: &str = "[System.Runtime]System.ReadOnlySpan`1";

/// `System.Runtime.InteropServices.MemoryMarshal`, whose `CreateSpan` and
/// `CreateReadOnlySpan` make a span over the elements of an inline array.
pub(crate) const MEMORY_MARSHAL: &str =
    "[System.Runtime]System.Runtime.InteropServices.MemoryMarshal";

/// The one field of the value type an array `[T; N]` is lowered to (decision 61).
pub(crate) const ARRAY_ELEMENT_FIELD: &str = "'Element'";

/// `System.Int128` and `System.UInt128` as classes (in method references) and as value
/// types (in signatures and locals): the lowering of `i128`/`u128` (decision 42).
pub(crate) const INT128_CLASS: &str = "[System.Runtime]System.Int128";
pub(crate) const UINT128_CLASS: &str = "[System.Runtime]System.UInt128";
pub(crate) const INT128: &str = "valuetype [System.Runtime]System.Int128";
pub(crate) const UINT128: &str = "valuetype [System.Runtime]System.UInt128";

/// `Sabiko.Runtime.Unit`, the storage of a `()` value where one is needed: a parameter,
/// a struct field, a tuple element (decision 52). `-> ()` stays `void`, and locals of type
/// `()` still get no storage (decision 32).
pub(crate) const UNIT: &str = "valuetype [Sabiko.Runtime]Sabiko.Runtime.Unit";

/// The wrappers that hold a shared and a mutable reference where the CLR cannot hold a
/// managed pointer: behind another reference and in a tuple (decision 63). Generated
/// into the module ([`Cx::ref_wrapper`]); C# names them `Ref<T>` and `RefMut<T>`.
pub(crate) const REF_CLASS: &str = "'Ref`1'";
pub(crate) const REF_MUT_CLASS: &str = "'RefMut`1'";

/// `System.ValueTuple`, the lowering of a tuple without references (decision 52).
const VALUE_TUPLE: &str = "[System.Runtime]System.ValueTuple";

/// `System.ValueTuple` holds this many elements directly; an eighth goes into `Rest`, a
/// nested tuple of the remaining elements.
pub(crate) const VALUE_TUPLE_DIRECT: usize = 7;

/// The custom attribute that makes a value type a `ref struct`.
pub(crate) const IS_BYREF_LIKE_ATTR: &str = ".custom instance void [System.Runtime]System.Runtime.CompilerServices.IsByRefLikeAttribute::.ctor() = ( 01 00 00 00 )";

/// The `modreq` that marks a managed pointer as read-only (`in` in C#).
pub(crate) const IN_MODREQ: &str =
    "modreq([System.Runtime]System.Runtime.InteropServices.InAttribute)";

/// The custom attribute C# expects on `in` parameters and `ref readonly` returns.
pub(crate) const IS_READONLY_ATTR: &str = ".custom instance void [System.Runtime]System.Runtime.CompilerServices.IsReadOnlyAttribute::.ctor() = ( 01 00 00 00 )";

/// Quotes an identifier for ilasm. Every user name is quoted so none collides with an IL
/// keyword (`value`, `method`, `add`, ...).
pub(crate) fn quote(name: &str) -> String {
    format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The static class that holds a module's functions: the module name in `PascalCase`, with
/// `Module` appended when a struct already uses that name.
#[must_use]
pub fn module_class_name(assembly_name: &str, struct_names: &[String]) -> String {
    let mut name = String::new();
    let mut upper = true;
    for c in assembly_name.chars() {
        if c.is_alphanumeric() {
            if upper {
                name.extend(c.to_uppercase());
                upper = false;
            } else {
                name.push(c);
            }
        } else {
            upper = true;
        }
    }
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert_str(0, "Module");
    }
    if struct_names.contains(&name) {
        name.push_str("Module");
    }
    name
}

pub(crate) fn is_str(ty: &Ty) -> bool {
    matches!(ty, Ty::Prim(PrimTy::Str))
}

pub(crate) fn is_slice(ty: &Ty) -> bool {
    matches!(ty, Ty::Slice(_))
}

/// `Span<T>` (`mutable`) or `ReadOnlySpan<T>` of the element type `elem`, as a type
/// (with `valuetype`); the class is [`span_class`].
pub(crate) fn span_type(cx: &mut Cx<'_>, elem: &Ty, mutable: bool, span: Span) -> String {
    format!("valuetype {}", span_class(cx, elem, mutable, span))
}

/// `Span<T>` or `ReadOnlySpan<T>` as a class, for member references.
pub(crate) fn span_class(cx: &mut Cx<'_>, elem: &Ty, mutable: bool, span: Span) -> String {
    let class = if mutable {
        SPAN_CLASS
    } else {
        READ_ONLY_SPAN_CLASS
    };
    format!("{class}<{}>", cil_type(cx, elem, span))
}

/// The CIL type of a primitive, or `None` for `str`, which only exists behind a reference.
pub(crate) fn prim_type(p: PrimTy) -> Option<&'static str> {
    Some(match p {
        PrimTy::I8 => "int8",
        PrimTy::I16 => "int16",
        PrimTy::I32 => "int32",
        PrimTy::I64 => "int64",
        PrimTy::Isize => "native int",
        PrimTy::U8 => "uint8",
        PrimTy::U16 => "uint16",
        PrimTy::U32 => "uint32",
        PrimTy::U64 => "uint64",
        PrimTy::Usize => "native uint",
        PrimTy::F32 => "float32",
        PrimTy::F64 => "float64",
        PrimTy::Bool => "bool",
        PrimTy::Char => "char",
        PrimTy::I128 => INT128,
        PrimTy::U128 => UINT128,
        PrimTy::Str => return None,
    })
}

/// The class of a 128-bit integer type, for calls to its operator methods.
pub(crate) fn wide_class(p: PrimTy) -> &'static str {
    debug_assert!(matches!(p, PrimTy::I128 | PrimTy::U128));
    if p == PrimTy::I128 {
        INT128_CLASS
    } else {
        UINT128_CLASS
    }
}

/// The CIL type of `ty` in a local, field, parameter or operand position. Reports and
/// returns a placeholder for unsupported types; `()` and `!` are the caller's business
/// (they have no values).
pub(crate) fn cil_type(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    match ty {
        Ty::Prim(p) => prim_type(*p).map_or_else(
            || {
                let what = format!("the type `{}`", p.name());
                cx.unsupported(&what, span);
                "int32".to_owned()
            },
            str::to_owned,
        ),
        Ty::Adt(def) => format!("valuetype {}", quote(&cx.resolve.def(*def).name)),
        Ty::Ref(_, inner) if is_str(inner) => STR_SPAN.to_owned(),
        Ty::Ref(m, inner) if is_slice(inner) => {
            let Ty::Slice(elem) = &**inner else {
                unreachable!()
            };
            if elem.has_ref() {
                cx.unsupported("a slice of references", span);
                return "int32".to_owned();
            }
            span_type(cx, elem, m.is_mut(), span)
        }
        // A reference to `()` is as stateless as `()` (decision 63).
        Ty::Ref(..) if is_unit_like(ty) => UNIT.to_owned(),
        Ty::Ref(_, inner) if inner.is_uninhabited() => {
            cx.unsupported("a value of type `&!` here", span);
            "int32".to_owned()
        }
        // A managed pointer to the place's storage: a reference held by the place is a
        // `Ref<T>`, since no managed pointer points to another (decision 63).
        Ty::Ref(_, inner) => format!("{}&", stored_type(cx, inner, span)),
        Ty::Unit => UNIT.to_owned(),
        Ty::Tuple(elems) => tuple_type(cx, ty, elems, span),
        Ty::Array(..) => match cx.inline_array(ty, span) {
            Some(name) => format!("valuetype {}", quote(&name)),
            None => "int32".to_owned(),
        },
        Ty::Never => {
            cx.unsupported("a value of type `!` here", span);
            "int32".to_owned()
        }
        Ty::Slice(_) | Ty::FnDef(_) | Ty::Infer(_) | Ty::Error => {
            cx.unsupported("this type", span);
            "int32".to_owned()
        }
    }
}

/// `()`, or a reference to a type like it (`&()`, `&mut &()`): types with one value,
/// which code generation stores nowhere and loads never, as it does `()` (decisions 52 and
/// 63). Where the IL needs a value (a parameter, a tuple element) it is a
/// `Sabiko.Runtime.Unit`; as a return type, `void`.
pub(crate) fn is_unit_like(ty: &Ty) -> bool {
    match ty {
        Ty::Unit => true,
        Ty::Ref(_, inner) => is_unit_like(inner),
        _ => false,
    }
}

/// Whether a value of `ty` is never loaded or stored: [`is_unit_like`] types, and
/// uninhabited ones (`!`, `&!`, decision 44), of which no value exists.
pub(crate) fn has_no_value(ty: &Ty) -> bool {
    is_unit_like(ty) || ty.is_uninhabited()
}

/// The CIL type of a place of type `ty` that a reference points to, or that is a tuple
/// element: the type itself, but for a reference lowered to a managed pointer, which is
/// held in a `Ref<T>` (shared) or `RefMut<T>` (mutable) wrapper, a `ref struct` with one
/// `ref` field, generated into the module (decision 63). The CLR has no pointer to a
/// managed pointer and no managed pointer as a generic argument or in a field of anything
/// but a `ref struct`: the wrapper is all three.
pub(crate) fn stored_type(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    match ty {
        Ty::Ref(m, inner) if Cx::is_byref(ty) => {
            let inner = stored_type(cx, inner, span);
            format!("valuetype {}<{inner}>", cx.ref_wrapper(m.is_mut()))
        }
        _ => cil_type(cx, ty, span),
    }
}

/// The `Value` field of the wrapper [`stored_type`] gives a reference type `ty`, for
/// `ldfld`.
pub(crate) fn wrapper_value_field(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    format!("!0& {}::'Value'", stored_type(cx, ty, span))
}

/// The constructor of the wrapper [`stored_type`] gives a reference type `ty`, for
/// `newobj`: it takes the managed pointer.
pub(crate) fn wrapper_ctor(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    format!("instance void {}::.ctor(!0&)", stored_type(cx, ty, span))
}

/// The CIL type of a tuple: `System.ValueTuple` (nesting the elements past the seventh
/// in `Rest`, as C# does) unless an element is or contains a reference, which no generic
/// argument can be; such a tuple is a `ref struct` generated into the module
/// ([`Cx::byref_tuple`]).
fn tuple_type(cx: &mut Cx<'_>, ty: &Ty, elems: &[Ty], span: Span) -> String {
    if ty.has_ref() {
        return format!("valuetype {}", quote(&cx.byref_tuple(ty, span)));
    }
    value_tuple_type(cx, elems, span)
}

/// The `System.ValueTuple` instantiation (``ValueTuple`2<int32, bool>``, written with its
/// `valuetype` prefix) for the elements of a tuple without references; the same text names
/// the class in member references.
pub(crate) fn value_tuple_type(cx: &mut Cx<'_>, elems: &[Ty], span: Span) -> String {
    let direct = elems.len().min(VALUE_TUPLE_DIRECT);
    let mut args: Vec<String> = elems[..direct]
        .iter()
        .map(|e| cil_type(cx, e, span))
        .collect();
    let arity = if elems.len() > VALUE_TUPLE_DIRECT {
        let rest = Ty::Tuple(elems[VALUE_TUPLE_DIRECT..].to_vec().into());
        args.push(cil_type(cx, &rest, span));
        VALUE_TUPLE_DIRECT + 1
    } else {
        direct
    };
    format!("valuetype {VALUE_TUPLE}`{arity}<{}>", args.join(", "))
}

/// Whether a tuple type is lowered to a generated `ref struct` rather than a
/// `System.ValueTuple`.
pub(crate) fn is_byref_tuple(ty: &Ty) -> bool {
    matches!(ty, Ty::Tuple(_)) && ty.has_ref()
}

/// The CIL type of a parameter in a method signature (definition and call sites alike); a
/// `()` parameter is a `Sabiko.Runtime.Unit`. A shared reference is a plain `T&` here: the
/// definition marks it `[in]` with `IsReadOnlyAttribute`, which is how C# emits an `in`
/// parameter of a non-virtual method. C# puts `modreq(InAttribute)` on `in` parameters of
/// virtual methods only and rejects a static method that has one (CS0570, "not supported
/// by the language"), which the `tests/interop` harness found.
pub(crate) fn param_type(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    cil_type(cx, ty, span)
}

/// The CIL return type in a method signature: `()` and `!` returns are `void`, a shared
/// reference carries the `in` modreq (C#'s `ref readonly` return, which has it on every
/// method).
pub(crate) fn ret_type(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    match ty {
        _ if has_no_value(ty) => "void".to_owned(),
        Ty::Ref(m, _) if !m.is_mut() && Cx::is_byref(ty) => {
            format!("{} {IN_MODREQ}", cil_type(cx, ty, span))
        }
        _ => cil_type(cx, ty, span),
    }
}

/// Opcode suffix for `ldind.*`/`stind.*` on a primitive, or `None` when `ldobj`/`stobj`
/// with the full type is needed.
pub(crate) fn ind_suffix(ty: &Ty) -> Option<&'static str> {
    let p = ty.as_prim()?;
    Some(match p {
        PrimTy::I8 => "i1",
        PrimTy::I16 => "i2",
        PrimTy::I32 => "i4",
        PrimTy::I64 | PrimTy::U64 => "i8",
        PrimTy::Isize | PrimTy::Usize => "i",
        PrimTy::U8 | PrimTy::Bool => "u1",
        PrimTy::U16 | PrimTy::Char => "u2",
        PrimTy::U32 => "u4",
        PrimTy::F32 => "r4",
        PrimTy::F64 => "r8",
        PrimTy::I128 | PrimTy::U128 | PrimTy::Str => return None,
    })
}

/// How values of an integer type sit on the evaluation stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StackWidth {
    /// Narrower than `int32`: results are widened and must be re-narrowed after arithmetic.
    Narrow,
    I32,
    I64,
    Native,
    /// `i128`/`u128`: a `System.Int128`/`System.UInt128` value; every operation is a call.
    Wide,
}

pub(crate) fn stack_width(p: PrimTy) -> StackWidth {
    match p {
        PrimTy::I8 | PrimTy::I16 | PrimTy::U8 | PrimTy::U16 => StackWidth::Narrow,
        PrimTy::I64 | PrimTy::U64 => StackWidth::I64,
        PrimTy::Isize | PrimTy::Usize => StackWidth::Native,
        PrimTy::I128 | PrimTy::U128 => StackWidth::Wide,
        _ => StackWidth::I32,
    }
}

/// `conv.*` suffix that narrows a stack value to the integer type (`i1`, `u2`, ...).
pub(crate) fn conv_suffix(p: PrimTy) -> &'static str {
    match p {
        PrimTy::I8 => "i1",
        PrimTy::I16 => "i2",
        PrimTy::I32 | PrimTy::I128 | PrimTy::U128 | PrimTy::Str => "i4",
        PrimTy::I64 => "i8",
        PrimTy::Isize => "i",
        PrimTy::U8 | PrimTy::Bool => "u1",
        PrimTy::U16 | PrimTy::Char => "u2",
        PrimTy::U32 => "u4",
        PrimTy::U64 => "u8",
        PrimTy::Usize => "u",
        PrimTy::F32 => "r4",
        PrimTy::F64 => "r8",
    }
}

/// The CIL type of the runtime `Print`/`PrintLine` overload for a printable type.
pub(crate) fn print_overload(ty: &Ty) -> Option<&'static str> {
    match ty {
        Ty::Prim(p) => prim_type(*p),
        Ty::Ref(_, inner) if is_str(inner) => Some(STR_SPAN),
        _ => None,
    }
}

/// Escapes a string for an ilasm `ldstr` literal (a QSTRING). Non-ASCII text is passed
/// through: ilasm reads the source as UTF-8.
pub(crate) fn il_string(s: &str) -> String {
    if s.contains('\0') {
        // ilasm ends a quoted string at an escaped NUL (`\000`), so a string holding one is
        // written as its UTF-16 code units, little-endian, which `ldstr` also accepts.
        let mut out = String::from("bytearray (");
        for (i, unit) in s.encode_utf16().enumerate() {
            let [lo, hi] = unit.to_le_bytes();
            let sep = if i == 0 { "" } else { " " };
            let _ = write!(out, "{sep}{lo:02X} {hi:02X}");
        }
        out.push(')');
        return out;
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                // Octal escape, as ilasm's QSTRING grammar allows.
                let _ = write!(out, "\\{:03o}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_name_from_file_stem() {
        assert_eq!(module_class_name("hello", &[]), "Hello");
        assert_eq!(module_class_name("my_prog-v2", &[]), "MyProgV2");
        assert_eq!(module_class_name("7up", &[]), "Module7up");
        assert_eq!(module_class_name("", &[]), "Module");
        assert_eq!(
            module_class_name("point", &["Point".to_owned()]),
            "PointModule"
        );
    }

    #[test]
    fn string_escapes() {
        assert_eq!(il_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
        assert_eq!(il_string("\u{1}"), "\"\\001\"");
        assert_eq!(il_string("héllo"), "\"héllo\"");
        assert_eq!(il_string("a\0é"), "bytearray (61 00 00 00 E9 00)");
    }
}
