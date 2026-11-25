mod cast;

pub(super) use cast::eval_cast;

use grove_query::TypedExpr;
use grove_query::ast::{BinaryOp, UnaryOp};
use grove_schema::validated::IntArithmetic;
use grove_types::{Span, Spanned};

use super::{EvalCtx, eval};
use super::{arithmetic, data};
use crate::error::ExecError;
use crate::value::{Scalar, Val};

fn is_temporal(value: &Val) -> bool {
    matches!(value, Val::Scalar(Scalar::Instant(_) | Scalar::Duration(_)))
}

pub(super) fn eval_binary(
    op: &Spanned<BinaryOp>,
    lhs: &TypedExpr,
    rhs: &TypedExpr,
    ctx: &mut EvalCtx<'_>,
    span: Span,
) -> Result<Val, ExecError> {
    if matches!(op.value, BinaryOp::And | BinaryOp::Or) {
        return eval_logic(op, lhs, rhs, ctx);
    }
    let mut operands = [eval(lhs, ctx)?, eval(rhs, ctx)?];
    if matches!(op.value, BinaryOp::Eq | BinaryOp::Ne | BinaryOp::In) {
        data::resolve_owning_values(ctx, &mut operands, None)?;
    }
    let [l, r] = operands;
    if matches!(
        op.value,
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge
    ) {
        return eval_ordering(op, &l, &r, span);
    }
    if matches!(op.value, BinaryOp::Eq | BinaryOp::Ne) {
        return eval_equality(op, &l, &r);
    }
    if op.value.is_arithmetic() && (is_temporal(&l) || is_temporal(&r)) {
        return Ok(Val::Scalar(arithmetic::duration_arith(
            op.value, l, r, span,
        )?));
    }
    let scalar = match (op.value, l, r) {
        (BinaryOp::In, needle, Val::Tuple(members)) => {
            Scalar::Bool(members.iter().any(|member| needle.eq(member) == Some(true)))
        }
        (op, Val::Scalar(Scalar::Int(a)), Val::Scalar(Scalar::Int(b))) if op.is_arithmetic() => {
            arithmetic::int_arith(
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
            arithmetic::float_arith(
                a,
                op.try_into().unwrap(),
                b,
                ctx.schema.config.float_checks,
                span,
            )?
        }
        (op, Val::Scalar(Scalar::Dec(a)), Val::Scalar(Scalar::Dec(b))) if op.is_arithmetic() => {
            arithmetic::dec_arith(
                a,
                op.try_into().unwrap(),
                b,
                ctx.schema.config.dec_arithmetic,
                span,
            )?
        }
        (op, lhs, rhs) => {
            return Err(ExecError::Internal {
                message: format!("mismatched binary operands: {lhs} {op} {rhs}"),
            });
        }
    };
    Ok(Val::Scalar(scalar))
}

pub(super) fn eval_unary(
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
        (op, operand) => Err(ExecError::Internal {
            message: format!("mismatched unary operand: {op}{operand}"),
        }),
    }
}

fn eval_equality(op: &Spanned<BinaryOp>, l: &Val, r: &Val) -> Result<Val, ExecError> {
    let eq = l.eq(r).ok_or_else(|| ExecError::Internal {
        message: format!("mismatched equality operands: {l} {} {r}", op.value),
    })?;
    let result = matches!((op.value, eq), (BinaryOp::Eq, true) | (BinaryOp::Ne, false));
    Ok(Val::Scalar(Scalar::Bool(result)))
}

fn eval_ordering(op: &Spanned<BinaryOp>, l: &Val, r: &Val, span: Span) -> Result<Val, ExecError> {
    let ord = l.cmp(r, span)?;
    let result = match op.value {
        BinaryOp::Lt => ord == std::cmp::Ordering::Less,
        BinaryOp::Le => ord != std::cmp::Ordering::Greater,
        BinaryOp::Gt => ord == std::cmp::Ordering::Greater,
        BinaryOp::Ge => ord != std::cmp::Ordering::Less,
        _ => {
            return Err(ExecError::Internal {
                message: "non-ordering operator in ordered dispatch".to_string(),
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
        return Err(ExecError::Internal {
            message: format!("non-Bool logic operand: {l}"),
        });
    };
    let decided = matches!((op.value, l), (BinaryOp::And, false) | (BinaryOp::Or, true));
    if decided {
        return Ok(Val::Scalar(Scalar::Bool(l)));
    }
    let r = eval(rhs, ctx)?;
    let Val::Scalar(Scalar::Bool(r)) = r else {
        return Err(ExecError::Internal {
            message: format!("non-Bool logic operand: {r}"),
        });
    };
    Ok(Val::Scalar(Scalar::Bool(r)))
}

#[cfg(test)]
mod tests;
