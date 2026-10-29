//! Type mapping from Sabiko types to CIL type syntax, and identifier helpers.

use std::fmt::Write as _;

use sabiko_resolve::PrimTy;
use sabiko_span::Span;
use sabiko_types::Ty;

use crate::Cx;

/// `ReadOnlySpan<char>`, the lowering of `&str`.
pub(crate) const STR_SPAN: &str = "valuetype [System.Runtime]System.ReadOnlySpan`1<char>";

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
        Ty::Ref(_, inner) => match &**inner {
            Ty::Ref(..) => {
                cx.unsupported("a reference to a reference", span);
                "int32&".to_owned()
            }
            Ty::Unit | Ty::Never => {
                cx.unsupported("a reference to `()`", span);
                "int32&".to_owned()
            }
            _ => format!("{}&", cil_type(cx, inner, span)),
        },
        Ty::Unit => UNIT.to_owned(),
        Ty::Tuple(elems) => tuple_type(cx, ty, elems, span),
        Ty::Never => {
            cx.unsupported("a value of type `!` here", span);
            "int32".to_owned()
        }
        Ty::FnDef(_) | Ty::Infer(_) | Ty::Error => {
            cx.unsupported("this type", span);
            "int32".to_owned()
        }
    }
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

/// The CIL type of a parameter in a method signature (definition and call sites alike):
/// shared references carry the `in` modreq; a `()` parameter is a `Sabiko.Runtime.Unit`.
pub(crate) fn param_type(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    match ty {
        Ty::Ref(m, inner) if !m.is_mut() && !is_str(inner) => {
            format!("{} {IN_MODREQ}", cil_type(cx, ty, span))
        }
        _ => cil_type(cx, ty, span),
    }
}

/// The CIL return type in a method signature: like [`param_type`], but `()` and `!`
/// returns are `void`.
pub(crate) fn ret_type(cx: &mut Cx<'_>, ty: &Ty, span: Span) -> String {
    match ty {
        Ty::Unit | Ty::Never => "void".to_owned(),
        _ => param_type(cx, ty, span),
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
    }
}
