use std::collections::HashMap;
use std::fmt::Display;

use chrono::{DateTime, Datelike, NaiveTime, SecondsFormat, TimeDelta, Timelike, Utc};
use grove_query::ast::{BinaryOp, ConstantName, Literal, TypeName, UnaryOp};
use grove_query::typecheck::types::{
    IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt, TypedMethodArg,
};
use grove_schema::validated::{DecArithmetic, IntArithmetic, ValidatedSchema};
use grove_types::{Span, Spanned};
use num_traits::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, RoundingStrategy};

use crate::error::ExecError;
use crate::exec::exec_root;
use crate::storage::Storage;
use crate::value::{
    Interner, NANOS_PER_DAY, NANOS_PER_HOUR, NANOS_PER_MIN, NANOS_PER_SEC, Scalar, Val, total_nanos,
};

/// Lexical value scope chain
#[derive(Debug, Default)]
pub struct Env {
    scopes: Vec<HashMap<String, Val>>,
}

impl Env {
    pub fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn define(&mut self, name: String, value: Val) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name, value);
    }

    /// `depth` counts scopes skipped to reach the identifier
    fn lookup(&self, name: &str, depth: usize) -> Option<&Val> {
        self.scopes.iter().rev().nth(depth)?.get(name)
    }
}

pub struct EvalCtx<'a> {
    env: Env,
    now: DateTime<Utc>,
    pub(crate) interner: Interner,
    pub(crate) storage: &'a dyn Storage,
    pub(crate) schema: &'a ValidatedSchema,
}

impl<'a> EvalCtx<'a> {
    pub fn new(
        env: Env,
        now: DateTime<Utc>,
        storage: &'a dyn Storage,
        schema: &'a ValidatedSchema,
    ) -> Self {
        EvalCtx {
            env,
            now,
            interner: Interner::default(),
            storage,
            schema,
        }
    }
}

/// Evaluate an expression
pub fn eval(expr: &TypedExpr, ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(decode_literal(lit, ctx.now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => match binding {
            IdentBinding::Let => {
                ctx.env
                    .lookup(&name.value, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("unbound variable `{}` (internal error)", name.value),
                    })
            }
            IdentBinding::Root { root_idx } => exec_root(ctx, *root_idx),
            IdentBinding::Field(_) | IdentBinding::ProjectedField { .. } => {
                todo!("data references with row contexts")
            }
            IdentBinding::Prev { .. } => todo!("prev with mutations"),
            IdentBinding::ListElement => todo!("element references inside collection operators"),
        },
        TypedExprKind::Some { value } => eval(value, ctx).map(|v| Val::Some(Box::new(v))),
        TypedExprKind::TypeConstant { ty, name } => Ok(eval_type_constant(ty.value, name.value)),
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                match eval(cond, ctx)? {
                    Val::Scalar(Scalar::Bool(true)) => return eval(value, ctx),
                    Val::Scalar(Scalar::Bool(false)) => {}
                    _ => {
                        return Err(ExecError::Integrity {
                            message: "if condition must be a Bool (internal error)".to_string(),
                        });
                    }
                }
            }
            eval(default, ctx)
        }
        TypedExprKind::Binary { op, lhs, rhs } => eval_binary(op, lhs, rhs, ctx, expr.span),
        TypedExprKind::Unary { op, expr: inner } => eval_unary(op, inner, ctx, expr.span),
        TypedExprKind::Field { .. } | TypedExprKind::TupleAccess { .. } => todo!("navigation"),
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => eval_method(base, &name.value, args, *optional, ctx, expr.span),
        TypedExprKind::Cast { expr: inner, ty } => {
            let value = eval(inner, ctx)?;
            eval_cast(value, ty.value, expr.span)
        }
        TypedExprKind::Tuple { elements } => {
            let mut items = Vec::with_capacity(elements.len());
            for element in elements {
                items.push(eval(element, ctx)?);
            }
            Ok(Val::Tuple(items))
        }
        TypedExprKind::Array { elements } => {
            let mut items = Vec::with_capacity(elements.len());
            for element in elements {
                items.push(eval(element, ctx)?);
            }
            Ok(Val::List(items))
        }
        TypedExprKind::Struct { fields } => {
            let mut record = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                let value = eval(value, ctx)?;
                record.push((ctx.interner.intern(&name.value), value));
            }
            Ok(Val::Record(record))
        }
        TypedExprKind::Projection { .. } => todo!("projections"),
    }
}

const MIDNIGHT: NaiveTime = NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time");

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

impl Display for ArithOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
                // i64::MIN % -1 is the only failure case, mathematically this is 0
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

fn eval_binary(
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
        (BinaryOp::In, _, _) => todo!("membership"),
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

fn eval_unary(
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

fn eval_cast(value: Val, target: TypeName, span: Span) -> Result<Val, ExecError> {
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

fn eval_method(
    base: &TypedExpr,
    name: &str,
    args: &[TypedMethodArg],
    optional: bool,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    // `?.` on none returns none without evaluating args
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    let mut arg_vals = Vec::with_capacity(args.len());
    for arg in args {
        arg_vals.push(eval(&arg.expr, ctx)?);
    }
    let scalar = eval_scalar_method(&base, name, &arg_vals, span)?;
    let value = Val::Scalar(scalar);
    Ok(if optional {
        Val::Some(Box::new(value))
    } else {
        value
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
        if matches!(base, Val::List(_)) {
            todo!("list methods");
        }
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

fn decode_literal(lit: &Spanned<Literal>, now: DateTime<Utc>) -> Val {
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

fn eval_type_constant(ty: TypeName, name: ConstantName) -> Val {
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

/// Evaluate a `let` and bind its value
pub fn eval_let(stmt: &TypedLetStmt, ctx: &mut EvalCtx<'_>) -> Result<(), ExecError> {
    let value = eval(&stmt.value, ctx)?;
    ctx.env.define(stmt.name.value.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use grove_query::typecheck::types::{QueryType, TypedStatement};
    use grove_schema::validated::{FieldId, RelationId, StructId, ValidatedSchema};
    use grove_types::Span;
    use rust_decimal::dec;
    use std::sync::Arc;

    use crate::storage::{Key, Row};

    const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    struct NoStorage;

    impl Storage for NoStorage {
        fn fetch(&self, _struct_id: StructId, _fields: &[FieldId]) -> Result<Vec<Row>, ExecError> {
            Err(ExecError::Integrity {
                message: "NoStorage backend cannot provide storage (internal error)".to_string(),
            })
        }

        fn fetch_related(
            &self,
            _rel: RelationId,
            _fetch_from_first: bool,
            _parents: &[Key],
            _fields: &[FieldId],
        ) -> Result<Vec<Vec<Row>>, ExecError> {
            Err(ExecError::Integrity {
                message: "NoStorage backend cannot provide storage (internal error)".to_string(),
            })
        }
    }

    fn run_src(src: &str) -> Result<Val, ExecError> {
        run_with(src, ValidatedSchema::default())
    }

    fn run_src_config(src: &str, config_block: &str) -> Result<Val, ExecError> {
        let schema_src =
            format!("{config_block}\nroot users: User; struct User {{ name: String }}");
        let (schema, schema_diags) = grove_schema::parse_schema(&schema_src);
        assert!(schema_diags.is_empty(), "{schema_diags:?}");
        let (schema, schema_diags) = grove_schema::validate(schema.unwrap());
        assert!(schema_diags.is_empty(), "{schema_diags:?}");
        run_with(src, schema.unwrap())
    }

    fn run_with(src: &str, schema: ValidatedSchema) -> Result<Val, ExecError> {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "{diags:?}");
        let file = typed.unwrap();
        let mut ctx = EvalCtx::new(Env::new(), FIXED_NOW, &NoStorage, &schema);
        for stmt in &file.statements {
            match stmt {
                TypedStatement::Let(let_stmt) => eval_let(let_stmt, &mut ctx)?,
                TypedStatement::Expr(expr) => {
                    eval(expr, &mut ctx)?;
                }
                TypedStatement::Mutation(_) => todo!("mutations"),
            }
        }
        eval(&file.result, &mut ctx)
    }

    #[test]
    fn eval_literals() {
        assert_eq!(run_src("42"), Ok(Val::Scalar(Scalar::Int(42))));
        assert_eq!(run_src("1.5f"), Ok(Val::Scalar(Scalar::Float(1.5))));
        assert_eq!(run_src("30.00"), Ok(Val::Scalar(Scalar::Dec(dec!(30.00)))));
        assert_eq!(
            run_src("\"hi\""),
            Ok(Val::Scalar(Scalar::String("hi".to_string())))
        );
        assert_eq!(run_src("true"), Ok(Val::Scalar(Scalar::Bool(true))));
        assert_eq!(
            run_src("some(1)"),
            Ok(Val::Some(Box::new(Val::Scalar(Scalar::Int(1))))),
        );
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
        assert_eq!(run_src("Int::MIN"), Ok(Val::Scalar(Scalar::Int(i64::MIN))));
        assert_eq!(
            run_src("Float::MAX"),
            Ok(Val::Scalar(Scalar::Float(f64::MAX)))
        );
        assert_eq!(
            run_src("Dec::MIN"),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MIN)))
        );
        assert_eq!(
            run_src("Dec::MAX"),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MAX)))
        );
    }

    #[test]
    fn eval_let_binding() {
        assert_eq!(run_src("let x = 1; x"), Ok(Val::Scalar(Scalar::Int(1))));
        assert_eq!(
            run_src("let x = 1; let x = 2; x"),
            Ok(Val::Scalar(Scalar::Int(2)))
        );
        assert_eq!(
            run_src("let x = true; let x = \"hello\"; x"),
            Ok(Val::Scalar(Scalar::String("hello".to_string())))
        );
        assert_eq!(
            run_src("let a = 1; let b = a; b"),
            Ok(Val::Scalar(Scalar::Int(1)))
        );
    }

    #[test]
    fn eval_if_scalar() {
        assert_eq!(
            run_src("if true { 1 } else { 2 }"),
            Ok(Val::Scalar(Scalar::Int(1)))
        );
        assert_eq!(
            run_src("if false { 1 } else { 2 }"),
            Ok(Val::Scalar(Scalar::Int(2)))
        );
        assert_eq!(
            run_src("if false { 1 } else if false { 2 } else { 3 }"),
            Ok(Val::Scalar(Scalar::Int(3)))
        );
    }

    #[test]
    fn eval_env_depth_collections() {
        let mut env = Env::new();
        env.define(
            "xs".to_string(),
            Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]),
        );
        let schema = ValidatedSchema::default();
        let mut ctx = EvalCtx::new(env, FIXED_NOW, &NoStorage, &schema);
        let span = Span::new(0, 0);
        let expr = TypedExpr {
            kind: TypedExprKind::Ident {
                name: grove_types::Spanned {
                    span,
                    value: "xs".to_string(),
                },
                binding: IdentBinding::Let,
                depth: 0,
            },
            ty: QueryType::List(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                grove_schema::validated::ScalarType::Int,
            ))))),
            span,
        };
        assert_eq!(
            eval(&expr, &mut ctx),
            Ok(Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]))
        );
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

    fn int(n: i64) -> Val {
        Val::Scalar(Scalar::Int(n))
    }

    fn float(x: f64) -> Val {
        Val::Scalar(Scalar::Float(x))
    }

    fn str(s: &str) -> Val {
        Val::Scalar(Scalar::String(s.to_string()))
    }

    fn bool(b: bool) -> Val {
        Val::Scalar(Scalar::Bool(b))
    }

    fn rec(fields: Vec<(&str, Val)>) -> Val {
        Val::Record(
            fields
                .into_iter()
                .map(|(name, value)| (Arc::from(name), value))
                .collect(),
        )
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
        assert_eq!(
            run_src("1.5 + 2.5"),
            Ok(Val::Scalar(Scalar::Dec(dec!(4.0))))
        );
        assert_eq!(
            run_src("5.00 / 2.0"),
            Ok(Val::Scalar(Scalar::Dec(dec!(2.5))))
        );
        assert_eq!(
            run_src("1.0 / 3.0"),
            Ok(Val::Scalar(Scalar::Dec(dec!(
                0.3333333333333333333333333333
            ))))
        );
        assert_eq!(
            run_src("1.10 + 2.20"),
            Ok(Val::Scalar(Scalar::Dec(dec!(3.3))))
        );
        assert_eq!(
            run_src("-7.0 % 3.0"),
            Ok(Val::Scalar(Scalar::Dec(dec!(-1))))
        );
        assert_eq!(
            run_src("-7.0 mod 3.0"),
            Ok(Val::Scalar(Scalar::Dec(dec!(2))))
        );
        assert_eq!(
            run_src("7.0 mod -3.0"),
            Ok(Val::Scalar(Scalar::Dec(dec!(1))))
        );
        assert_eq!(
            run_src("-7.0 mod -3.0"),
            Ok(Val::Scalar(Scalar::Dec(dec!(2))))
        );
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
        assert_eq!(
            run_src_config("Dec::MAX + 1.0", sat),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MAX)))
        );
        assert_eq!(
            run_src_config("Dec::MIN - 1.0", sat),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MIN)))
        );
        assert_eq!(
            run_src_config("Dec::MAX * 10.0", sat),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MAX)))
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
        assert_eq!(
            run_src("#75s / #30s"),
            Ok(Val::Scalar(Scalar::Dec(dec!(2.5))))
        );
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
            ("5 as Dec", Val::Scalar(Scalar::Dec(dec!(5)))),
            ("5 as Int", int(5)),
            ("1.9f as Int", int(1)),
            ("-1.9f as Int", int(-1)),
            ("1e300f as Int", int(i64::MAX)),
            ("(-1e300f) as Int", int(i64::MIN)),
            ("1.5f as Dec", Val::Scalar(Scalar::Dec(dec!(1.5)))),
            ("1e300f as Dec", Val::Scalar(Scalar::Dec(Decimal::MAX))),
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
            (
                "@2026-10-20_14:30:45.second()",
                Val::Scalar(Scalar::Dec(dec!(45))),
            ),
            (
                "@2026-10-20_14:30:45.123456789.second()",
                Val::Scalar(Scalar::Dec(dec!(45.123456789))),
            ),
            ("@1970-01-01.epoch()", Val::Scalar(Scalar::Dec(dec!(0)))),
            ("@1970-01-02.epoch()", Val::Scalar(Scalar::Dec(dec!(86400)))),
            (
                "@1970-01-01_00:00:01.5.epoch()",
                Val::Scalar(Scalar::Dec(dec!(1.5))),
            ),
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
            ("#90s.as_seconds()", Val::Scalar(Scalar::Dec(dec!(90)))),
            ("#1.5s.as_seconds()", Val::Scalar(Scalar::Dec(dec!(1.5)))),
            ("(-#90s).as_seconds()", Val::Scalar(Scalar::Dec(dec!(-90)))),
            ("#3600s.as_minutes()", Val::Scalar(Scalar::Dec(dec!(60)))),
            ("#3600s.as_hours()", Val::Scalar(Scalar::Dec(dec!(1)))),
            ("#86400s.as_days()", Val::Scalar(Scalar::Dec(dec!(1)))),
            ("#90s.as_hours()", Val::Scalar(Scalar::Dec(dec!(0.025)))),
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
            ("2.5.round_dp(0)", Val::Scalar(Scalar::Dec(dec!(2)))),
            ("3.5.round_dp(0)", Val::Scalar(Scalar::Dec(dec!(4)))),
            ("2.675.round_dp(2)", Val::Scalar(Scalar::Dec(dec!(2.68)))),
            ("2.665.round_dp(2)", Val::Scalar(Scalar::Dec(dec!(2.66)))),
            ("2.567.round_dp(2)", Val::Scalar(Scalar::Dec(dec!(2.57)))),
            ("1.5.round_dp(100)", Val::Scalar(Scalar::Dec(dec!(1.5)))),
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

    #[test]
    fn eval_tuple_literal() {
        assert_eq!(
            run_src("(1, \"hi\")"),
            Ok(Val::Tuple(vec![int(1), str("hi")]))
        );
        assert_eq!(
            run_src("(1, (2, 3))"),
            Ok(Val::Tuple(vec![int(1), Val::Tuple(vec![int(2), int(3)])]))
        );
        assert_eq!(
            run_src("let x = 1; (x, x)"),
            Ok(Val::Tuple(vec![int(1), int(1)]))
        );
        assert_eq!(
            run_src("if true { (1, 2) } else { (3, 4) }"),
            Ok(Val::Tuple(vec![int(1), int(2)]))
        );
    }

    #[test]
    fn eval_array_literal() {
        assert_eq!(
            run_src("[1, 2, 3]"),
            Ok(Val::List(vec![int(1), int(2), int(3)]))
        );
        assert_eq!(
            run_src("[[1], [2]]"),
            Ok(Val::List(vec![
                Val::List(vec![int(1)]),
                Val::List(vec![int(2)])
            ]))
        );
        assert_eq!(run_src("[1 + 1, 3]"), Ok(Val::List(vec![int(2), int(3)])));
        assert_eq!(
            run_src("let x = 1; [x, x]"),
            Ok(Val::List(vec![int(1), int(1)]))
        );
    }

    #[test]
    fn eval_struct_literal() {
        assert_eq!(
            run_src("{name = \"Alice\", age = 30}"),
            Ok(rec(vec![("name", str("Alice")), ("age", int(30))]))
        );
        assert_eq!(run_src("let x = 1; {a = x}"), Ok(rec(vec![("a", int(1))])));
        assert_eq!(
            run_src("{a = {b = 1}}"),
            Ok(rec(vec![("a", rec(vec![("b", int(1))]))]))
        );
        assert_eq!(
            run_src("{a = [1, 2]}"),
            Ok(rec(vec![("a", Val::List(vec![int(1), int(2)]))]))
        );
    }
}
