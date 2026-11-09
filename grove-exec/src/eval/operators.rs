use grove_query::ast::{BinaryOp, TypeName, UnaryOp};
use grove_query::typecheck::types::TypedExpr;
use grove_schema::validated::IntArithmetic;
use grove_types::{Span, Spanned};
use num_traits::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

use super::arithmetic::{dec_arith, duration_from_nanos, float_arith, int_arith, scale_duration};
use super::data::resolve_owning_values;
use super::{EvalCtx, eval};
use crate::error::ExecError;
use crate::value::{Scalar, Val, total_nanos};

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
        resolve_owning_values(ctx, &mut operands, None)?;
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

fn eval_ordering(op: &Spanned<BinaryOp>, l: &Val, r: &Val, span: Span) -> Result<Val, ExecError> {
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

pub(super) fn eval_cast(value: Val, target: TypeName, span: Span) -> Result<Val, ExecError> {
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

#[cfg(test)]
mod tests {
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};
    use grove_types::Span;
    use num_traits::ToPrimitive;
    use rust_decimal::{Decimal, dec};

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
}
