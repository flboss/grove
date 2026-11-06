use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use chrono::{DateTime, Datelike, NaiveTime, SecondsFormat, TimeDelta, Timelike, Utc};
use grove_query::ast::{BinaryOp, ConstantName, Literal, SortDir, TypeName, UnaryOp};
use grove_query::typecheck::types::{TypedExpr, TypedMethodArg};
use grove_schema::validated::{Config, DecArithmetic, IntArithmetic};
use grove_types::{Span, Spanned};
use num_traits::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, RoundingStrategy};

use super::{EvalCtx, eval};
use crate::error::ExecError;
use crate::eval::data::resolve_base_link;
use crate::value::{
    NANOS_PER_DAY, NANOS_PER_HOUR, NANOS_PER_MIN, NANOS_PER_SEC, Scalar, Val, total_nanos,
};

pub(crate) const MIDNIGHT: NaiveTime =
    NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time");

#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Mod,
}

impl ArithOp {
    pub fn is_rhs_zero_undefined(&self) -> bool {
        matches!(self, ArithOp::Div | ArithOp::Rem | ArithOp::Mod)
    }

    pub fn full_name(&self) -> &str {
        match self {
            ArithOp::Add => "addition",
            ArithOp::Sub => "subtraction",
            ArithOp::Mul => "multiplication",
            ArithOp::Div => "division",
            ArithOp::Rem => "C-style remainder",
            ArithOp::Mod => "euclidean remainder",
        }
    }
}

impl fmt::Display for ArithOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArithOp::Add => write!(f, "+"),
            ArithOp::Sub => write!(f, "-"),
            ArithOp::Mul => write!(f, "*"),
            ArithOp::Div => write!(f, "/"),
            ArithOp::Rem => write!(f, "%"),
            ArithOp::Mod => write!(f, "mod"),
        }
    }
}

impl TryFrom<BinaryOp> for ArithOp {
    type Error = ();

    fn try_from(value: BinaryOp) -> Result<Self, Self::Error> {
        match value {
            BinaryOp::Add => Ok(ArithOp::Add),
            BinaryOp::Sub => Ok(ArithOp::Sub),
            BinaryOp::Mul => Ok(ArithOp::Mul),
            BinaryOp::Div => Ok(ArithOp::Div),
            BinaryOp::Rem => Ok(ArithOp::Rem),
            BinaryOp::Mod => Ok(ArithOp::Mod),
            _ => Err(()),
        }
    }
}

fn int_arith(
    a: i64,
    op: ArithOp,
    b: i64,
    mode: IntArithmetic,
    span: Span,
) -> Result<Scalar, ExecError> {
    if b == 0 && op.is_rhs_zero_undefined() {
        return Err(ExecError::Abort {
            message: format!("{} by zero: {a} {op} {b}", op.full_name()),
            span,
        });
    }
    let checked = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => a.checked_div(b),
        ArithOp::Rem => a.checked_rem(b),
        ArithOp::Mod => a.checked_rem_euclid(b),
    };
    let n = match checked {
        Some(n) => n,
        None => match mode {
            IntArithmetic::Checked => {
                return Err(ExecError::Abort {
                    message: format!("integer overflow: {a} {op} {b}"),
                    span,
                });
            }
            IntArithmetic::Saturating => match op {
                ArithOp::Add => a.saturating_add(b),
                ArithOp::Sub => a.saturating_sub(b),
                ArithOp::Mul => a.saturating_mul(b),
                ArithOp::Div => a.saturating_div(b),
                // i64::MIN % -1 is the only failure case
                ArithOp::Rem | ArithOp::Mod => 0,
            },
            IntArithmetic::Wrapping => match op {
                ArithOp::Add => a.wrapping_add(b),
                ArithOp::Sub => a.wrapping_sub(b),
                ArithOp::Mul => a.wrapping_mul(b),
                ArithOp::Div => a.wrapping_div(b),
                ArithOp::Rem | ArithOp::Mod => 0,
            },
        },
    };
    Ok(Scalar::Int(n))
}

fn float_arith(
    a: f64,
    op: ArithOp,
    b: f64,
    float_checks: bool,
    span: Span,
) -> Result<Scalar, ExecError> {
    let f = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
        ArithOp::Mod => {
            let r = a % b;
            if r < 0.0 { r + b.abs() } else { r }
        }
    };
    if float_checks && !f.is_finite() {
        return Err(ExecError::Abort {
            message: format!("non-finite float result: {a} {op} {b}"),
            span,
        });
    }
    Ok(Scalar::Float(f))
}

fn dec_arith(
    a: Decimal,
    op: ArithOp,
    b: Decimal,
    mode: DecArithmetic,
    span: Span,
) -> Result<Scalar, ExecError> {
    if b.is_zero() && op.is_rhs_zero_undefined() {
        return Err(ExecError::Abort {
            message: format!("{} by zero: {a} {op} {b}", op.full_name()),
            span,
        });
    }
    let checked = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => a.checked_div(b),
        ArithOp::Rem => a.checked_rem(b),
        ArithOp::Mod => a.checked_rem(b).and_then(|r| {
            if r.is_sign_negative() {
                r.checked_add(b.abs())
            } else {
                Some(r)
            }
        }),
    };
    let d = match checked {
        Some(d) => d,
        None => match mode {
            DecArithmetic::Checked => {
                return Err(ExecError::Abort {
                    message: format!("decimal overflow: {a} {op} {b}"),
                    span,
                });
            }
            DecArithmetic::Saturating => match op {
                ArithOp::Add | ArithOp::Sub => {
                    if a.is_sign_positive() {
                        Decimal::MAX
                    } else {
                        Decimal::MIN
                    }
                }
                ArithOp::Mul | ArithOp::Div => {
                    if a.is_sign_negative() == b.is_sign_negative() {
                        Decimal::MAX
                    } else {
                        Decimal::MIN
                    }
                }
                // Scale-up overflow occurred
                ArithOp::Rem | ArithOp::Mod => {
                    return Err(ExecError::Abort {
                        message: format!("decimal overflow: {a} {op} {b}"),
                        span,
                    });
                }
            },
        },
    };
    Ok(Scalar::Dec(d.normalize()))
}

/// Build a Duration from whole nanoseconds; out-of-range aborts
fn duration_from_nanos(nanos: i128, span: Span) -> Result<TimeDelta, ExecError> {
    let secs = nanos.div_euclid(NANOS_PER_SEC as i128);
    let sub = nanos.rem_euclid(NANOS_PER_SEC as i128) as u32;
    i64::try_from(secs)
        .ok()
        .and_then(|secs| TimeDelta::new(secs, sub))
        .ok_or_else(|| ExecError::Abort {
            message: format!("duration overflow: {secs}.{sub:09}s"),
            span,
        })
}

/// Scale a duration by a decimal factor (banker's rounding to ns)
fn scale_duration(
    duration: TimeDelta,
    factor: Decimal,
    is_div: bool,
    span: Span,
) -> Result<Scalar, ExecError> {
    if is_div && factor.is_zero() {
        return Err(ExecError::Abort {
            message: format!(
                "division by zero: {} / {factor}",
                Scalar::Duration(duration)
            ),
            span,
        });
    }
    let base = Decimal::from_i128_with_scale(total_nanos(&duration), 0);
    let scaled = if is_div {
        base.checked_div(factor)
    } else {
        base.checked_mul(factor)
    };
    let nanos = scaled
        .and_then(|scaled| {
            let rounded = scaled.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
            rounded.to_i128()
        })
        .ok_or_else(|| ExecError::Abort {
            message: format!(
                "duration overflow: {factor} {} {}",
                if is_div { "/" } else { "*" },
                Scalar::Duration(duration)
            ),
            span,
        })?;
    duration_from_nanos(nanos, span).map(Scalar::Duration)
}

pub(crate) fn eval_binary(
    op: &Spanned<BinaryOp>,
    lhs: &TypedExpr,
    rhs: &TypedExpr,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    if matches!(op.value, BinaryOp::And | BinaryOp::Or) {
        return eval_logic(op, lhs, rhs, ctx);
    }
    let l = eval(lhs, ctx)?;
    let r = eval(rhs, ctx)?;
    if matches!(
        op.value,
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge
    ) {
        return eval_ordered(op, &l, &r, span);
    }
    if matches!(op.value, BinaryOp::Eq | BinaryOp::Ne) {
        return eval_equality(op, &l, &r);
    }
    let scalar = match (op.value, l, r) {
        (BinaryOp::In, needle, Val::Tuple(members)) => {
            Scalar::Bool(members.iter().any(|member| needle.eq(member) == Some(true)))
        }
        (op, Val::Scalar(Scalar::Int(a)), Val::Scalar(Scalar::Int(b))) if op.is_arithmetic() => {
            int_arith(
                a,
                op.try_into().unwrap(),
                b,
                ctx.schema.config.int_arithmetic,
                span,
            )?
        }
        (op, Val::Scalar(Scalar::Float(a)), Val::Scalar(Scalar::Float(b)))
            if op.is_arithmetic() =>
        {
            float_arith(
                a,
                op.try_into().unwrap(),
                b,
                ctx.schema.config.float_checks,
                span,
            )?
        }
        (op, Val::Scalar(Scalar::Dec(a)), Val::Scalar(Scalar::Dec(b))) if op.is_arithmetic() => {
            dec_arith(
                a,
                op.try_into().unwrap(),
                b,
                ctx.schema.config.dec_arithmetic,
                span,
            )?
        }
        (
            BinaryOp::Add,
            Val::Scalar(lhs @ Scalar::Instant(t)),
            Val::Scalar(rhs @ Scalar::Duration(d)),
        ) => Scalar::Instant(t.checked_add_signed(d).ok_or_else(|| ExecError::Abort {
            message: format!("instant overflow: {lhs} + {rhs}"),
            span,
        })?),
        (
            BinaryOp::Sub,
            Val::Scalar(lhs @ Scalar::Instant(t)),
            Val::Scalar(rhs @ Scalar::Duration(d)),
        ) => Scalar::Instant(t.checked_sub_signed(d).ok_or_else(|| ExecError::Abort {
            message: format!("instant overflow: {lhs} - {rhs}"),
            span,
        })?),
        (BinaryOp::Sub, Val::Scalar(Scalar::Instant(a)), Val::Scalar(Scalar::Instant(b))) => {
            Scalar::Duration(a.signed_duration_since(b))
        }
        (
            BinaryOp::Add,
            Val::Scalar(lhs @ Scalar::Duration(a)),
            Val::Scalar(rhs @ Scalar::Duration(b)),
        ) => Scalar::Duration(a.checked_add(&b).ok_or_else(|| ExecError::Abort {
            message: format!("duration overflow: {lhs} + {rhs}"),
            span,
        })?),
        (
            BinaryOp::Sub,
            Val::Scalar(lhs @ Scalar::Duration(a)),
            Val::Scalar(rhs @ Scalar::Duration(b)),
        ) => Scalar::Duration(a.checked_sub(&b).ok_or_else(|| ExecError::Abort {
            message: format!("duration overflow: {lhs} - {rhs}"),
            span,
        })?),
        (
            BinaryOp::Rem,
            Val::Scalar(lhs @ Scalar::Duration(a)),
            Val::Scalar(rhs @ Scalar::Duration(b)),
        ) => {
            let (an, bn) = (total_nanos(&a), total_nanos(&b));
            if bn == 0 {
                return Err(ExecError::Abort {
                    message: format!("remainder by zero: {lhs} % {rhs}"),
                    span,
                });
            }
            Scalar::Duration(duration_from_nanos(an % bn, span)?)
        }
        (
            BinaryOp::Mod,
            Val::Scalar(lhs @ Scalar::Duration(a)),
            Val::Scalar(rhs @ Scalar::Duration(b)),
        ) => {
            let (an, bn) = (total_nanos(&a), total_nanos(&b));
            if bn == 0 {
                return Err(ExecError::Abort {
                    message: format!("remainder by zero: {lhs} mod {rhs}"),
                    span,
                });
            }
            let r = an % bn;
            let r = if r < 0 { r + bn.abs() } else { r };
            Scalar::Duration(duration_from_nanos(r, span)?)
        }
        (
            BinaryOp::Div,
            Val::Scalar(lhs @ Scalar::Duration(a)),
            Val::Scalar(rhs @ Scalar::Duration(b)),
        ) => {
            if b.is_zero() {
                return Err(ExecError::Abort {
                    message: format!("division by zero: {lhs} / {rhs}"),
                    span,
                });
            }
            let ratio = Decimal::from_i128_with_scale(total_nanos(&a), 0)
                .checked_div(Decimal::from_i128_with_scale(total_nanos(&b), 0))
                .ok_or_else(|| ExecError::Abort {
                    message: format!("decimal overflow: {lhs} / {rhs}"),
                    span,
                })?;
            Scalar::Dec(ratio.normalize())
        }
        (BinaryOp::Mul, Val::Scalar(Scalar::Duration(d)), factor)
        | (BinaryOp::Mul, factor, Val::Scalar(Scalar::Duration(d)))
            if matches!(
                factor,
                Val::Scalar(Scalar::Int(_) | Scalar::Float(_) | Scalar::Dec(_))
            ) =>
        {
            match factor {
                Val::Scalar(Scalar::Int(n)) => {
                    let scaled =
                        total_nanos(&d)
                            .checked_mul(n as i128)
                            .ok_or_else(|| ExecError::Abort {
                                message: format!(
                                    "duration overflow: {} * {factor}",
                                    Scalar::Duration(d)
                                ),
                                span,
                            })?;
                    Scalar::Duration(duration_from_nanos(scaled, span)?)
                }
                Val::Scalar(Scalar::Float(x)) => {
                    let factor = Decimal::from_f64(x).ok_or_else(|| ExecError::Abort {
                        message: format!("cannot scale a duration by {x}"),
                        span,
                    })?;
                    scale_duration(d, factor, false, span)?
                }
                Val::Scalar(Scalar::Dec(factor)) => scale_duration(d, factor, false, span)?,
                _ => unreachable!("factor type guarded"),
            }
        }
        (BinaryOp::Div, Val::Scalar(Scalar::Duration(d)), factor)
            if matches!(
                factor,
                Val::Scalar(Scalar::Int(_) | Scalar::Float(_) | Scalar::Dec(_))
            ) =>
        {
            if matches!(factor, Val::Scalar(Scalar::Int(n)) if n == 0)
                || matches!(factor, Val::Scalar(Scalar::Float(f)) if f == 0.0)
            {
                return Err(ExecError::Abort {
                    message: format!("division by zero: {} / {factor}", Scalar::Duration(d)),
                    span,
                });
            }
            match factor {
                Val::Scalar(Scalar::Int(n)) => scale_duration(d, Decimal::from(n), true, span)?,
                Val::Scalar(Scalar::Float(x)) => {
                    let factor = Decimal::from_f64(x).ok_or_else(|| ExecError::Abort {
                        message: format!("cannot scale a duration by {x}"),
                        span,
                    })?;
                    scale_duration(d, factor, true, span)?
                }
                Val::Scalar(Scalar::Dec(f)) => scale_duration(d, f, true, span)?,
                _ => unreachable!("factor guard above"),
            }
        }
        (op, lhs, rhs) => {
            return Err(ExecError::Integrity {
                message: format!("mismatched binary operands: {lhs} {op} {rhs} (internal error)"),
            });
        }
    };
    Ok(Val::Scalar(scalar))
}

pub(crate) fn eval_unary(
    op: &Spanned<UnaryOp>,
    inner: &TypedExpr,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let value = eval(inner, ctx)?;
    match (op.value, value) {
        (UnaryOp::Neg, Val::Scalar(Scalar::Int(n))) => Ok(match n.checked_neg() {
            Some(m) => Val::Scalar(Scalar::Int(m)),
            None => match ctx.schema.config.int_arithmetic {
                IntArithmetic::Checked => {
                    return Err(ExecError::Abort {
                        message: format!("integer overflow: -{n}"),
                        span,
                    });
                }
                IntArithmetic::Saturating => Val::Scalar(Scalar::Int(i64::MAX)),
                IntArithmetic::Wrapping => Val::Scalar(Scalar::Int(i64::MIN)),
            },
        }),
        (UnaryOp::Neg, Val::Scalar(Scalar::Float(x))) => {
            if ctx.schema.config.float_checks && !(-x).is_finite() {
                return Err(ExecError::Abort {
                    message: format!("non-finite float result: -{x}"),
                    span,
                });
            }
            Ok(Val::Scalar(Scalar::Float(-x)))
        }
        (UnaryOp::Neg, Val::Scalar(Scalar::Dec(d))) => {
            Ok(Val::Scalar(Scalar::Dec((-d).normalize())))
        }
        (UnaryOp::Neg, Val::Scalar(Scalar::Duration(d))) => Ok(Val::Scalar(Scalar::Duration(-d))),
        (UnaryOp::Not, Val::Scalar(Scalar::Bool(b))) => Ok(Val::Scalar(Scalar::Bool(!b))),
        (op, operand) => Err(ExecError::Integrity {
            message: format!("mismatched unary operand: {op}{operand} (internal error)"),
        }),
    }
}

fn eval_equality(op: &Spanned<BinaryOp>, l: &Val, r: &Val) -> Result<Val, ExecError> {
    let eq = l.eq(r).ok_or_else(|| ExecError::Integrity {
        message: format!(
            "mismatched equality operands: {l} {} {r} (internal error)",
            op.value
        ),
    })?;
    let result = matches!((op.value, eq), (BinaryOp::Eq, true) | (BinaryOp::Ne, false));
    Ok(Val::Scalar(Scalar::Bool(result)))
}

fn eval_ordered(op: &Spanned<BinaryOp>, l: &Val, r: &Val, span: Span) -> Result<Val, ExecError> {
    let ord = l.cmp(r, span)?;
    let result = match op.value {
        BinaryOp::Lt => ord == std::cmp::Ordering::Less,
        BinaryOp::Le => ord != std::cmp::Ordering::Greater,
        BinaryOp::Gt => ord == std::cmp::Ordering::Greater,
        BinaryOp::Ge => ord != std::cmp::Ordering::Less,
        _ => {
            return Err(ExecError::Integrity {
                message: "non-ordering operator in ordered dispatch (internal error)".to_string(),
            });
        }
    };
    Ok(Val::Scalar(Scalar::Bool(result)))
}

// Short-circuiting, evaluates lazily
fn eval_logic(
    op: &Spanned<BinaryOp>,
    lhs: &TypedExpr,
    rhs: &TypedExpr,
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let l = eval(lhs, ctx)?;
    let Val::Scalar(Scalar::Bool(l)) = l else {
        return Err(ExecError::Integrity {
            message: format!("non-Bool logic operand: {l} (internal error)"),
        });
    };
    let decided = matches!((op.value, l), (BinaryOp::And, false) | (BinaryOp::Or, true));
    if decided {
        return Ok(Val::Scalar(Scalar::Bool(l)));
    }
    let r = eval(rhs, ctx)?;
    let Val::Scalar(Scalar::Bool(r)) = r else {
        return Err(ExecError::Integrity {
            message: format!("non-Bool logic operand: {r} (internal error)"),
        });
    };
    Ok(Val::Scalar(Scalar::Bool(r)))
}

pub(crate) fn eval_cast(value: Val, target: TypeName, span: Span) -> Result<Val, ExecError> {
    let scalar = match (value, target) {
        (Val::Scalar(Scalar::Int(n)), TypeName::Int) => Scalar::Int(n),
        (Val::Scalar(Scalar::Int(n)), TypeName::Float) => Scalar::Float(n as f64),
        (Val::Scalar(Scalar::Int(n)), TypeName::Dec) => {
            Scalar::Dec(Decimal::from_i64(n).expect("Decimal fits every i64"))
        }
        // Rust `as` semantics: truncate toward zero, saturate, NaN to zero
        (Val::Scalar(Scalar::Float(x)), TypeName::Int) => Scalar::Int(x as i64),
        (Val::Scalar(Scalar::Float(x)), TypeName::Float) => Scalar::Float(x),
        (Val::Scalar(Scalar::Float(x)), TypeName::Dec) => match Decimal::from_f64(x) {
            Some(d) => Scalar::Dec(d.normalize()),
            None if x.is_nan() => {
                return Err(ExecError::Abort {
                    message: "cannot cast NaN to Dec".to_string(),
                    span,
                });
            }
            None => Scalar::Dec(if x.is_sign_positive() {
                Decimal::MAX
            } else {
                Decimal::MIN
            }),
        },
        (Val::Scalar(Scalar::Dec(d)), TypeName::Int) => {
            Scalar::Int(d.trunc().to_i64().unwrap_or(if d.is_sign_positive() {
                i64::MAX
            } else {
                i64::MIN
            }))
        }
        (Val::Scalar(Scalar::Dec(d)), TypeName::Float) => {
            Scalar::Float(d.to_f64().expect("f64 has a wider range than Decimal"))
        }
        (Val::Scalar(Scalar::Dec(d)), TypeName::Dec) => Scalar::Dec(d),
        (Val::Scalar(Scalar::Bool(b)), TypeName::Int) => Scalar::Int(i64::from(b)),
        (value, target) => {
            return Err(ExecError::Integrity {
                message: format!("mismatched cast operands: {value} as {target} (internal error)"),
            });
        }
    };
    Ok(Val::Scalar(scalar))
}

pub(crate) fn eval_method(
    base: &TypedExpr,
    name: &str,
    args: &[TypedMethodArg],
    optional: bool,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    // `?.` on none returns none without evaluating args
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    if let Some(sort_kind) = SortKind::from_str(name) {
        return eval_sort(sort_kind, base, optional, args, ctx, span);
    }
    if name == "filter" {
        return eval_filter(base, optional, args, ctx);
    }
    if name == "map" {
        return eval_map(base, optional, args, ctx);
    }
    if let Some(kind) = ReduceKind::from_str(name) {
        return eval_reduce(kind, base, optional, ctx, span);
    }
    if let Some(kind) = KeyedReduceKind::from_str(name) {
        return eval_reduce_over(kind, base, optional, args, ctx, span);
    }
    if let Some(opt_kind) = OptMethodKind::from_str(name) {
        return eval_optional(opt_kind, base, optional, args, ctx, span);
    }
    let arg_vals = args
        .iter()
        .map(|arg| eval(&arg.expr, ctx))
        .collect::<Result<Vec<_>, _>>()?;
    if name == "flatten" {
        return eval_flatten(base, optional);
    }
    if matches!(name, "take" | "skip" | "first" | "nth") {
        return eval_slice(name, base, optional, &arg_vals, span);
    }
    if name == "len" {
        match &base {
            Val::List(_) => return eval_list_len(base, optional),
            // String::len falls through
            Val::Scalar(_) => {}
            _ => {
                return Err(ExecError::Integrity {
                    message: format!("`.len()` on unsupported value: {base} (internal error)"),
                });
            }
        }
    }
    if name == "contains" && matches!(&base, Val::List(_)) {
        let [needle] = arg_vals.as_slice() else {
            return Err(ExecError::Integrity {
                message: "`contains()` expects one argument (internal error)".to_string(),
            });
        };
        return eval_list_contains(base, optional, needle);
    }
    let scalar = eval_scalar_method(&base, name, &arg_vals, span)?;
    let value = Val::Scalar(scalar);
    Ok(if optional {
        Val::Some(Box::new(value))
    } else {
        value
    })
}

/// Slice a List according to take/skip/first/nth method
fn eval_slice(
    name: &str,
    base: Val,
    optional: bool,
    args: &[Val],
    span: Span,
) -> Result<Val, ExecError> {
    let mut items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!("`?.{name}()` on non-list value: {other} (internal error)"),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`{name}()` on non-list value: {other} (internal error)"),
            });
        }
    };
    let index_arg = |method: &str| -> Result<i64, ExecError> {
        let [arg] = args else {
            return Err(ExecError::Integrity {
                message: format!("`{method}()` expects one argument (internal error)"),
            });
        };
        match arg {
            Val::Scalar(Scalar::Int(n)) => Ok(*n),
            _ => Err(ExecError::Integrity {
                message: format!("`{method}()` expects an Int argument (internal error)"),
            }),
        }
    };
    let result = match name {
        // take/skip: negative bounds abort
        "take" | "skip" => {
            let n = index_arg(name)?;
            if n < 0 {
                return Err(ExecError::Abort {
                    message: format!("`{name}()` called with negative bound {n}"),
                    span,
                });
            }
            let n = (n as usize).min(items.len());
            if name == "take" {
                items.truncate(n);
            } else {
                items.drain(..n);
            }
            Val::List(items)
        }
        // first: head or None
        "first" => match items.into_iter().next() {
            Some(head) => Val::Some(Box::new(head)),
            None => Val::None,
        },
        // nth: 0-based, negative indices from back, out-of-range is None
        "nth" => {
            let n = index_arg(name)?;
            let len = items.len() as i64;
            let index = if n >= 0 { n } else { len + n };
            if index < 0 || index >= len {
                Val::None
            } else {
                Val::Some(Box::new(items.swap_remove(index as usize)))
            }
        }
        _ => {
            return Err(ExecError::Integrity {
                message: format!("unknown slice method `{name}` (internal error)"),
            });
        }
    };
    Ok(if optional {
        Val::Some(Box::new(result))
    } else {
        result
    })
}

/// Element count of a List
fn eval_list_len(base: Val, optional: bool) -> Result<Val, ExecError> {
    let count = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items.len(),
            other => {
                return Err(ExecError::Integrity {
                    message: format!("`?.len()` on non-list value: {other} (internal error)"),
                });
            }
        },
        (_, Val::List(items)) => items.len(),
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`.len()` on non-list value: {other} (internal error)"),
            });
        }
    };
    let len = Val::Scalar(Scalar::Int(count as i64));
    Ok(if optional {
        Val::Some(Box::new(len))
    } else {
        len
    })
}

/// List membership by element equality
fn eval_list_contains(base: Val, optional: bool, needle: &Val) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!("`?.contains()` on non-list value: {other} (internal error)"),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`.contains()` on non-list value: {other} (internal error)"),
            });
        }
    };
    let found = items.iter().any(|item| item.eq(needle) == Some(true));
    let result = Val::Scalar(Scalar::Bool(found));
    Ok(if optional {
        Val::Some(Box::new(result))
    } else {
        result
    })
}

#[derive(Debug, Clone, Copy)]
enum SortKind {
    Asc,
    Desc,
    AscKeyed,
    DescKeyed,
}

impl SortKind {
    pub fn from_str(name: &str) -> Option<SortKind> {
        Some(match name {
            "sort" | "sort_asc" => SortKind::Asc,
            "sort_desc" => SortKind::Desc,
            "sort_over" | "sort_asc_over" => SortKind::AscKeyed,
            "sort_desc_over" => SortKind::DescKeyed,
            _ => return None,
        })
    }

    pub fn is_keyed(&self) -> bool {
        matches!(self, SortKind::AscKeyed | SortKind::DescKeyed)
    }

    pub fn is_desc(&self) -> bool {
        matches!(self, SortKind::Desc | SortKind::DescKeyed)
    }

    pub fn sort_dir(&self) -> SortDir {
        match self {
            SortKind::Asc | SortKind::AscKeyed => SortDir::Asc,
            SortKind::Desc | SortKind::DescKeyed => SortDir::Desc,
        }
    }
}

impl fmt::Display for SortKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SortKind::Asc => write!(f, "sort_asc"),
            SortKind::Desc => write!(f, "sort_desc"),
            SortKind::AscKeyed => write!(f, "sort_asc_over"),
            SortKind::DescKeyed => write!(f, "sort_desc_over"),
        }
    }
}

/// Sort a List by value or key expressions (stable)
fn eval_sort(
    kind: SortKind,
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let mut items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.{kind}()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`{kind}()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    if kind.is_keyed() {
        sort_by_keys(&mut items, kind, args, ctx, span)?;
    } else if !args.is_empty() {
        return Err(ExecError::Integrity {
            message: format!("`{kind}()` takes no key arguments (internal error)"),
        });
    } else {
        sort_keyless(&mut items, kind, span)?;
    }
    let sorted = Val::List(items);
    Ok(if optional {
        Val::Some(Box::new(sorted))
    } else {
        sorted
    })
}

/// Keyless sort (stable) over values
fn sort_keyless(items: &mut [Val], kind: SortKind, span: Span) -> Result<(), ExecError> {
    let mut result = Ok(());
    items.sort_by(|a, b| match (result.as_ref(), a.cmp(b, span)) {
        (Ok(_), Ok(order)) => {
            if kind.is_desc() {
                order.reverse()
            } else {
                order
            }
        }
        (Ok(_), Err(e)) => {
            result = Err(e);
            std::cmp::Ordering::Equal
        }
        (Err(_), _) => std::cmp::Ordering::Equal,
    });
    result
}

// TODO: batch eval first key
/// Keyed sort (stable) with per-key expressions, directions
fn sort_by_keys(
    items: &mut Vec<Val>,
    kind: SortKind,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<(), ExecError> {
    let mut failure: Option<ExecError> = None;
    let mut items_key_cache: Vec<(Val, RefCell<Vec<Val>>)> = std::mem::take(items)
        .into_iter()
        .map(|item| (item, RefCell::new(Vec::new())))
        .collect();
    items_key_cache.sort_by(|(a, cache_a), (b, cache_b)| {
        if failure.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match compare_keys_eval_cached(a, b, cache_a, cache_b, args, kind.sort_dir(), ctx, span) {
            Ok(order) => order,
            Err(e) => {
                failure = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    *items = items_key_cache.into_iter().map(|(item, _)| item).collect();
    Ok(())
}

/// Compare sort keys, with on-demand evaluation and caching
#[allow(clippy::too_many_arguments)]
fn compare_keys_eval_cached(
    a: &Val,
    b: &Val,
    cache_a: &RefCell<Vec<Val>>,
    cache_b: &RefCell<Vec<Val>>,
    keys: &[TypedMethodArg],
    default_dir: SortDir,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<std::cmp::Ordering, ExecError> {
    for (i, arg) in keys.iter().enumerate() {
        if cache_a.borrow().len() <= i {
            let key = eval_sort_key(ctx, arg, a)?;
            cache_a.borrow_mut().push(key);
        }
        if cache_b.borrow().len() <= i {
            let key = eval_sort_key(ctx, arg, b)?;
            cache_b.borrow_mut().push(key);
        }
        let key_a = &cache_a.borrow()[i];
        let key_b = &cache_b.borrow()[i];
        // Refs are considered undecidable for now
        if key_a.contains_ref() || key_b.contains_ref() {
            continue;
        }
        match key_a.cmp(key_b, span)? {
            std::cmp::Ordering::Equal => continue,
            non_equal => {
                return Ok(
                    if arg.direction.as_deref().copied().unwrap_or(default_dir) == SortDir::Desc {
                        non_equal.reverse()
                    } else {
                        non_equal
                    },
                );
            }
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

// TODO: resolve owning Refs
/// Evaluate one sort key expr against an element scope
fn eval_sort_key(
    ctx: &mut EvalCtx<'_>,
    arg: &TypedMethodArg,
    item: &Val,
) -> Result<Val, ExecError> {
    let map_element = push_element_scope(ctx, item);
    let key = eval(&arg.expr, ctx).map(|key| resolve_base_link(ctx, key));
    pop_element_scope(ctx, map_element);
    key?
}

/// Push a record-fields or `$` element scope;
/// returns whether generic `$`-element was bound
fn push_element_scope(ctx: &mut EvalCtx<'_>, item: &Val) -> bool {
    let (scope, map_element): (HashMap<Rc<str>, Val>, _) = match item {
        Val::Record(record) => (
            record
                .iter()
                .map(|(name, value)| (ctx.interner.intern(name.as_ref()), value.clone()))
                .collect(),
            false,
        ),
        _ => {
            ctx.env.element_depth += 1;
            (
                [(
                    ctx.interner.intern("$".repeat(ctx.env.element_depth)),
                    item.clone(),
                )]
                .into(),
                true,
            )
        }
    };
    ctx.env.push_scope(scope);
    map_element
}

fn pop_element_scope(ctx: &mut EvalCtx<'_>, map_element: bool) {
    ctx.env.pop_scope();
    if map_element {
        ctx.env.element_depth -= 1;
    }
}

/// Keep elements whose predicate evaluates to true, preserving order
fn eval_filter(
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.filter()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`filter()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let [arg] = args else {
        return Err(ExecError::Integrity {
            message: "`filter()` expects one argument (internal error)".to_string(),
        });
    };
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let map_element = push_element_scope(ctx, &item);
        let pred = eval(&arg.expr, ctx).and_then(|pred| resolve_base_link(ctx, pred));
        pop_element_scope(ctx, map_element);
        match pred? {
            Val::Scalar(Scalar::Bool(true)) => kept.push(item),
            Val::Scalar(Scalar::Bool(false)) => {}
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`filter()` predicate evaluated to non-Bool: {other} (internal error)"
                    ),
                });
            }
        }
    }
    let filtered = Val::List(kept);
    Ok(if optional {
        Val::Some(Box::new(filtered))
    } else {
        filtered
    })
}

/// Push a `$`-element scope
fn push_map_scope(ctx: &mut EvalCtx<'_>, item: Val) {
    ctx.env.element_depth += 1;
    let scope: HashMap<Rc<str>, Val> =
        [(ctx.interner.intern("$".repeat(ctx.env.element_depth)), item)].into();
    ctx.env.push_scope(scope);
}

fn pop_map_scope(ctx: &mut EvalCtx<'_>) {
    ctx.env.pop_scope();
    ctx.env.element_depth -= 1;
}

/// Evaluate the map method arg once per element
fn eval_map(
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.map()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`map()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let [arg] = args else {
        return Err(ExecError::Integrity {
            message: "`map()` expects one argument (internal error)".to_string(),
        });
    };
    let mut mapped = Vec::with_capacity(items.len());
    for item in items {
        push_map_scope(ctx, item);
        let value = eval(&arg.expr, ctx);
        pop_map_scope(ctx);
        mapped.push(value?);
    }
    let mapped = Val::List(mapped);
    Ok(if optional {
        Val::Some(Box::new(mapped))
    } else {
        mapped
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ReduceKind {
    Sum,
    Avg,
    Max,
    Min,
}

impl ReduceKind {
    pub fn from_str(name: &str) -> Option<ReduceKind> {
        Some(match name {
            "sum" => ReduceKind::Sum,
            "avg" => ReduceKind::Avg,
            "max" => ReduceKind::Max,
            "min" => ReduceKind::Min,
            _ => return None,
        })
    }
}

impl fmt::Display for ReduceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReduceKind::Sum => write!(f, "sum"),
            ReduceKind::Avg => write!(f, "avg"),
            ReduceKind::Max => write!(f, "max"),
            ReduceKind::Min => write!(f, "min"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyedReduceKind {
    SumOver,
    AvgOver,
    MaxBy,
    MinBy,
}

impl KeyedReduceKind {
    pub fn from_str(name: &str) -> Option<KeyedReduceKind> {
        Some(match name {
            "sum_over" => KeyedReduceKind::SumOver,
            "avg_over" => KeyedReduceKind::AvgOver,
            "max_by" => KeyedReduceKind::MaxBy,
            "min_by" => KeyedReduceKind::MinBy,
            _ => return None,
        })
    }
}

impl fmt::Display for KeyedReduceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyedReduceKind::SumOver => write!(f, "sum_over"),
            KeyedReduceKind::AvgOver => write!(f, "avg_over"),
            KeyedReduceKind::MaxBy => write!(f, "max_by"),
            KeyedReduceKind::MinBy => write!(f, "min_by"),
        }
    }
}

/// Fold a list to one value
fn eval_reduce(
    kind: ReduceKind,
    base: Val,
    optional: bool,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.{kind}()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`{kind}()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let config = &ctx.schema.config;
    let reduced = if items.is_empty() {
        Val::None
    } else {
        match kind {
            ReduceKind::Sum => Val::Scalar(sum_scalars(
                &collect_reduction_scalars(items, kind)?,
                config,
                span,
            )?),
            ReduceKind::Avg => Val::Scalar(avg_scalars(
                &collect_reduction_scalars(items, kind)?,
                config,
                span,
            )?),
            ReduceKind::Max => extremum(items, std::cmp::Ordering::Greater, span)?,
            ReduceKind::Min => extremum(items, std::cmp::Ordering::Less, span)?,
        }
    };
    let result = match reduced {
        Val::None => Val::None,
        value => Val::Some(Box::new(value)),
    };
    Ok(if optional {
        Val::Some(Box::new(result))
    } else {
        result
    })
}

/// Fold a list to one value by a per-element key
fn eval_reduce_over(
    kind: KeyedReduceKind,
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!(
                        "`?.{kind}()` called on non-list value: {other} (internal error)"
                    ),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`{kind}()` called on non-list value: {other} (internal error)"),
            });
        }
    };
    let [arg] = args else {
        return Err(ExecError::Integrity {
            message: format!("`{kind}()` expects one key argument (internal error)"),
        });
    };
    let config = &ctx.schema.config;
    let reduced = if items.is_empty() {
        Val::None
    } else {
        match kind {
            KeyedReduceKind::SumOver | KeyedReduceKind::AvgOver => {
                let mut keys = Vec::with_capacity(items.len());
                for item in &items {
                    let map_element = push_element_scope(ctx, item);
                    let key = eval(&arg.expr, ctx).and_then(|key| resolve_base_link(ctx, key));
                    pop_element_scope(ctx, map_element);
                    match key? {
                        Val::Scalar(scalar) => keys.push(scalar),
                        other => {
                            return Err(ExecError::Integrity {
                                message: format!(
                                    "`{kind}()` key evaluated to non-scalar: {other} (internal error)"
                                ),
                            });
                        }
                    }
                }
                let reduced = match kind {
                    KeyedReduceKind::SumOver => sum_scalars(&keys, config, span)?,
                    _ => avg_scalars(&keys, config, span)?,
                };
                Val::Some(Box::new(Val::Scalar(reduced)))
            }
            KeyedReduceKind::MaxBy | KeyedReduceKind::MinBy => {
                let want = if kind == KeyedReduceKind::MaxBy {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
                let mut best: Option<(Val, Val)> = None;
                for item in items {
                    let map_element = push_element_scope(ctx, &item);
                    let key = eval(&arg.expr, ctx).and_then(|key| resolve_base_link(ctx, key));
                    pop_element_scope(ctx, map_element);
                    let key = key?;
                    if key.contains_ref() {
                        continue;
                    }
                    let replace = match &best {
                        None => true,
                        Some((_, best_key)) => key.cmp(best_key, span)? == want,
                    };
                    if replace {
                        best = Some((item, key));
                    }
                }
                best.map_or(Val::None, |(item, _)| Val::Some(Box::new(item)))
            }
        }
    };
    Ok(if optional {
        Val::Some(Box::new(reduced))
    } else {
        reduced
    })
}

fn collect_reduction_scalars(items: Vec<Val>, kind: ReduceKind) -> Result<Vec<Scalar>, ExecError> {
    items
        .into_iter()
        .map(|item| match item {
            Val::Scalar(scalar) => Ok(scalar),
            other => Err(ExecError::Integrity {
                message: format!("`{kind}()` over non-scalar element: {other} (internal error)"),
            }),
        })
        .collect()
}

fn extremum(mut items: Vec<Val>, want: std::cmp::Ordering, span: Span) -> Result<Val, ExecError> {
    if items.is_empty() {
        return Err(ExecError::Integrity {
            message: "extremum over no values (internal error)".to_string(),
        });
    }
    let mut best_index = 0;
    for (index, item) in items[1..].iter().enumerate() {
        if item.cmp(&items[best_index], span)? == want {
            best_index = index + 1;
        }
    }
    Ok(items.swap_remove(best_index))
}

/// Sum non-empty same-typed scalars with config arithmetic
fn sum_scalars(values: &[Scalar], config: &Config, span: Span) -> Result<Scalar, ExecError> {
    let mut total = values
        .first()
        .cloned()
        .ok_or_else(|| ExecError::Integrity {
            message: "`sum()` over no values (internal error)".to_string(),
        })?;
    for value in &values[1..] {
        total = match (total, value) {
            (Scalar::Int(a), Scalar::Int(b)) => {
                int_arith(a, ArithOp::Add, *b, config.int_arithmetic, span)?
            }
            (Scalar::Float(a), Scalar::Float(b)) => {
                float_arith(a, ArithOp::Add, *b, config.float_checks, span)?
            }
            (Scalar::Dec(a), Scalar::Dec(b)) => {
                dec_arith(a, ArithOp::Add, *b, config.dec_arithmetic, span)?
            }
            (Scalar::Duration(a), Scalar::Duration(b)) => {
                Scalar::Duration(a.checked_add(b).ok_or_else(|| ExecError::Abort {
                    message: format!("duration overflow: {a} + {b}"),
                    span,
                })?)
            }
            _ => {
                return Err(ExecError::Integrity {
                    message: "`sum()` over mismatched values (internal error)".to_string(),
                });
            }
        };
    }
    Ok(total)
}

/// Average non-empty same-typed scalars with config arithmetic
fn avg_scalars(values: &[Scalar], config: &Config, span: Span) -> Result<Scalar, ExecError> {
    let first = values.first().ok_or_else(|| ExecError::Integrity {
        message: "`avg()` over no values (internal error)".to_string(),
    })?;
    let n = values.len();
    match first {
        Scalar::Instant(_) => return avg_instants(values, span),
        Scalar::Int(first) => {
            let mut total = *first as i128;
            for value in &values[1..] {
                let Scalar::Int(summand) = value else {
                    return Err(ExecError::Integrity {
                        message: "`avg()` over mismatched values (internal error)".to_string(),
                    });
                };
                total += *summand as i128;
            }
            let sum = total / n as i128;
            return Ok(Scalar::Int(sum as i64));
        }
        _ => {}
    }
    let sum = sum_scalars(values, config, span)?;
    match sum {
        Scalar::Float(total) => {
            float_arith(total, ArithOp::Div, n as f64, config.float_checks, span)
        }
        Scalar::Dec(total) => dec_arith(
            total,
            ArithOp::Div,
            Decimal::from(n as u64),
            config.dec_arithmetic,
            span,
        ),
        Scalar::Duration(total) => scale_duration(total, Decimal::from(n as u64), true, span),
        _ => Err(ExecError::Integrity {
            message: "`avg()` over mismatched values (internal error)".to_string(),
        }),
    }
}

/// Average instants by computing average offset from first
fn avg_instants(values: &[Scalar], span: Span) -> Result<Scalar, ExecError> {
    let Some(Scalar::Instant(start)) = values.first() else {
        return Err(ExecError::Integrity {
            message: "`avg()` over no values (internal error)".to_string(),
        });
    };
    let mut total_offset = TimeDelta::zero();
    for value in &values[1..] {
        let Scalar::Instant(instant) = value else {
            return Err(ExecError::Integrity {
                message: "`avg()` over mismatched values (internal error)".to_string(),
            });
        };
        total_offset = total_offset
            .checked_add(&instant.signed_duration_since(*start))
            .ok_or_else(|| ExecError::Abort {
                message: format!("duration overflow averaging instants around {start}"),
                span,
            })?;
    }
    let Scalar::Duration(mean_offset) =
        scale_duration(total_offset, Decimal::from(values.len() as u64), true, span)?
    else {
        return Err(ExecError::Integrity {
            message: "`avg()` offset duration was not a duration (internal error)".to_string(),
        });
    };
    start
        .checked_add_signed(mean_offset)
        .map(Scalar::Instant)
        .ok_or_else(|| ExecError::Abort {
            message: format!("instant overflow: {start} + {mean_offset}"),
            span,
        })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OptMethodKind {
    IsSome,
    IsNone,
    Unwrap,
    UnwrapOr,
}

impl OptMethodKind {
    pub fn from_str(name: &str) -> Option<OptMethodKind> {
        Some(match name {
            "is_some" => OptMethodKind::IsSome,
            "is_none" => OptMethodKind::IsNone,
            "unwrap" => OptMethodKind::Unwrap,
            "unwrap_or" => OptMethodKind::UnwrapOr,
            _ => return None,
        })
    }
}

impl fmt::Display for OptMethodKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptMethodKind::IsSome => write!(f, "is_some"),
            OptMethodKind::IsNone => write!(f, "is_none"),
            OptMethodKind::Unwrap => write!(f, "unwrap"),
            OptMethodKind::UnwrapOr => write!(f, "unwrap_or"),
        }
    }
}

/// Collapse or default an optional
fn eval_optional(
    kind: OptMethodKind,
    base: Val,
    optional: bool,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    if !args.is_empty() && kind != OptMethodKind::UnwrapOr {
        return Err(ExecError::Integrity {
            message: format!("`{kind}()` takes no arguments (internal error)"),
        });
    }
    if kind == OptMethodKind::UnwrapOr && args.len() != 1 {
        return Err(ExecError::Integrity {
            message: "`unwrap_or()` expects one argument (internal error)".to_string(),
        });
    }
    // dispatch already unwraps outer optional on `?.`
    let result = match base {
        Val::None => eval_opt_absent(kind, args, ctx, span)?,
        Val::Some(v) => eval_opt_present(kind, *v),
        _ => {
            return Err(ExecError::Integrity {
                message: format!(
                    "`{kind}()` called on non-optional value: {base} (internal error)"
                ),
            });
        }
    };
    Ok(if optional {
        Val::Some(Box::new(result))
    } else {
        result
    })
}

fn eval_opt_present(kind: OptMethodKind, value: Val) -> Val {
    match kind {
        OptMethodKind::IsSome => Val::Scalar(Scalar::Bool(true)),
        OptMethodKind::IsNone => Val::Scalar(Scalar::Bool(false)),
        _ => value,
    }
}

fn eval_opt_absent(
    kind: OptMethodKind,
    args: &[TypedMethodArg],
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    match kind {
        OptMethodKind::IsSome => Ok(Val::Scalar(Scalar::Bool(false))),
        OptMethodKind::IsNone => Ok(Val::Scalar(Scalar::Bool(true))),
        OptMethodKind::Unwrap => Err(ExecError::Abort {
            message: "`unwrap()` called on `none`".to_string(),
            span,
        }),
        OptMethodKind::UnwrapOr => {
            let [arg] = args else {
                return Err(ExecError::Integrity {
                    message: "`unwrap_or()` expects one argument (internal error)".to_string(),
                });
            };
            Ok(eval_opt_present(kind, eval(&arg.expr, ctx)?))
        }
    }
}

/// Concatenate one nesting level
fn eval_flatten(base: Val, optional: bool) -> Result<Val, ExecError> {
    let items = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => match *v {
            Val::List(items) => items,
            other => {
                return Err(ExecError::Integrity {
                    message: format!("`?.flatten()` on non-list value: {other} (internal error)"),
                });
            }
        },
        (_, Val::List(items)) => items,
        (_, other) => {
            return Err(ExecError::Integrity {
                message: format!("`flatten()` on non-list value: {other} (internal error)"),
            });
        }
    };
    let mut flat = Vec::new();
    for item in items {
        match item {
            Val::List(inner) => flat.extend(inner),
            other => {
                return Err(ExecError::Integrity {
                    message: format!("`flatten()` over non-list element: {other} (internal error)"),
                });
            }
        }
    }
    let flat = Val::List(flat);
    Ok(if optional {
        Val::Some(Box::new(flat))
    } else {
        flat
    })
}

fn method_arg<'a>(args: &'a [Val], method: &str, index: usize) -> Result<&'a Val, ExecError> {
    args.get(index).ok_or_else(|| ExecError::Integrity {
        message: format!("missing argument {index} for `{method}` (internal error)"),
    })
}

fn method_string_arg<'a>(args: &'a [Val], method: &str) -> Result<&'a str, ExecError> {
    let Val::Scalar(Scalar::String(s)) = method_arg(args, method, 0)? else {
        return Err(ExecError::Integrity {
            message: format!("`{method}` expects a String argument (internal error)"),
        });
    };
    Ok(s.as_str())
}

fn method_int_arg(args: &[Val], method: &str) -> Result<i64, ExecError> {
    let Val::Scalar(Scalar::Int(n)) = method_arg(args, method, 0)? else {
        return Err(ExecError::Integrity {
            message: format!("`{method}` expects an Int argument (internal error)"),
        });
    };
    Ok(*n)
}

fn second_dec(dt: &DateTime<Utc>) -> Decimal {
    Decimal::new(
        dt.second() as i64 * 1_000_000_000 + dt.nanosecond() as i64,
        9,
    )
    .normalize()
}

fn epoch_dec(dt: &DateTime<Utc>) -> Decimal {
    Decimal::from_i128_with_scale(
        dt.timestamp() as i128 * 1_000_000_000 + dt.nanosecond() as i128,
        9,
    )
    .normalize()
}

fn duration_as(total_nanos: Decimal, divisor: i64) -> Scalar {
    Scalar::Dec(
        total_nanos
            .checked_div(Decimal::from(divisor))
            .expect("as_* divides nanos by a nonzero constant")
            .normalize(),
    )
}

fn eval_scalar_method(
    base: &Val,
    name: &str,
    args: &[Val],
    span: Span,
) -> Result<Scalar, ExecError> {
    let Val::Scalar(base) = base else {
        return Err(ExecError::Integrity {
            message: format!("method `{}` on unsupported value (internal error)", name),
        });
    };
    match (base, name) {
        (Scalar::String(s), "contains") => {
            Ok(Scalar::Bool(s.contains(method_string_arg(args, name)?)))
        }
        (Scalar::String(s), "starts_with") => {
            Ok(Scalar::Bool(s.starts_with(method_string_arg(args, name)?)))
        }
        (Scalar::String(s), "ends_with") => {
            Ok(Scalar::Bool(s.ends_with(method_string_arg(args, name)?)))
        }
        (Scalar::String(s), "to_upper") => Ok(Scalar::String(s.to_uppercase())),
        (Scalar::String(s), "to_lower") => Ok(Scalar::String(s.to_lowercase())),
        (Scalar::String(s), "len") => Ok(Scalar::Int(s.chars().count() as i64)),
        (Scalar::Instant(t), "year") => Ok(Scalar::Int(t.year() as i64)),
        (Scalar::Instant(t), "month") => Ok(Scalar::Int(t.month() as i64)),
        (Scalar::Instant(t), "day") => Ok(Scalar::Int(t.day() as i64)),
        (Scalar::Instant(t), "hour") => Ok(Scalar::Int(t.hour() as i64)),
        (Scalar::Instant(t), "minute") => Ok(Scalar::Int(t.minute() as i64)),
        (Scalar::Instant(t), "weekday") => {
            Ok(Scalar::Int(t.weekday().num_days_from_sunday() as i64))
        }
        (Scalar::Instant(t), "second") => Ok(Scalar::Dec(second_dec(t))),
        (Scalar::Instant(t), "epoch") => Ok(Scalar::Dec(epoch_dec(t))),
        (Scalar::Instant(t), "to_rfc3339") => Ok(Scalar::String(
            t.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )),
        (Scalar::Duration(d), "as_seconds") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_SEC as i64,
        )),
        (Scalar::Duration(d), "as_minutes") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_MIN as i64,
        )),
        (Scalar::Duration(d), "as_hours") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_HOUR as i64,
        )),
        (Scalar::Duration(d), "as_days") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_DAY as i64,
        )),
        (Scalar::Dec(d), "round_dp") => {
            let n = method_int_arg(args, name)?;
            if n < 0 {
                return Err(ExecError::Abort {
                    message: format!("round_dp needs non-negative digits, got {n}"),
                    span,
                });
            }
            if n > 28 {
                Ok(Scalar::Dec(*d))
            } else {
                Ok(Scalar::Dec(
                    d.round_dp_with_strategy(n as u32, RoundingStrategy::MidpointNearestEven)
                        .normalize(),
                ))
            }
        }
        (scalar, "to_string")
            if matches!(
                scalar,
                Scalar::Int(_) | Scalar::Float(_) | Scalar::Bool(_) | Scalar::Dec(_)
            ) =>
        {
            Ok(Scalar::String(scalar.to_string()))
        }
        (Scalar::Float(x), "round") => Ok(Scalar::Float(x.round())),
        (Scalar::Float(x), "floor") => Ok(Scalar::Float(x.floor())),
        (Scalar::Float(x), "ceil") => Ok(Scalar::Float(x.ceil())),
        _ => Err(ExecError::Integrity {
            message: format!(
                "unknown method `{name}` for {} (internal error)",
                base.kind_str(),
            ),
        }),
    }
}

pub(crate) fn decode_literal(lit: &Spanned<Literal>, now: DateTime<Utc>) -> Val {
    match &lit.value {
        Literal::Int(n) => Val::Scalar(Scalar::Int(*n)),
        Literal::Float(x) => Val::Scalar(Scalar::Float(*x)),
        Literal::Dec(d) => Val::Scalar(Scalar::Dec(*d)),
        Literal::String(s) => Val::Scalar(Scalar::String(s.clone())),
        Literal::Bool(b) => Val::Scalar(Scalar::Bool(*b)),
        Literal::Instant(t) => Val::Scalar(Scalar::Instant(*t)),
        Literal::Duration(d) => Val::Scalar(Scalar::Duration(*d)),
        Literal::None => Val::None,
        Literal::Now => Val::Scalar(Scalar::Instant(now)),
        Literal::Today(time) => Val::Scalar(Scalar::Instant(
            now.date_naive()
                .and_time(time.unwrap_or(MIDNIGHT))
                .and_utc(),
        )),
    }
}

pub(crate) fn eval_type_constant(ty: TypeName, name: ConstantName) -> Val {
    let scalar = match (ty, name) {
        (TypeName::Int, ConstantName::Min) => Scalar::Int(i64::MIN),
        (TypeName::Int, ConstantName::Max) => Scalar::Int(i64::MAX),
        (TypeName::Float, ConstantName::Min) => Scalar::Float(f64::MIN),
        (TypeName::Float, ConstantName::Max) => Scalar::Float(f64::MAX),
        (TypeName::Dec, ConstantName::Min) => Scalar::Dec(Decimal::MIN),
        (TypeName::Dec, ConstantName::Max) => Scalar::Dec(Decimal::MAX),
    };
    Val::Scalar(scalar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};
    use chrono::{DateTime, TimeDelta};
    use rust_decimal::{Decimal, dec};

    #[test]
    fn method_flatten() {
        match run_seeded("users.orders.flatten()") {
            Ok(Val::List(orders)) => {
                let amounts: Vec<i64> = orders
                    .iter()
                    .map(|order| match order {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "amount") {
                                Some((_, Val::Scalar(Scalar::Int(n)))) => *n,
                                got => panic!("expected amount, got {got:?}"),
                            }
                        }
                        other => panic!("expected order record, got {other}"),
                    })
                    .collect();
                assert_eq!(amounts, [100, 200, 400]);
            }
            got => panic!("expected flat orders, got {got:?}"),
        }
    }

    #[test]
    fn eval_literals() {
        assert_eq!(run_src("42"), Ok(int(42)));
        assert_eq!(run_src("1.5f"), Ok(float(1.5)));
        assert_eq!(run_src("30.00"), Ok(dec(dec!(30.00))));
        assert_eq!(run_src("\"hi\""), Ok(str("hi")));
        assert_eq!(run_src("true"), Ok(bool(true)));
        assert_eq!(run_src("some(1)"), Ok(Val::Some(Box::new(int(1)))),);
    }

    #[test]
    fn eval_temporal_literals() {
        assert_eq!(run_src("@now"), Ok(Val::Scalar(Scalar::Instant(FIXED_NOW))));
        let midnight = FIXED_NOW.date_naive().and_time(MIDNIGHT).and_utc();
        assert_eq!(
            run_src("@today"),
            Ok(Val::Scalar(Scalar::Instant(midnight)))
        );
        let noon = FIXED_NOW
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(
            run_src("@today_12:00"),
            Ok(Val::Scalar(Scalar::Instant(noon)))
        );
        assert_eq!(
            run_src("@2026-10-20"),
            Ok(Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(1792454400, 0).unwrap()
            )))
        );
        assert_eq!(
            run_src("#90s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(90))))
        );
    }

    #[test]
    fn eval_type_constants() {
        assert_eq!(run_src("Int::MIN"), Ok(int(i64::MIN)));
        assert_eq!(run_src("Float::MAX"), Ok(float(f64::MAX)));
        assert_eq!(run_src("Dec::MIN"), Ok(dec(Decimal::MIN)));
        assert_eq!(run_src("Dec::MAX"), Ok(dec(Decimal::MAX)));
    }

    #[test]
    fn eval_int_arithmetic() {
        assert_eq!(run_src("1 + 2"), Ok(int(3)));
        assert_eq!(run_src("7 - 10"), Ok(int(-3)));
        assert_eq!(run_src("6 * 7"), Ok(int(42)));
        assert_eq!(run_src("7 / 2"), Ok(int(3)));
        assert_eq!(run_src("-7 / 2"), Ok(int(-3)));
        assert_eq!(run_src("-7 % 3"), Ok(int(-1)));
        assert_eq!(run_src("7 % -3"), Ok(int(1)));
        assert_eq!(run_src("-7 mod 3"), Ok(int(2)));
        assert_eq!(run_src("7 mod -3"), Ok(int(1)));
        assert_eq!(run_src("-5"), Ok(int(-5)));
        assert_eq!(run_src("--5"), Ok(int(5)));
    }

    #[test]
    fn eval_int_overflow_checked() {
        for src in [
            "Int::MAX + 1",
            "Int::MAX * 2",
            "Int::MIN - 1",
            "Int::MIN / -1",
            "-Int::MIN",
        ] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("integer overflow"), "{src}: {err}");
        }
        for src in ["1 / 0", "1 % 0", "1 mod 0"] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("zero"), "{src}: {err}");
        }
    }

    #[test]
    fn eval_int_saturating() {
        let sat = r#"config { int_arithmetic = "saturating" }"#;
        assert_eq!(run_src_config("Int::MAX + 1", sat), Ok(int(i64::MAX)));
        assert_eq!(run_src_config("Int::MIN - 1", sat), Ok(int(i64::MIN)));
        assert_eq!(run_src_config("Int::MIN / -1", sat), Ok(int(i64::MAX)));
        assert_eq!(run_src_config("-Int::MIN", sat), Ok(int(i64::MAX)));
        assert!(
            run_src_config("1 / 0", sat)
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_int_wrapping() {
        let wrap = r#"config { int_arithmetic = "wrapping" }"#;
        assert_eq!(run_src_config("Int::MAX + 1", wrap), Ok(int(i64::MIN)));
        assert_eq!(run_src_config("Int::MIN / -1", wrap), Ok(int(i64::MIN)));
        assert!(
            run_src_config("1 % 0", wrap)
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_float_arithmetic() {
        assert_eq!(run_src("1.5f + 2.25f"), Ok(float(3.75)));
        assert_eq!(run_src("10.0f / 4.0f"), Ok(float(2.5)));
        assert_eq!(run_src("7.5f % 2.0f"), Ok(float(1.5)));
        assert_eq!(run_src("5.5f mod 2.0f"), Ok(float(1.5)));
        assert_eq!(run_src("-7.5f mod 2.0f"), Ok(float(0.5)));
        assert_eq!(run_src("-0.0f"), Ok(float(0.0)));
    }

    #[test]
    fn eval_float_checks() {
        for src in ["1.0f / 0.0f", "0.0f / 0.0f", "1e308f * 10.0f"] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("non-finite"), "{src}: {err}");
        }
        let off = r#"config { float_checks = false }"#;
        assert_eq!(run_src_config("1.0f / 0.0f", off), Ok(float(f64::INFINITY)));
        match run_src_config("0.0f / 0.0f", off) {
            Ok(Val::Scalar(Scalar::Float(x))) => assert!(x.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
    }

    #[test]
    fn eval_dec_arithmetic() {
        assert_eq!(run_src("1.5 + 2.5"), Ok(dec(dec!(4.0))));
        assert_eq!(run_src("5.00 / 2.0"), Ok(dec(dec!(2.5))));
        assert_eq!(
            run_src("1.0 / 3.0"),
            Ok(dec(dec!(0.3333333333333333333333333333)))
        );
        assert_eq!(run_src("1.10 + 2.20"), Ok(dec(dec!(3.3))));
        assert_eq!(run_src("-7.0 % 3.0"), Ok(dec(dec!(-1))));
        assert_eq!(run_src("-7.0 mod 3.0"), Ok(dec(dec!(2))));
        assert_eq!(run_src("7.0 mod -3.0"), Ok(dec(dec!(1))));
        assert_eq!(run_src("-7.0 mod -3.0"), Ok(dec(dec!(2))));
    }

    #[test]
    fn eval_dec_overflow_checked() {
        for src in [
            "Dec::MAX + 1.0",
            "Dec::MAX * 10.0",
            "Dec::MIN - 1.0",
            "1.0 / 0.0",
            "1.0 % 0.0",
        ] {
            let err = run_src(src).unwrap_err();
            assert!(
                err.to_string().contains("overflow") || err.to_string().contains("zero"),
                "{src}: {err}"
            );
        }
    }

    #[test]
    fn eval_dec_saturating() {
        let sat = r#"config { dec_arithmetic = "saturating" }"#;
        assert_eq!(run_src_config("Dec::MAX + 1.0", sat), Ok(dec(Decimal::MAX)));
        assert_eq!(run_src_config("Dec::MIN - 1.0", sat), Ok(dec(Decimal::MIN)));
        assert_eq!(
            run_src_config("Dec::MAX * 10.0", sat),
            Ok(dec(Decimal::MAX))
        );
        assert!(
            run_src_config("1.0 / 0.0", sat)
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_temporal_arithmetic() {
        let td_30d = TimeDelta::days(30);
        let plus_30d = FIXED_NOW + td_30d;
        assert_eq!(
            run_src("@2026-10-20 + #30d"),
            Ok(Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(1792454400, 0).unwrap() + td_30d
            )))
        );
        assert_eq!(
            run_src("@now + #30d"),
            Ok(Val::Scalar(Scalar::Instant(plus_30d)))
        );
        assert_eq!(
            run_src("@now - @2026-10-20"),
            Ok(Val::Scalar(Scalar::Duration(
                FIXED_NOW.signed_duration_since(DateTime::from_timestamp(1792454400, 0).unwrap())
            )))
        );
        assert_eq!(run_src("#75s / #30s"), Ok(dec(dec!(2.5))));
        assert_eq!(
            run_src("-#90s % #7s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(-6))))
        );
        assert_eq!(
            run_src("-#90s mod #7s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(1))))
        );
        assert_eq!(
            run_src("#30d * 3"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(90))))
        );
        assert_eq!(
            run_src("3 * #30d"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(90))))
        );
        assert_eq!(
            run_src("#30d * 1.5"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(45))))
        );
        assert_eq!(
            run_src("2.0f * #30s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(60))))
        );
        assert_eq!(
            run_src("#30d / 2"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(15))))
        );
        assert_eq!(
            run_src("-#30d"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(-30))))
        );
        assert_eq!(
            run_src("#5s * 0"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::zero())))
        );
    }

    #[test]
    fn eval_temporal_overflow() {
        let err = run_src("@262142-12-31 + #1d").unwrap_err();
        assert!(err.to_string().contains("instant overflow"), "{err}");
        let err = run_src("@262142-12-31_23:59:59.999999999 + #0.000000001s").unwrap_err();
        assert!(err.to_string().contains("instant overflow"), "{err}");
        let err = run_src("#100000000000d + #100000000000d").unwrap_err();
        assert!(err.to_string().contains("duration overflow"), "{err}");
        let err = run_src("#100000000000d * 100000000000").unwrap_err();
        assert!(err.to_string().contains("duration overflow"), "{err}");
        for src in ["#90s / #0s", "#5s % #0s", "#5s mod #0s", "#5s / 0"] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("zero"), "{src}: {err}");
        }
    }

    #[test]
    fn eval_ordered_comparisons() {
        for (src, expected) in [
            ("1 < 2", true),
            ("2 <= 2", true),
            ("3 > 4", false),
            ("2 >= 3", false),
            ("1 < 1", false),
            ("1.5f < 2.0f", true),
            ("2.0f <= 2.0f", true),
            ("1.5 < 2.5", true),
            ("30.00 < 30.01", true),
            ("\"a\" < \"b\"", true),
            ("\"b\" <= \"b\"", true),
            ("\"abc\" > \"abd\"", false),
            ("false < true", true),
            ("true <= true", true),
            ("@now < @3000-01-02", true),
            ("#30s > #1s", true),
            ("#30s >= #30s", true),
        ] {
            assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
        }
    }

    #[test]
    fn eval_ordered_tuples() {
        use std::cmp::Ordering;
        let span = Span::new(0, 0);
        assert_eq!(
            Val::Tuple(vec![int(1), str("a")]).cmp(&Val::Tuple(vec![int(1), str("b")]), span),
            Ok(Ordering::Less)
        );
        assert_eq!(
            Val::Tuple(vec![int(1), str("a")]).cmp(&Val::Tuple(vec![int(1), str("a")]), span),
            Ok(Ordering::Equal)
        );
        assert_eq!(
            Val::Tuple(vec![int(2), str("a")]).cmp(&Val::Tuple(vec![int(1), str("z")]), span),
            Ok(Ordering::Greater)
        );
        assert!(
            Val::Tuple(vec![int(1)])
                .cmp(&Val::Tuple(vec![int(1), str("a")]), span)
                .is_err()
        );
    }

    #[test]
    fn eval_ordered_nan_aborts() {
        let off = r#"config { float_checks = false }"#;
        let err = run_src_config("(0.0f / 0.0f) < 1.0f", off).unwrap_err();
        assert!(err.to_string().contains("NaN"), "{err}");
    }

    #[test]
    fn eval_equality() {
        for (src, expected) in [
            ("1 == 1", true),
            ("1 != 2", true),
            ("1 == 2", false),
            ("1.5f == 1.5f", true),
            ("\"a\" == \"a\"", true),
            ("\"a\" != \"b\"", true),
            ("true == true", true),
            ("true != false", true),
            ("30.00 == 30.0", true),
            ("@now == @now", true),
            ("#30s == #30s", true),
            ("some(1) == some(1)", true),
            ("some(1) != some(2)", true),
            ("none == some(2)", false),
            ("some(some(\"a\")) != some(none)", true),
        ] {
            assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
        }
    }

    #[test]
    fn eval_equality_nan() {
        let off = r#"config { float_checks = false }"#;
        assert_eq!(
            run_src_config("(0.0f / 0.0f) == (0.0f / 0.0f)", off),
            Ok(bool(false))
        );
        assert_eq!(
            run_src_config("(0.0f / 0.0f) != (0.0f / 0.0f)", off),
            Ok(bool(true))
        );
    }

    #[test]
    fn eval_equality_structural() {
        assert_eq!(Val::None.eq(&Val::None), Some(true));
        assert_eq!(
            Val::List(vec![int(1), Val::None]).eq(&Val::List(vec![int(1), Val::None])),
            Some(true)
        );
        assert_eq!(
            Val::List(vec![int(1)]).eq(&Val::List(vec![int(1), int(2)])),
            Some(false)
        );
        assert_eq!(
            Val::Scalar(Scalar::Int(1)).eq(&Val::Scalar(Scalar::Bool(true))),
            None
        );
    }

    #[test]
    fn eval_in() {
        for (src, expected) in [
            ("1 in (1, 2, false)", true),
            ("9 in (1, 2, 3)", false),
            ("\"a\" in ([true], 1.0, \"a\")", true),
            ("\"z\" in (\"a\", 0.0f)", false),
            ("2 in (1, \"a\")", false),
            ("\"b\" in (1, \"a\")", false),
            ("some(1) in (some(1), 2)", true),
            ("some(3) in (some(2), 3)", false),
            ("{ b = 2, a = 1 } in ({ a = 1, b = 2 }, { a = 0 })", true),
            ("{ b = 2, a = 9 } in ({ a = 1, b = 2 }, { a = 0 })", false),
        ] {
            assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
        }
    }

    #[test]
    fn eval_in_composite() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(names("users[id in (1, 3)]"), [str("Ada"), str("Cyd")]);
        assert_eq!(
            names("users[name in (\"Ada\", \"Eli\")]"),
            [str("Ada"), str("Eli")]
        );
        assert_eq!(names("users[id in (2, \"x\")]"), [str("Bob")]);
        assert_eq!(
            names(
                "users[if nickname.is_some() {nickname.unwrap() in (\"A\", \"C\")} else {false}]"
            ),
            [str("Ada"), str("Cyd")]
        );
        assert_eq!(
            names("users[nickname in (none, some(\"A\"))]"),
            [str("Ada"), str("Bob"), str("Dan")]
        );
    }

    #[test]
    fn eval_logic() {
        for (src, expected) in [
            ("true && true", true),
            ("true && false", false),
            ("false && true", false),
            ("true || false", true),
            ("false || false", false),
            ("true || false && false", true),
            ("(true || false) && false", false),
            ("!false", true),
            ("!!true", true),
            ("!(1 != 2)", false),
        ] {
            assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
        }
    }

    #[test]
    fn eval_logic_short_circuit() {
        assert_eq!(run_src("false && (1 / 0 == 0)"), Ok(bool(false)));
        assert_eq!(run_src("true || (1 / 0 == 0)"), Ok(bool(true)));
        assert!(
            run_src("true && (1 / 0 == 0)")
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_casts() {
        for (src, expected) in [
            ("5 as Float", float(5.0)),
            ("5 as Dec", dec(dec!(5))),
            ("5 as Int", int(5)),
            ("1.9f as Int", int(1)),
            ("-1.9f as Int", int(-1)),
            ("1e300f as Int", int(i64::MAX)),
            ("(-1e300f) as Int", int(i64::MIN)),
            ("1.5f as Dec", dec(dec!(1.5))),
            ("1e300f as Dec", dec(Decimal::MAX)),
            ("5.9 as Int", int(5)),
            ("-5.9 as Int", int(-5)),
            ("30.00 as Int", int(30)),
            ("Dec::MAX as Int", int(i64::MAX)),
            ("(-Dec::MAX) as Int", int(i64::MIN)),
            ("-Dec::MAX as Int", int(i64::MIN + 1)),
            ("Dec::MIN as Float", float(Decimal::MIN.to_f64().unwrap())),
            ("5.5 as Float", float(5.5)),
            ("true as Int", int(1)),
            ("false as Int", int(0)),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_cast_nan_to_dec_aborts() {
        let off = r#"config { float_checks = false }"#;
        let err = run_src_config("(0.0f / 0.0f) as Dec", off).unwrap_err();
        assert!(err.to_string().contains("NaN"), "{err}");
    }

    #[test]
    fn method_take_skip() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(names("users.take(2)"), [str("Ada"), str("Bob")]);
        assert_eq!(names("users.skip(3)"), [str("Dan"), str("Eli")]);
        assert_eq!(names("users.take(0)"), Vec::new());
        assert_eq!(names("users.skip(5)"), Vec::new());
        assert_eq!(names("users.take(99)").len(), 5);
        assert_eq!(names("users.skip(99)"), Vec::new());
        assert_eq!(names("users.skip(1).take(2)"), [str("Bob"), str("Cyd")]);
        assert_eq!(names("users.take(5).skip(3)"), [str("Dan"), str("Eli")]);
        assert_eq!(names("users.take(2).take(5)").len(), 2);
        assert_eq!(names("users.skip(1).skip(2)").len(), 2);
        match run_seeded("users { o = orders.take(1) }") {
            Ok(Val::List(rows)) => {
                let counts: Vec<usize> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "o") {
                                Some((_, Val::List(orders))) => orders.len(),
                                got => panic!("expected orders list, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(counts, [1, 1, 0, 0, 0]);
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("users.take(-1)") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("negative bound"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        match run_seeded("users.skip(-2)") {
            Err(ExecError::Abort { .. }) => {}
            got => panic!("expected abort, got {got:?}"),
        }
    }

    #[test]
    fn method_first_nth() {
        let nth_name = |src: &str| match run_seeded(src) {
            Ok(Val::Some(user)) => match *user {
                Val::Record(user) => user
                    .into_iter()
                    .find_map(|(name, value)| (name.as_ref() == "name").then_some(value)),
                other => panic!("expected user record, got {other}"),
            },
            Ok(Val::None) => None,
            got => panic!("expected optional user for {src}, got {got:?}"),
        };
        assert_eq!(nth_name("users.nth(0)"), Some(str("Ada")));
        assert_eq!(nth_name("users.nth(4)"), Some(str("Eli")));
        assert_eq!(nth_name("users.nth(-1)"), Some(str("Eli")));
        assert_eq!(nth_name("users.nth(-5)"), Some(str("Ada")));
        assert_eq!(nth_name("users.nth(5)"), None);
        assert_eq!(nth_name("users.nth(-6)"), None);
        assert_eq!(nth_name("users.skip(5).nth(0)"), None);
        assert_eq!(nth_name("users.skip(5).nth(-1)"), None);
        assert_eq!(nth_name("users.take(2).first()"), Some(str("Ada")));
        assert_eq!(nth_name("users.skip(5).first()"), None);
        assert_eq!(nth_name("users.skip(2).first()"), Some(str("Cyd")));
        assert_eq!(nth_name("users.skip(2).nth(-1)"), Some(str("Eli")));
        assert_eq!(nth_name("users.take(3).nth(2)"), Some(str("Cyd")));
    }

    #[test]
    fn method_list_len() {
        assert_eq!(run_seeded("users.len()"), Ok(int(5)));
        assert_eq!(run_seeded("roles.len()"), Ok(int(2)));
        assert_eq!(run_seeded("users.take(2).len()"), Ok(int(2)));
        assert_eq!(run_seeded("users.skip(5).len()"), Ok(int(0)));
        match run_seeded("users { n = orders.len() }") {
            Ok(Val::List(rows)) => {
                let counts: Vec<i64> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "n") {
                                Some((_, Val::Scalar(Scalar::Int(n)))) => *n,
                                got => panic!("expected count, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(counts, [2, 1, 0, 0, 0]);
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(run_seeded("users.roles.len()"), Ok(int(5)));
        assert_eq!(run_seeded("users.roles.flatten().len()"), Ok(int(4)));
    }

    #[test]
    fn method_contains() {
        for src in [
            "[1, 2, 3].contains(2)",
            "[\"a\", \"bb\"].contains(\"bb\")",
            "[(1, 2), (3, 4)].contains((3, 4))",
            "users.tags.flatten().contains(\"a1\")",
        ] {
            assert_eq!(run_seeded(src), Ok(bool(true)), "{src}");
        }
        for src in [
            "[1, 2, 3].contains(9)",
            "[(1, 2)].contains((1, 3))",
            "users.tags.flatten().contains(\"zzz\")",
        ] {
            assert_eq!(run_seeded(src), Ok(bool(false)), "{src}");
        }
        match run_seeded("users { t = tags.contains(\"a1\") }") {
            Ok(Val::List(rows)) => {
                let flags: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "t") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected flag, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    flags,
                    [
                        bool(true),
                        bool(false),
                        bool(false),
                        bool(false),
                        bool(true)
                    ]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(
            run_seeded("users.scores.flatten().contains((1, \"one\"))"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users.scores.flatten().contains((9, \"nine\"))"),
            Ok(bool(false))
        );
        assert_eq!(
            run_seeded("users.first()?.tags?.contains(\"a1\")"),
            Ok(some(bool(true)))
        );
        assert_eq!(
            run_seeded("users { id, name }.contains({ name = \"Ada\", id = 1 })"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users { id, name }.contains({ id = 1, name = \"Ada\" })"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users { id, name }.contains({ id = 9, name = \"A\" })"),
            Ok(bool(false))
        );
    }

    #[test]
    fn method_sort_scalars() {
        let run_sort = |src: &str| -> Vec<Val> {
            match run_src(src) {
                Ok(Val::List(elements)) => elements,
                got => panic!("expected List, got: {got:?}"),
            }
        };
        assert_eq!(
            run_sort("[4,2,5,3,1].sort()"),
            vec![int(1), int(2), int(3), int(4), int(5)]
        );
        assert_eq!(
            run_sort(r#"["def", "abc", "xyz", "ab"].sort_desc()"#),
            vec![str("xyz"), str("def"), str("abc"), str("ab")]
        );
        assert_eq!(run_sort("[0.0f].sort_asc()"), vec![float(0.0)]);
        assert_eq!(
            run_sort("[1, 4, 2, 5, 3].sort_over(-$)"),
            [int(5), int(4), int(3), int(2), int(1)],
        );
        assert_eq!(
            run_sort("[2, 3, 1].sort_over(-$ + [4, 5].sort_desc_over($$ * -2 + $).len())"),
            [int(3), int(2), int(1)],
        );
    }

    #[test]
    fn method_sort_stable_composition() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        // rowid order within grounps
        assert_eq!(
            names("users.sort_over(active)"),
            [str("Cyd"), str("Eli"), str("Ada"), str("Bob"), str("Dan")]
        );
        assert_eq!(
            names("users.skip(2).sort_desc_over(name)"),
            [str("Eli"), str("Dan"), str("Cyd")]
        );
        match run_seeded("users { o = orders.sort_desc_over(amount) }") {
            Ok(Val::List(rows)) => {
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        assert_eq!(orders.len(), 2);
                        let amounts: Vec<i64> = orders
                            .iter()
                            .map(|order| match order {
                                Val::Record(fields) => {
                                    match fields.iter().find(|(name, _)| name.as_ref() == "amount")
                                    {
                                        Some((_, Val::Scalar(Scalar::Int(n)))) => *n,
                                        got => panic!("expected amount, got {got:?}"),
                                    }
                                }
                                other => panic!("expected order record, got {other}"),
                            })
                            .collect();
                        assert_eq!(amounts, [200, 100]);
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn method_sort_keyless_composition() {
        match run_seeded("users { t = tags.sort_desc() }") {
            Ok(Val::List(rows)) => {
                let Val::Record(second) = &rows[1] else {
                    panic!("expected record");
                };
                match second.iter().find(|(name, _)| name.as_ref() == "t") {
                    Some((_, Val::List(tags))) => {
                        assert_eq!(tags, &vec![str("b2"), str("b1")]);
                    }
                    got => panic!("expected tags list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn method_sort_over() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(
            names("users.sort_over(name)"),
            [str("Ada"), str("Bob"), str("Cyd"), str("Dan"), str("Eli")],
        );
        assert_eq!(
            names("users.sort_desc_over(name)"),
            [str("Eli"), str("Dan"), str("Cyd"), str("Bob"), str("Ada")],
        );
        assert_eq!(
            names("users.sort_desc_over(asc name)"),
            names("users.sort_over(name)"),
        );
        assert_eq!(
            names("users.sort_over(active, name)"),
            [str("Cyd"), str("Eli"), str("Ada"), str("Bob"), str("Dan")],
        );
        assert_eq!(
            names("users.sort_over(desc active, asc name)"),
            [str("Ada"), str("Bob"), str("Dan"), str("Cyd"), str("Eli")],
        );
    }

    #[test]
    fn method_filter_records() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(names("users[active]"), [str("Ada"), str("Bob"), str("Dan")]);
        assert_eq!(names("users.filter(active)"), names("users[active]"));
        assert_eq!(names("users[id > 2]"), [str("Cyd"), str("Dan"), str("Eli")]);
        assert_eq!(
            names("users[name.contains(\"A\".to_lower())]"),
            [str("Ada"), str("Dan")]
        );
        assert_eq!(names("users[active && id > 1]"), [str("Bob"), str("Dan")]);
        assert_eq!(names("users[nickname == some(\"A\")]"), [str("Ada")]);
        assert_eq!(
            names("users[profile?.bio?.len() == some(6)]"),
            [str("Ada"), str("Cyd")]
        );
        assert_eq!(
            names("users[manager?.name == users[name.starts_with(\"B\")].first()?.name]"),
            [str("Ada"), str("Cyd")]
        );
        assert_eq!(names("users[orders.len() > 1]"), [str("Ada")]);
        assert_eq!(
            names("users[orders.len() == 0]"),
            [str("Cyd"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            names("users[orders[amount > 150].len() == 1]"),
            [str("Ada"), str("Bob")]
        );
    }

    #[test]
    fn method_filter_scalars() {
        let run_filter = |src: &str| match run_src(src) {
            Ok(Val::List(elements)) => elements,
            got => panic!("expected List, got: {got:?}"),
        };
        assert_eq!(
            run_filter("[4, 2, 5, 3, 1].filter($ > 2)"),
            vec![int(4), int(5), int(3)]
        );
        assert_eq!(run_filter("[-1, -2, 2, 3][$ - 2 > 0]"), vec![int(3)]);
        assert_eq!(
            run_filter("[\"a\", \"bb\", \"c\"][!($.len() <= 1)]"),
            vec![str("bb")]
        );
        assert_eq!(
            run_filter("[[1, 2], [5]][$[$$ > 2].len() > 0]"),
            vec![Val::List(vec![int(5)])]
        );
        assert_eq!(run_filter("[1, 2, 3].filter($ > 10)"), Vec::new());
        assert_eq!(run_filter("[1][$ > 10][$ > 100]"), Vec::new());
        assert_eq!(
            match run_seeded("users.tags.flatten().filter($ == \"a1\")") {
                Ok(Val::List(elements)) => elements,
                got => panic!("expected List, got: {got:?}"),
            },
            vec![str("a1"), str("a1")]
        );
    }

    #[test]
    fn method_filter_optional() {
        let amounts = |src: &str| match run_seeded(src) {
            Ok(Val::Some(list)) => match *list {
                Val::List(orders) => orders
                    .into_iter()
                    .map(|order| match order {
                        Val::Record(fields) => fields
                            .into_iter()
                            .find_map(|(name, value)| (name.as_ref() == "amount").then_some(value))
                            .expect("expected `amount` field"),
                        other => panic!("expected order record, got {other}"),
                    })
                    .collect::<Vec<_>>(),
                got => panic!("expected orders list, got {got:?}"),
            },
            got => panic!("expected some(orders) for {src}, got {got:?}"),
        };
        assert_eq!(
            amounts("users.first()?.orders?.filter(amount > 150)"),
            [int(200)]
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.orders?.filter(amount > 0)"),
            Ok(Val::None)
        );
    }

    #[test]
    fn method_filter_composition() {
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(names("users[active][id > 1]"), [str("Bob"), str("Dan")]);
        assert_eq!(names("users[active][!active]"), []);
        assert_eq!(
            names("users.sort_desc_over(id).take(2)[active]"),
            [str("Dan")]
        );
        assert_eq!(names("users.skip(5)[active]"), Vec::new());
        assert_eq!(names("users[active][id > 100]"), Vec::new());
        match run_seeded("users[id == 1 / 0]") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("division by zero"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
    }

    #[test]
    fn method_filter_projection() {
        match run_seeded("users { o = orders[amount > 150] }") {
            Ok(Val::List(rows)) => {
                let counts: Vec<usize> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "o") {
                                Some((_, Val::List(orders))) => orders.len(),
                                got => panic!("expected orders list, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(counts, [1, 1, 0, 0, 0]);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        let Val::Record(order) = &orders[0] else {
                            panic!("expected order record");
                        };
                        assert!(order.iter().any(|(name, value)| {
                            name.as_ref() == "amount" && *value == int(200)
                        }));
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("roles { u = users[active] { name, o = orders[amount > 100] } }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 2);
                let user_names = |row: &Val| match row {
                    Val::Record(fields) => {
                        match fields.iter().find(|(name, _)| name.as_ref() == "u") {
                            Some((_, Val::List(users))) => users
                                .iter()
                                .map(|user| match user {
                                    Val::Record(user) => user
                                        .iter()
                                        .find_map(|(name, value)| {
                                            (name.as_ref() == "name").then_some(value.clone())
                                        })
                                        .expect("expected `name` field"),
                                    other => panic!("expected user record, got {other}"),
                                })
                                .collect::<Vec<_>>(),
                            got => panic!("expected users list, got {got:?}"),
                        }
                    }
                    other => panic!("expected record, got {other}"),
                };
                assert_eq!(user_names(&rows[0]), [str("Ada")]);
                assert_eq!(user_names(&rows[1]), [str("Ada"), str("Ada")]);
                let Val::Record(admin) = &rows[0] else {
                    panic!("expected record");
                };
                match admin.iter().find(|(name, _)| name.as_ref() == "u") {
                    Some((_, Val::List(users))) => {
                        let Val::Record(user) = &users[0] else {
                            panic!("expected user record");
                        };
                        match user.iter().find(|(name, _)| name.as_ref() == "o") {
                            Some((_, Val::List(orders))) => {
                                assert_eq!(orders.len(), 1);
                                let Val::Record(order) = &orders[0] else {
                                    panic!("expected order record");
                                };
                                assert!(order.iter().any(|(name, value)| {
                                    name.as_ref() == "amount" && *value == int(200)
                                }));
                            }
                            got => panic!("expected orders list, got {got:?}"),
                        }
                    }
                    got => panic!("expected users list, got {got:?}"),
                }
            }
            got => panic!("expected projected roles, got {got:?}"),
        }
    }

    #[test]
    fn method_optional_presence() {
        assert_eq!(run_src("some(42).is_some()"), Ok(bool(true)));
        assert_eq!(run_src("some(42).is_none()"), Ok(bool(false)));
        let names = |src: &str| match run_seeded(src) {
            Ok(Val::List(rows)) => rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(fields) => fields
                        .into_iter()
                        .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                        .expect("expected `name` field"),
                    other => panic!("expected record, got {other}"),
                })
                .collect::<Vec<_>>(),
            got => panic!("expected rows for {src}, got {got:?}"),
        };
        assert_eq!(
            names("users[manager.is_some()]"),
            [str("Ada"), str("Cyd"), str("Dan")]
        );
        assert_eq!(
            names("users[!!profile.is_none()]"),
            [str("Bob"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            run_seeded("users.first()?.nickname.is_some()"),
            Ok(bool(true))
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.nickname.is_some()"),
            Ok(bool(false))
        );
    }

    #[test]
    fn method_unwrap() {
        assert_eq!(run_src("some(42).unwrap()"), Ok(int(42)));
        assert_eq!(
            run_seeded("users.skip(1).first().unwrap().name"),
            Ok(str("Bob"))
        );
        match run_seeded("users.skip(5).first().unwrap()") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("unwrap"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        assert_eq!(run_src("some(some(1))?.unwrap()"), Ok(some(int(1))));
        assert_eq!(
            run_seeded("users.first()?.nickname?.is_some()"),
            Ok(some(bool(true)))
        );
    }

    #[test]
    fn method_unwrap_or() {
        assert_eq!(run_src("some(1).unwrap_or(99)"), Ok(int(1)));
        assert_eq!(
            run_src("some(some(1)).unwrap_or(some(99))"),
            Ok(some(int(1)))
        );
        assert_eq!(run_src("some(1).unwrap_or(1 / 0)"), Ok(int(1)));
        assert_eq!(run_src("some(some(1))?.unwrap_or(1 / 0)"), Ok(some(int(1))));
        assert_eq!(
            run_seeded("users.skip(1).first()?.nickname.unwrap_or(some(\"anon\"))"),
            Ok(some(str("anon")))
        );
        assert_eq!(
            run_seeded("users.first()?.nickname.unwrap_or(some(\"B\"))"),
            Ok(some(str("A")))
        );
        match run_seeded("users { n = nickname.unwrap_or(\"anon\") }") {
            Ok(Val::List(rows)) => {
                let nicknames: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "n") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected nickname, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    nicknames,
                    [str("A"), str("anon"), str("C"), str("anon"), str("E")]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("users { m = manager?.name.unwrap_or(\"boss\") }") {
            Ok(Val::List(rows)) => {
                let names: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "m") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected name, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    names,
                    [str("Bob"), str("boss"), str("Bob"), str("Ada"), str("boss")]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(
            run_seeded("users.skip(5).first()?.nickname.unwrap_or(some(\"anon\"))"),
            Ok(some(str("anon")))
        );
    }

    #[test]
    fn method_map() {
        let run_map = |src: &str| match run_seeded(src) {
            Ok(Val::List(elements)) => elements,
            got => panic!("expected List, got: {got:?}"),
        };
        assert_eq!(
            run_map("[1, 2, 4].map($ - 2)"),
            vec![int(-1), int(0), int(2)]
        );
        assert_eq!(run_map("[1, 2, 3].map($)"), vec![int(1), int(2), int(3)]);
        assert_eq!(
            run_map("[1, 2, 3].map($ + 1).map($ * 2)"),
            vec![int(4), int(6), int(8)]
        );
        assert_eq!(
            run_map("[(1, \"a\"), (2, \"b\")].map($.1)"),
            vec![str("a"), str("b")]
        );
        assert_eq!(
            run_map("[1, 2].map(($, $))"),
            vec![
                Val::Tuple(vec![int(1), int(1)]),
                Val::Tuple(vec![int(2), int(2)])
            ]
        );
        assert_eq!(run_map("[1].skip(1).map($ / 0)"), Vec::new());
        match run_src("[1].map($ / 0)") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("division by zero"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        assert_eq!(
            run_map("users.map($.name)"),
            [str("Ada"), str("Bob"), str("Cyd"), str("Dan"), str("Eli")]
        );
        assert_eq!(
            run_map("users.map($.balance * 2.0)"),
            [
                dec(dec!(60)),
                dec(dec!(0)),
                dec(dec!(-10.5)),
                dec(dec!(2000000)),
                dec(dec!(0.2))
            ]
        );
        assert_eq!(
            run_map("users.map($.active)"),
            [bool(true), bool(true), bool(false), bool(true), bool(false)]
        );
        assert_eq!(
            run_map("users.map($.nickname)"),
            [
                some(str("A")),
                Val::None,
                some(str("C")),
                Val::None,
                some(str("E"))
            ]
        );
        assert_eq!(
            run_map("users.map($.id.to_string())"),
            [str("1"), str("2"), str("3"), str("4"), str("5")]
        );
        assert_eq!(
            run_map("users.map($.pos.1)"),
            [float(-2.5), float(0.0), float(7.0), float(2.0), float(-0.5)]
        );
        assert_eq!(
            run_map("users.map({ n = $.name })"),
            [
                rec(vec![("n", str("Ada"))]),
                rec(vec![("n", str("Bob"))]),
                rec(vec![("n", str("Cyd"))]),
                rec(vec![("n", str("Dan"))]),
                rec(vec![("n", str("Eli"))])
            ]
        );
        assert_eq!(
            run_map("users.map(if $.active { 1 } else { 0 })"),
            [int(1), int(1), int(0), int(1), int(0)]
        );
        assert_eq!(
            run_seeded("users.skip(5).map($.id / 0)"),
            Ok(Val::List(vec![]))
        );
    }

    #[test]
    fn method_map_nesting() {
        let run_map = |src: &str| match run_seeded(src) {
            Ok(Val::List(elements)) => elements,
            got => panic!("expected List, got: {got:?}"),
        };
        assert_eq!(
            run_map("[[1, 2], [3]].map($.map($$ + 1))"),
            vec![Val::List(vec![int(2), int(3)]), Val::List(vec![int(4)])]
        );
        assert_eq!(
            run_map("[[1, 2], [3]].map($.map($$ + $.len()))"),
            vec![Val::List(vec![int(3), int(4)]), Val::List(vec![int(4)])]
        );
        assert_eq!(
            run_map("[[[1]], [[2, 3]]].map($.map($$.map($$$ + $$.len() + $.len())))"),
            vec![
                Val::List(vec![Val::List(vec![int(3)])]),
                Val::List(vec![Val::List(vec![int(5), int(6)])])
            ]
        );
        assert_eq!(
            run_map("[1, 2].map(users[id > $].name)"),
            vec![
                Val::List(vec![str("Bob"), str("Cyd"), str("Dan"), str("Eli")]),
                Val::List(vec![str("Cyd"), str("Dan"), str("Eli")]),
            ]
        );
        assert_eq!(
            run_map("users { o = orders.map($.amount + 5) }.o"),
            vec![
                Val::List(vec![int(105), int(205)]),
                Val::List(vec![int(405)]),
                Val::List(vec![]),
                Val::List(vec![]),
                Val::List(vec![]),
            ]
        );
    }

    #[test]
    fn method_map_optional() {
        assert_eq!(
            run_seeded("users.first()?.orders?.map($.amount/3)"),
            Ok(some(Val::List(vec![int(33), int(66)])))
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.orders?.map($.amount)"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded(
                "some([some([1,11]), some([2,22,222]), none])?.map($?.map($$ * 2 - 3 * $?.len().unwrap_or(0)))"
            ),
            Ok(some(Val::List(vec![
                some(Val::List(vec![int(-4), int(16)])),
                some(Val::List(vec![int(-5), int(35), int(435)])),
                Val::None
            ])))
        );
    }

    #[test]
    fn method_reduce_keyless() {
        for (src, expected) in [
            ("[1, 2, 3].sum()", some(int(6))),
            ("[1.5f, 2.5f].sum()", some(float(4.0))),
            (
                "[30.00, 0.1].sum()",
                some(Val::Scalar(Scalar::Dec(dec!(30.1)))),
            ),
            (
                "[#90s, #1.5s].sum()",
                some(Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(
                    91500,
                )))),
            ),
            ("[1, 2, 4].avg()", some(int(2))),
            ("[1.0, 2.0].avg()", some(dec(dec!(1.5)))),
            ("[1.5f, 2.5f].avg()", some(float(2.0))),
            (
                "[30.00, 0.10].avg()",
                some(Val::Scalar(Scalar::Dec(dec!(15.05)))),
            ),
            (
                "[#90s, #1.5s].avg()",
                some(Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(
                    45750,
                )))),
            ),
            (
                "[@1970-01-01, @1970-01-03_02:00].avg()",
                some(Val::Scalar(Scalar::Instant(
                    DateTime::from_timestamp(25 * 60 * 60, 0).unwrap(),
                ))),
            ),
            ("[3, 1, 2].max()", some(int(3))),
            ("[3, 1, 2].min()", some(int(1))),
            ("[\"b\", \"a\"].max()", some(str("b"))),
            (
                "[(1, \"b\"), (1, \"a\")].max()",
                some(Val::Tuple(vec![int(1), str("b")])),
            ),
            (
                "[(\"b\", 1), (\"a\", 2)].min()",
                some(Val::Tuple(vec![str("a"), int(2)])),
            ),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        assert_eq!(
            run_seeded("users.first()?.orders?.amount?.sum()"),
            Ok(some(some(int(300))))
        );
        for (src, expected) in [
            ("users.id.sum()", some(int(15))),
            ("users.id.max()", some(int(5))),
            ("users.name.min()", some(str("Ada"))),
            (
                "users.balance.sum()",
                some(Val::Scalar(Scalar::Dec(dec!(1000024.85)))),
            ),
            (
                "users.balance.avg()",
                some(Val::Scalar(Scalar::Dec(dec!(200004.97)))),
            ),
        ] {
            assert_eq!(run_seeded(src), Ok(expected), "{src}");
        }
        for src in [
            "[1].skip(1).sum()",
            "[4.0].skip(1).avg()",
            "[true].skip(1).max()",
            "[\"d\"].skip(1).min()",
        ] {
            assert_eq!(run_src(src), Ok(Val::None), "{src}");
        }
        assert_eq!(
            run_seeded("users.elapsed.avg()?.as_seconds()"),
            Ok(some(Val::Scalar(Scalar::Dec(dec!(18.024691358)))))
        );
        match run_src("[Int::MAX, 1].sum()") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(message.contains("integer overflow"), "got: {message}")
            }
            got => panic!("expected abort, got {got:?}"),
        }
        match run_src("[Float::MAX, Float::MAX].sum()") {
            Err(ExecError::Abort { message, .. }) => {
                assert!(
                    message.contains("non-finite float result"),
                    "got: {message}"
                )
            }
            got => panic!("expected abort, got {got:?}"),
        }
    }

    #[test]
    fn method_reduce_keyed() {
        for (src, expected) in [
            ("[-3, 2, 3].sum_over($ + 1)", some(int(5))),
            ("[10].sum_over($ - 2)", some(int(8))),
            ("[1, 2, 3].avg_over($ * $)", some(int(4))),
            ("[1.5f, 2.5f].avg_over($ + 0.5f)", some(float(2.5))),
            (
                "[30.000, 0.10].avg_over($ / 3.0)",
                some(Val::Scalar(Scalar::Dec(dec!(
                    5.0166666666666666666666666665
                )))),
            ),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        assert_eq!(run_src("[3, 1, 2].min_by($ + 10)"), Ok(some(int(1))));
        assert_eq!(
            run_src("[(1, \"a\"), (1, \"b\")].max_by($.0)"),
            Ok(some(Val::Tuple(vec![int(1), str("a")]))),
        );
        assert_eq!(
            run_src("[(2, \"b\"), (1, \"a\")].min_by($.1)"),
            Ok(some(Val::Tuple(vec![int(1), str("a")]))),
        );
        for src in [
            "[1].skip(1).sum_over($ / 0)",
            "[1].skip(1).avg_over($ / 0)",
            "[1].skip(1).max_by($ / 0)",
            "[1].skip(1).min_by($ / 0)",
        ] {
            assert_eq!(run_src(src), Ok(Val::None), "{src}");
        }

        assert_eq!(
            run_seeded("users { t = orders.sum_over(amount) }.t"),
            Ok(Val::List(vec![
                some(int(300)),
                some(int(400)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
        assert_eq!(
            run_seeded("users.orders.map($.avg_over(amount))"),
            Ok(Val::List(vec![
                some(int(150)),
                some(int(400)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
        assert_eq!(
            run_seeded("users { t = orders.sum_over(amount + id) }.t"),
            Ok(Val::List(vec![
                some(int(302)),
                some(int(402)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
        assert_eq!(
            run_seeded("users { m = orders.max_by(amount)?.amount }.m"),
            Ok(Val::List(vec![
                some(int(200)),
                some(int(400)),
                Val::None,
                Val::None,
                Val::None
            ])),
        );
    }

    #[test]
    fn method_reduce_optional() {
        assert_eq!(
            run_seeded("users.first()?.lucky?.map($.unwrap_or(0))?.sum()"),
            Ok(some(some(int(5))))
        );
        assert_eq!(
            run_seeded("users.first()?.orders?.max_by(amount).unwrap()?.amount"),
            Ok(some(int(200)))
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.lucky?.map($.unwrap_or(0))?.sum()"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("users.skip(5).first()?.orders?.max_by(amount)"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("users.first()?.orders?.map($.amount)?.sum()"),
            Ok(some(some(int(300))))
        );
        assert_eq!(
            run_seeded("users.skip(2).first()?.orders?.sum_over(amount)"),
            Ok(some(Val::None))
        );
    }

    #[test]
    fn eval_string_methods() {
        for (src, expected) in [
            (r#""hello".contains("ell")"#, bool(true)),
            (r#""hello".contains("z")"#, bool(false)),
            (r#""hello".contains("ELL")"#, bool(false)),
            (r#""hello".contains("")"#, bool(true)),
            (r#""".contains("")"#, bool(true)),
            (r#""hello".starts_with("he")"#, bool(true)),
            (r#""hello".starts_with("lo")"#, bool(false)),
            (r#""hello".ends_with("lo")"#, bool(true)),
            (r#""hello".ends_with("he")"#, bool(false)),
            (r#""HeLLo".to_upper()"#, str("HELLO")),
            (r#""HeLLo".to_lower()"#, str("hello")),
            (r#""ß".to_upper()"#, str("SS")),
            (r#""hello".len()"#, int(5)),
            (r#""".len()"#, int(0)),
            (r#""héllo".len()"#, int(5)),
            (r#""😀".len()"#, int(1)),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_instant_methods() {
        for (src, expected) in [
            ("@2026-10-20.year()", int(2026)),
            ("@2026-10-20.month()", int(10)),
            ("@2026-10-20.day()", int(20)),
            ("@2026-10-20.hour()", int(0)),
            ("@2026-10-20.minute()", int(0)),
            ("@2026-10-20.weekday()", int(2)),
            ("@2026-10-25.weekday()", int(0)),
            ("@2026-10-20_14:30:45.hour()", int(14)),
            ("@2026-10-20_14:30:45.minute()", int(30)),
            ("@2026-10-20_14:30:45.second()", dec(dec!(45))),
            (
                "@2026-10-20_14:30:45.123456789.second()",
                dec(dec!(45.123456789)),
            ),
            ("@1970-01-01.epoch()", dec(dec!(0))),
            ("@1970-01-02.epoch()", dec(dec!(86400))),
            ("@1970-01-01_00:00:01.5.epoch()", dec(dec!(1.5))),
            (
                "@2026-10-20_14:30:45.to_rfc3339()",
                str("2026-10-20T14:30:45Z"),
            ),
            (
                "@2026-10-20_14:30:45.5.to_rfc3339()",
                str("2026-10-20T14:30:45.500Z"),
            ),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_duration_methods() {
        for (src, expected) in [
            ("#90s.as_seconds()", dec(dec!(90))),
            ("#1.5s.as_seconds()", dec(dec!(1.5))),
            ("(-#90s).as_seconds()", dec(dec!(-90))),
            ("#3600s.as_minutes()", dec(dec!(60))),
            ("#3600s.as_hours()", dec(dec!(1))),
            ("#86400s.as_days()", dec(dec!(1))),
            ("#90s.as_hours()", dec(dec!(0.025))),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        match run_src("#1s.as_minutes()") {
            Ok(Val::Scalar(Scalar::Dec(d))) => {
                assert_eq!(d.scale(), 28);
                assert!(d.to_string().starts_with("0.016666"), "{d}");
            }
            got => panic!("expected Dec, got {got:?}"),
        }
    }

    #[test]
    fn eval_dec_methods() {
        for (src, expected) in [
            ("2.5.round_dp(0)", dec(dec!(2))),
            ("3.5.round_dp(0)", dec(dec!(4))),
            ("2.675.round_dp(2)", dec(dec!(2.68))),
            ("2.665.round_dp(2)", dec(dec!(2.66))),
            ("2.567.round_dp(2)", dec(dec!(2.57))),
            ("1.5.round_dp(100)", dec(dec!(1.5))),
            ("30.00.to_string()", str("30.0")),
            ("2.5.to_string()", str("2.5")),
            ("(123456789 as Dec).to_string()", str("123456789.0")),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        let err = run_src("2.5.round_dp(-1)").unwrap_err();
        assert!(err.to_string().contains("round_dp"), "{err}");
    }

    #[test]
    fn eval_scalar_to_string() {
        for (src, expected) in [
            ("42.to_string()", str("42")),
            ("3.14f.to_string()", str("3.14")),
            ("1.0f.to_string()", str("1.0")),
            ("true.to_string()", str("true")),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_float_rounding() {
        for (src, expected) in [
            ("2.5f.round()", float(3.0)),
            ("(-2.5f).round()", float(-3.0)),
            ("2.1f.round()", float(2.0)),
            ("2.9f.floor()", float(2.0)),
            ("(-2.1f).floor()", float(-3.0)),
            ("2.1f.ceil()", float(3.0)),
            ("(-2.9f).ceil()", float(-2.0)),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_optional_method_on_some() {
        assert_eq!(
            run_src(r#"some("hi")?.len()"#),
            Ok(Val::Some(Box::new(int(2))))
        );
        assert_eq!(
            run_src(r#"some("hi")?.contains("i")"#),
            Ok(Val::Some(Box::new(bool(true))))
        );
    }
}
