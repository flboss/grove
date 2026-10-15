use std::collections::HashMap;
use std::fmt::Display;

use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use grove_query::ast::{BinaryOp, ConstantName, Literal, TypeName, UnaryOp};
use grove_query::typecheck::types::{IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt};
use grove_schema::validated::{Config, DecArithmetic, IntArithmetic};
use grove_types::{Span, Spanned};
use num_traits::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, RoundingStrategy};

use crate::error::ExecError;
use crate::value::{Scalar, Val};

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

/// Evaluate a scalar expression; fixed clock for `Now`/`Today`
pub fn eval(
    expr: &TypedExpr,
    env: &mut Env,
    now: DateTime<Utc>,
    config: &Config,
) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(decode_literal(lit, now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => match binding {
            IdentBinding::Let => {
                env.lookup(&name.value, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("unbound variable `{}` (internal error)", name.value),
                    })
            }
            IdentBinding::Root { .. }
            | IdentBinding::Field(_)
            | IdentBinding::ProjectedField { .. } => todo!("data references with row contexts"),
            IdentBinding::Prev { .. } => todo!("prev with mutations"),
            IdentBinding::ListElement => todo!("element references inside collection operators"),
        },
        TypedExprKind::Some { value } => {
            eval(value, env, now, config).map(|v| Val::Some(Box::new(v)))
        }
        TypedExprKind::TypeConstant { ty, name } => Ok(eval_type_constant(ty.value, name.value)),
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                match eval(cond, env, now, config)? {
                    Val::Scalar(Scalar::Bool(true)) => return eval(value, env, now, config),
                    Val::Scalar(Scalar::Bool(false)) => {}
                    _ => {
                        return Err(ExecError::Integrity {
                            message: "if condition must be a Bool (internal error)".to_string(),
                        });
                    }
                }
            }
            eval(default, env, now, config)
        }
        TypedExprKind::Binary { op, lhs, rhs } => {
            eval_binary(op, lhs, rhs, env, now, config, expr.span)
        }
        TypedExprKind::Unary { op, expr: inner } => {
            eval_unary(op, inner, env, now, config, expr.span)
        }
        TypedExprKind::Field { .. } | TypedExprKind::TupleAccess { .. } => todo!("navigation"),
        TypedExprKind::Method { .. } => todo!("methods"),
        TypedExprKind::Cast { .. } => todo!("casts"),
        TypedExprKind::Tuple { .. }
        | TypedExprKind::Array { .. }
        | TypedExprKind::Struct { .. }
        | TypedExprKind::Projection { .. } => todo!("composite literals"),
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

/// Total nanoseconds of a Duration
fn total_nanos(d: TimeDelta) -> i128 {
    d.num_seconds() as i128 * 1_000_000_000 + d.subsec_nanos() as i128
}

/// Build a Duration from whole nanoseconds; out-of-range aborts
fn duration_from_nanos(nanos: i128, span: Span) -> Result<TimeDelta, ExecError> {
    let secs = nanos.div_euclid(1_000_000_000);
    let sub = nanos.rem_euclid(1_000_000_000) as u32;
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
    let base = Decimal::from_i128_with_scale(total_nanos(duration), 0);
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
    env: &mut Env,
    now: DateTime<Utc>,
    config: &Config,
    span: Span,
) -> Result<Val, ExecError> {
    let l = eval(lhs, env, now, config)?;
    let r = eval(rhs, env, now, config)?;
    let scalar = match (op.value, l, r) {
        (BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge, _, _) => {
            todo!("comparisons")
        }
        (BinaryOp::Eq | BinaryOp::Ne, _, _) => todo!("equality"),
        (BinaryOp::And | BinaryOp::Or, _, _) => todo!("logic"),
        (BinaryOp::In, _, _) => todo!("membership"),
        (op, Val::Scalar(Scalar::Int(a)), Val::Scalar(Scalar::Int(b))) if op.is_arithmetic() => {
            int_arith(a, op.try_into().unwrap(), b, config.int_arithmetic, span)?
        }
        (op, Val::Scalar(Scalar::Float(a)), Val::Scalar(Scalar::Float(b)))
            if op.is_arithmetic() =>
        {
            float_arith(a, op.try_into().unwrap(), b, config.float_checks, span)?
        }
        (op, Val::Scalar(Scalar::Dec(a)), Val::Scalar(Scalar::Dec(b))) if op.is_arithmetic() => {
            dec_arith(a, op.try_into().unwrap(), b, config.dec_arithmetic, span)?
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
            let (an, bn) = (total_nanos(a), total_nanos(b));
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
            let (an, bn) = (total_nanos(a), total_nanos(b));
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
            let ratio = Decimal::from_i128_with_scale(total_nanos(a), 0)
                .checked_div(Decimal::from_i128_with_scale(total_nanos(b), 0))
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
                        total_nanos(d)
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
    env: &mut Env,
    now: DateTime<Utc>,
    config: &Config,
    span: Span,
) -> Result<Val, ExecError> {
    let value = eval(inner, env, now, config)?;
    match (op.value, value) {
        (UnaryOp::Neg, Val::Scalar(Scalar::Int(n))) => Ok(match n.checked_neg() {
            Some(m) => Val::Scalar(Scalar::Int(m)),
            None => match config.int_arithmetic {
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
            if config.float_checks && !(-x).is_finite() {
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
        (UnaryOp::Not, _) => todo!("logic"),
        (op, operand) => Err(ExecError::Integrity {
            message: format!("mismatched unary operand: {op}{operand} (internal error)"),
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
pub fn eval_let(
    stmt: &TypedLetStmt,
    env: &mut Env,
    now: DateTime<Utc>,
    config: &Config,
) -> Result<(), ExecError> {
    let value = eval(&stmt.value, env, now, config)?;
    env.define(stmt.name.value.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use grove_query::typecheck::types::{QueryType, TypedStatement};
    use grove_schema::validated::ValidatedSchema;
    use grove_types::Span;
    use rust_decimal::dec;

    const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

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
        let mut env = Env::new();
        for stmt in &file.statements {
            match stmt {
                TypedStatement::Let(let_stmt) => {
                    eval_let(let_stmt, &mut env, FIXED_NOW, &schema.config)?
                }
                TypedStatement::Expr(expr) => {
                    eval(expr, &mut env, FIXED_NOW, &schema.config)?;
                }
                TypedStatement::Mutation(_) => todo!("mutations"),
            }
        }
        eval(&file.result, &mut env, FIXED_NOW, &schema.config)
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
            eval(&expr, &mut env, FIXED_NOW, &Config::default()),
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

    fn float(x: f64) -> Val {
        Val::Scalar(Scalar::Float(x))
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
}
