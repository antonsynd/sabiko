//! Evaluation of `const` item initialisers.
//!
//! Phase 2 constants are literals, arithmetic, comparisons, casts and references to other
//! constants. Evaluation follows Rust's compile-time semantics: overflow, division by zero
//! and over-wide shifts are errors, casts wrap or saturate like `as`.

// Two's-complement bit manipulation is the point of this module.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::{HashMap, HashSet};

use sabiko_ast::{BinOp, Expr, ExprKind, Lit, UnOp};
use sabiko_diagnostics::{Diagnostic, DiagnosticSink, ErrorCode};
use sabiko_resolve::{DefId, DefKind, PrimTy, Res, ResolveResults};
use sabiko_span::Span;

use crate::check::BodyTypes;
use crate::ty::Ty;

/// A compile-time value. Signed integers use `Int`, unsigned `Uint`; the type is carried
/// alongside wherever the value is used.
#[derive(Clone, Debug, PartialEq)]
pub enum ConstValue {
    Int(i128),
    Uint(u128),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(String),
    Unit,
}

impl ConstValue {
    /// Renders the value the way the MIR printer shows constants: `1_i32`, `true`, `"s"`.
    #[must_use]
    pub fn render(&self, ty: &Ty, resolve: &ResolveResults) -> String {
        match self {
            ConstValue::Int(v) => format!("{v}_{}", ty.display(resolve)),
            ConstValue::Uint(v) => format!("{v}_{}", ty.display(resolve)),
            ConstValue::Float(v) => {
                let mut s = format!("{v:?}");
                if !s.contains(['.', 'e', 'N', 'i']) {
                    s.push_str(".0");
                }
                format!("{s}_{}", ty.display(resolve))
            }
            ConstValue::Bool(b) => b.to_string(),
            ConstValue::Char(c) => format!("{c:?}"),
            ConstValue::Str(s) => format!("{s:?}"),
            ConstValue::Unit => "()".to_owned(),
        }
    }
}

struct ConstError {
    span: Span,
    code: sabiko_diagnostics::ErrorCode,
    message: String,
    /// The cause was already reported (a referenced constant failed); say nothing more.
    silent: bool,
}

fn non_const(span: Span, what: &str) -> ConstError {
    ConstError {
        span,
        code: ErrorCode::NON_CONSTANT_EXPRESSION,
        message: format!("{what} cannot be used in constants yet"),
        silent: false,
    }
}

fn failed(span: Span, message: impl Into<String>) -> ConstError {
    ConstError {
        span,
        code: ErrorCode::CONST_EVAL_FAILED,
        message: message.into(),
        silent: false,
    }
}

/// Evaluates every constant in `bodies` (keyed by the `const` item's `DefId`), in
/// dependency order, reporting failures once per constant.
pub(crate) fn eval_all<'a>(
    resolve: &ResolveResults,
    bodies: &HashMap<DefId, (&'a Expr, &'a BodyTypes)>,
    sink: &mut DiagnosticSink,
) -> HashMap<DefId, ConstValue> {
    let mut ev = Evaluator {
        resolve,
        bodies,
        done: HashMap::new(),
        visiting: HashSet::new(),
        failed: HashSet::new(),
        sink,
    };
    let mut defs: Vec<DefId> = bodies.keys().copied().collect();
    defs.sort();
    for def in defs {
        ev.eval_const(def, None);
    }
    ev.done
}

struct Evaluator<'a, 'b> {
    resolve: &'a ResolveResults,
    bodies: &'a HashMap<DefId, (&'b Expr, &'b BodyTypes)>,
    done: HashMap<DefId, ConstValue>,
    visiting: HashSet<DefId>,
    failed: HashSet<DefId>,
    sink: &'a mut DiagnosticSink,
}

impl Evaluator<'_, '_> {
    /// `use_span`: where this constant is referenced from, for cycle errors.
    fn eval_const(&mut self, def: DefId, use_span: Option<Span>) -> Option<ConstValue> {
        if let Some(v) = self.done.get(&def) {
            return Some(v.clone());
        }
        if self.failed.contains(&def) {
            return None;
        }
        let (expr, body) = self.bodies[&def];
        if body.has_errors {
            self.failed.insert(def);
            return None;
        }
        if self.visiting.contains(&def) {
            let name = self.resolve.def(def).name.clone();
            let span = use_span.unwrap_or(expr.span);
            self.sink.emit(
                Diagnostic::error(
                    ErrorCode::CONST_EVAL_FAILED,
                    format!("cycle detected when evaluating constant `{name}`"),
                )
                .with_label(span, "cyclic reference")
                .with_label(self.resolve.def(def).span, format!("`{name}` defined here")),
            );
            self.failed.insert(def);
            return None;
        }
        self.visiting.insert(def);
        let result = self.eval(expr, body);
        self.visiting.remove(&def);
        match result {
            Ok(v) => {
                self.done.insert(def, v.clone());
                Some(v)
            }
            Err(e) => {
                if !e.silent {
                    self.sink.emit(
                        Diagnostic::error(e.code, e.message).with_label(e.span, "in this constant"),
                    );
                }
                self.failed.insert(def);
                None
            }
        }
    }

    fn eval(&mut self, expr: &Expr, body: &BodyTypes) -> Result<ConstValue, ConstError> {
        let ty = body.expr_ty(expr.id).clone();
        // An error type comes from a mistake reported elsewhere, such as an unresolved
        // declared type on a constant this one reads: fail without a second diagnostic.
        if ty.is_error() {
            return Err(ConstError {
                span: expr.span,
                code: ErrorCode::CONST_EVAL_FAILED,
                message: String::new(),
                silent: true,
            });
        }
        let v = self.eval_kind(expr, &ty, body)?;
        // Floats are computed in f64; an `f32` result must hold exactly what an f32 can.
        // Rounding each operation's f64 result is exact f32 arithmetic for `+ - * /`
        // (f64 has more than 2 * 24 + 2 bits) and `%` is exact in both.
        Ok(match (v, ty.as_prim()) {
            (ConstValue::Float(f), Some(PrimTy::F32)) => ConstValue::Float(f64::from(f as f32)),
            (v, _) => v,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn eval_kind(
        &mut self,
        expr: &Expr,
        ty: &Ty,
        body: &BodyTypes,
    ) -> Result<ConstValue, ConstError> {
        match &expr.kind {
            ExprKind::Lit(lit) => Ok(match lit {
                Lit::Int { value, .. } => int_from_u128(*value, ty),
                Lit::Float { text, .. } => {
                    let f = sabiko_lexer::literal::parse_float(text)
                        .map_err(|_| failed(expr.span, "invalid float literal"))?;
                    ConstValue::Float(f.value)
                }
                Lit::Bool(b) => ConstValue::Bool(*b),
                Lit::Char(c) => ConstValue::Char(*c),
                Lit::Str(s) => ConstValue::Str(s.clone()),
            }),
            ExprKind::Unit => Ok(ConstValue::Unit),
            ExprKind::Paren(inner) => self.eval(inner, body),
            ExprKind::Path(_) => match self.resolve.res(expr.id) {
                Res::Def(d) if self.resolve.def(d).kind == DefKind::Const => {
                    if !self.bodies.contains_key(&d) {
                        return Err(failed(expr.span, "referenced constant has errors"));
                    }
                    self.eval_const(d, Some(expr.span)).ok_or(ConstError {
                        span: expr.span,
                        code: ErrorCode::CONST_EVAL_FAILED,
                        message: String::new(),
                        silent: true,
                    })
                }
                Res::Def(_) => Err(non_const(expr.span, "struct values")),
                Res::Local(_) => Err(non_const(expr.span, "local variables")),
                Res::Prim(_) | Res::Err => Err(failed(expr.span, "unresolved name")),
            },
            ExprKind::Unary(op, operand) => {
                let v = self.eval(operand, body)?;
                let prim = ty.as_prim();
                match (op, v) {
                    // `-170141183460469231731687303715884105728i128`: the literal is
                    // `2^127`, read as `i128::MIN` (see `int_from_u128`), and its negation
                    // is `i128::MIN` itself.
                    (UnOp::Neg, ConstValue::Int(i128::MIN)) if is_int_literal(operand) => {
                        check_range(i128::MIN, prim, expr.span, "negate")
                    }
                    (UnOp::Neg, ConstValue::Int(i)) => {
                        let r = i
                            .checked_neg()
                            .ok_or_else(|| failed(expr.span, "attempt to negate with overflow"))?;
                        check_range(r, prim, expr.span, "negate")
                    }
                    (UnOp::Neg, ConstValue::Float(f)) => Ok(ConstValue::Float(-f)),
                    (UnOp::Not, ConstValue::Bool(b)) => Ok(ConstValue::Bool(!b)),
                    (UnOp::Not, ConstValue::Int(i)) => Ok(ConstValue::Int(!i)),
                    (UnOp::Not, ConstValue::Uint(u)) => {
                        let p = prim.ok_or_else(|| failed(expr.span, "untyped integer"))?;
                        Ok(ConstValue::Uint(truncate(!u, p)))
                    }
                    _ => Err(failed(expr.span, "invalid operand for unary operator")),
                }
            }
            ExprKind::Binary(op, lhs, rhs) => {
                let l = self.eval(lhs, body)?;
                // `&&` / `||` short-circuit.
                if let (BinOp::And, ConstValue::Bool(false)) | (BinOp::Or, ConstValue::Bool(true)) =
                    (op, &l)
                {
                    return Ok(l);
                }
                let r = self.eval(rhs, body)?;
                let operand_ty = body.expr_ty(lhs.id).as_prim();
                binary(*op, l, r, operand_ty, expr.span)
            }
            ExprKind::Cast(inner, _) => {
                let v = self.eval(inner, body)?;
                let from = body.expr_ty(inner.id).as_prim();
                let to = ty
                    .as_prim()
                    .ok_or_else(|| failed(expr.span, "invalid cast"))?;
                cast(&v, from, to).ok_or_else(|| failed(expr.span, "invalid cast"))
            }
            ExprKind::Block(_) => Err(non_const(expr.span, "blocks")),
            ExprKind::If { .. } => Err(non_const(expr.span, "`if` expressions")),
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                Err(non_const(expr.span, "function calls"))
            }
            ExprKind::Struct { .. } => Err(non_const(expr.span, "struct values")),
            ExprKind::Field(..) => Err(non_const(expr.span, "field accesses")),
            ExprKind::Ref { .. } => Err(non_const(expr.span, "references")),
            ExprKind::Deref(_) => Err(non_const(expr.span, "dereferences")),
            ExprKind::Index(..) => Err(non_const(expr.span, "indexing")),
            ExprKind::Assign(..) | ExprKind::AssignOp(..) => {
                Err(non_const(expr.span, "assignments"))
            }
            ExprKind::While { .. } | ExprKind::Loop { .. } => Err(non_const(expr.span, "loops")),
            ExprKind::Break(_) | ExprKind::Continue | ExprKind::Return(_) => {
                Err(non_const(expr.span, "control flow"))
            }
        }
    }
}

/// The value of an integer literal of type `ty`. A signed literal above `i128::MAX` can
/// only be `2^127` under a negation (the type checker rejects it anywhere else); it wraps
/// to `i128::MIN`, which the negation keeps. (Saturating it to `i128::MAX` evaluated
/// `-170141183460469231731687303715884105728` to `i128::MIN + 1`.)
fn int_from_u128(value: u128, ty: &Ty) -> ConstValue {
    match ty.as_prim() {
        #[allow(clippy::cast_possible_wrap)]
        Some(p) if p.is_signed_int() => ConstValue::Int(value as i128),
        _ => ConstValue::Uint(value),
    }
}

/// Whether `e` is an integer literal, through parentheses.
fn is_int_literal(mut e: &Expr) -> bool {
    while let ExprKind::Paren(inner) = &e.kind {
        e = inner;
    }
    matches!(e.kind, ExprKind::Lit(Lit::Int { .. }))
}

/// The inclusive `(min, max)` of an integer type.
#[must_use]
pub fn int_bounds(p: PrimTy) -> (i128, u128) {
    let bits = p.int_bits().expect("integer type");
    if p.is_signed_int() {
        let max = (1u128 << (bits - 1)) - 1;
        (-(max as i128) - 1, max)
    } else if bits == 128 {
        (0, u128::MAX)
    } else {
        (0, (1u128 << bits) - 1)
    }
}

fn bounds(p: PrimTy) -> (i128, u128) {
    int_bounds(p)
}

fn check_range(
    v: i128,
    prim: Option<PrimTy>,
    span: Span,
    op: &str,
) -> Result<ConstValue, ConstError> {
    let p = prim.ok_or_else(|| failed(span, "untyped integer"))?;
    let (min, max) = bounds(p);
    if v < min || v > max as i128 {
        return Err(failed(span, format!("attempt to {op} with overflow")));
    }
    Ok(ConstValue::Int(v))
}

fn check_range_u(
    v: u128,
    prim: Option<PrimTy>,
    span: Span,
    op: &str,
) -> Result<ConstValue, ConstError> {
    let p = prim.ok_or_else(|| failed(span, "untyped integer"))?;
    let (_, max) = bounds(p);
    if v > max {
        return Err(failed(span, format!("attempt to {op} with overflow")));
    }
    Ok(ConstValue::Uint(v))
}

fn truncate(v: u128, p: PrimTy) -> u128 {
    let (_, max) = bounds(p);
    v & max
}

fn shift_amount(r: &ConstValue, bits: u32, span: Span, op: &str) -> Result<u32, ConstError> {
    let amount = match r {
        ConstValue::Int(i) => u32::try_from(*i).ok(),
        ConstValue::Uint(u) => u32::try_from(*u).ok(),
        _ => None,
    };
    match amount {
        Some(a) if a < bits => Ok(a),
        _ => Err(failed(span, format!("attempt to {op} with overflow"))),
    }
}

#[allow(clippy::too_many_lines)]
fn binary(
    op: BinOp,
    lhs: ConstValue,
    rhs: ConstValue,
    prim: Option<PrimTy>,
    span: Span,
) -> Result<ConstValue, ConstError> {
    use ConstValue as V;
    let bits = prim.and_then(PrimTy::int_bits).unwrap_or(128);
    Ok(match (op, lhs, rhs) {
        (BinOp::Add, V::Int(a), V::Int(b)) => {
            let v = a
                .checked_add(b)
                .ok_or_else(|| failed(span, "attempt to add with overflow"))?;
            check_range(v, prim, span, "add")?
        }
        (BinOp::Sub, V::Int(a), V::Int(b)) => {
            let v = a
                .checked_sub(b)
                .ok_or_else(|| failed(span, "attempt to subtract with overflow"))?;
            check_range(v, prim, span, "subtract")?
        }
        (BinOp::Mul, V::Int(a), V::Int(b)) => {
            let v = a
                .checked_mul(b)
                .ok_or_else(|| failed(span, "attempt to multiply with overflow"))?;
            check_range(v, prim, span, "multiply")?
        }
        (BinOp::Div, V::Int(a), V::Int(b)) => {
            if b == 0 {
                return Err(failed(span, "attempt to divide by zero"));
            }
            let v = a
                .checked_div(b)
                .ok_or_else(|| failed(span, "attempt to divide with overflow"))?;
            check_range(v, prim, span, "divide")?
        }
        (BinOp::Rem, V::Int(a), V::Int(b)) => {
            if b == 0 {
                return Err(failed(
                    span,
                    "attempt to calculate the remainder with a divisor of zero",
                ));
            }
            let v = a
                .checked_rem(b)
                .ok_or_else(|| failed(span, "attempt to calculate the remainder with overflow"))?;
            check_range(v, prim, span, "calculate the remainder")?
        }
        (BinOp::Add, V::Uint(a), V::Uint(b)) => {
            let v = a
                .checked_add(b)
                .ok_or_else(|| failed(span, "attempt to add with overflow"))?;
            check_range_u(v, prim, span, "add")?
        }
        (BinOp::Sub, V::Uint(a), V::Uint(b)) => {
            let v = a
                .checked_sub(b)
                .ok_or_else(|| failed(span, "attempt to subtract with overflow"))?;
            check_range_u(v, prim, span, "subtract")?
        }
        (BinOp::Mul, V::Uint(a), V::Uint(b)) => {
            let v = a
                .checked_mul(b)
                .ok_or_else(|| failed(span, "attempt to multiply with overflow"))?;
            check_range_u(v, prim, span, "multiply")?
        }
        (BinOp::Div, V::Uint(a), V::Uint(b)) => {
            if b == 0 {
                return Err(failed(span, "attempt to divide by zero"));
            }
            V::Uint(a / b)
        }
        (BinOp::Rem, V::Uint(a), V::Uint(b)) => {
            if b == 0 {
                return Err(failed(
                    span,
                    "attempt to calculate the remainder with a divisor of zero",
                ));
            }
            V::Uint(a % b)
        }
        (BinOp::BitAnd, V::Int(a), V::Int(b)) => V::Int(a & b),
        (BinOp::BitOr, V::Int(a), V::Int(b)) => V::Int(a | b),
        (BinOp::BitXor, V::Int(a), V::Int(b)) => V::Int(a ^ b),
        (BinOp::BitAnd, V::Uint(a), V::Uint(b)) => V::Uint(a & b),
        (BinOp::BitOr, V::Uint(a), V::Uint(b)) => V::Uint(a | b),
        (BinOp::BitXor, V::Uint(a), V::Uint(b)) => V::Uint(a ^ b),
        (BinOp::BitAnd | BinOp::And, V::Bool(a), V::Bool(b)) => V::Bool(a & b),
        (BinOp::BitOr | BinOp::Or, V::Bool(a), V::Bool(b)) => V::Bool(a | b),
        (BinOp::BitXor, V::Bool(a), V::Bool(b)) => V::Bool(a ^ b),
        (BinOp::Shl, V::Int(a), amount) => {
            let s = shift_amount(&amount, bits, span, "shift left")?;
            let p = prim.ok_or_else(|| failed(span, "untyped integer"))?;
            // Shift in the unsigned domain and sign-extend back.
            let (_, max) = bounds(p);
            let raw = ((a as u128) << s) & ((max << 1) | 1);
            V::Int(sign_extend(raw, bits))
        }
        (BinOp::Shr, V::Int(a), amount) => {
            let s = shift_amount(&amount, bits, span, "shift right")?;
            V::Int(a >> s)
        }
        (BinOp::Shl, V::Uint(a), amount) => {
            let s = shift_amount(&amount, bits, span, "shift left")?;
            let p = prim.ok_or_else(|| failed(span, "untyped integer"))?;
            V::Uint(truncate(a << s, p))
        }
        (BinOp::Shr, V::Uint(a), amount) => {
            let s = shift_amount(&amount, bits, span, "shift right")?;
            V::Uint(a >> s)
        }
        (BinOp::Add, V::Float(a), V::Float(b)) => V::Float(a + b),
        (BinOp::Sub, V::Float(a), V::Float(b)) => V::Float(a - b),
        (BinOp::Mul, V::Float(a), V::Float(b)) => V::Float(a * b),
        (BinOp::Div, V::Float(a), V::Float(b)) => V::Float(a / b),
        (BinOp::Rem, V::Float(a), V::Float(b)) => V::Float(a % b),
        (op, lhs, rhs) if op.is_comparison() => V::Bool(
            compare(op, &lhs, &rhs)
                .ok_or_else(|| failed(span, "invalid operands for comparison"))?,
        ),
        _ => return Err(failed(span, "invalid operands for binary operator")),
    })
}

fn sign_extend(raw: u128, bits: u32) -> i128 {
    if bits == 128 {
        return raw as i128;
    }
    let sign = 1u128 << (bits - 1);
    if raw & sign != 0 {
        (raw | !((sign << 1) - 1)) as i128
    } else {
        raw as i128
    }
}

fn compare(op: BinOp, lhs: &ConstValue, rhs: &ConstValue) -> Option<bool> {
    use std::cmp::Ordering;
    let ord = match (lhs, rhs) {
        (ConstValue::Int(a), ConstValue::Int(b)) => a.cmp(b),
        (ConstValue::Uint(a), ConstValue::Uint(b)) => a.cmp(b),
        (ConstValue::Bool(a), ConstValue::Bool(b)) => a.cmp(b),
        (ConstValue::Char(a), ConstValue::Char(b)) => a.cmp(b),
        (ConstValue::Unit, ConstValue::Unit) => Ordering::Equal,
        (ConstValue::Float(a), ConstValue::Float(b)) => match a.partial_cmp(b) {
            Some(o) => o,
            // NaN: only `!=` holds.
            None => return Some(op == BinOp::Ne),
        },
        _ => return None,
    };
    Some(match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Ne => ord != Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        BinOp::Ge => ord != Ordering::Less,
        _ => return None,
    })
}

/// A cast source normalised to a 128-bit two's complement pattern or a float.
enum Src {
    Bits(u128),
    Float(f64),
}

/// `v as to`, with Rust's semantics: integer casts wrap, float to int saturates.
fn cast(v: &ConstValue, from: Option<PrimTy>, to: PrimTy) -> Option<ConstValue> {
    let src = match v {
        ConstValue::Int(i) => Src::Bits(*i as u128),
        ConstValue::Uint(u) => Src::Bits(*u),
        ConstValue::Bool(b) => Src::Bits(u128::from(*b)),
        ConstValue::Char(c) => Src::Bits(u128::from(u32::from(*c))),
        ConstValue::Float(f) => Src::Float(*f),
        ConstValue::Str(_) | ConstValue::Unit => return None,
    };
    Some(match (src, to) {
        (Src::Bits(b), p) if p.is_integer() => {
            let bits = p.int_bits()?;
            let raw = if bits == 128 {
                b
            } else {
                b & ((1u128 << bits) - 1)
            };
            if p.is_signed_int() {
                ConstValue::Int(sign_extend(raw, bits))
            } else {
                ConstValue::Uint(raw)
            }
        }
        (Src::Bits(b), PrimTy::Char) => ConstValue::Char(char::from_u32(u32::try_from(b).ok()?)?),
        (Src::Bits(b), PrimTy::F32) => {
            let f = if v_signed(from) {
                (b as i128) as f32
            } else {
                b as f32
            };
            ConstValue::Float(f64::from(f))
        }
        (Src::Bits(b), PrimTy::F64) => ConstValue::Float(if v_signed(from) {
            (b as i128) as f64
        } else {
            b as f64
        }),
        (Src::Float(f), PrimTy::F32) => ConstValue::Float(f64::from(f as f32)),
        (Src::Float(f), PrimTy::F64) => ConstValue::Float(f),
        // Rust's `as` already maps NaN to 0 and saturates at the 128-bit range; clamp to
        // the target's range on top.
        (Src::Float(f), p) if p.is_integer() => {
            let (min, max) = bounds(p);
            if p.is_signed_int() {
                let v = if f >= max as f64 {
                    max as i128
                } else if f <= min as f64 {
                    min
                } else {
                    f as i128
                };
                ConstValue::Int(v)
            } else {
                ConstValue::Uint(if f >= max as f64 { max } else { f as u128 })
            }
        }
        _ => return None,
    })
}

fn v_signed(from: Option<PrimTy>) -> bool {
    from.is_some_and(PrimTy::is_signed_int)
}
