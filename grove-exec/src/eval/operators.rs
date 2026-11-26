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
mod tests {
    use crate::eval::harness::{bool, bytes, int, run_seeded, run_src, run_src_config, some, str};
    use crate::value::{Scalar, Val};
    use grove_types::Span;

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
            (r#"hex"00" < hex"0100""#, true),
            (r#"hex"00" <= hex"00""#, true),
            (r#"hex"0000" > hex"00""#, true),
            (r#"hex"ff" > hex"0100""#, true),
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
            (r#"hex"00" == hex"00""#, true),
            (r#"hex"00" != hex"01""#, true),
            (r#"hex"0000" == hex"00""#, false),
            ("some(1) == some(1)", true),
            ("some(1) != some(2)", true),
            ("none == some(2)", false),
            ("some(some(\"a\")) != some(none)", true),
        ] {
            assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
        }
    }

    #[test]
    fn eval_equality_non_owning_refs_ignored() {
        assert_eq!(run_seeded("users == users"), Ok(bool(true)));
        assert_eq!(run_seeded("users != users"), Ok(bool(false)));
        assert_eq!(run_seeded("users in (users,)"), Ok(bool(true)));
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
            Val::list(vec![int(1), Val::None]).eq(&Val::list(vec![int(1), Val::None])),
            Some(true)
        );
        assert_eq!(
            Val::list(vec![int(1)]).eq(&Val::list(vec![int(1), int(2)])),
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
            (r#"hex"00" in (hex"00", hex"01")"#, true),
            (r#"hex"02" in (hex"00", hex"01")"#, false),
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
    fn eval_bytes_sort_filter_extrema() {
        assert_eq!(
            run_src(r#"[hex"02", hex"01"].sort()"#),
            Ok(Val::list(vec![bytes(&[0x01]), bytes(&[0x02])]))
        );
        assert_eq!(
            run_src(r#"[hex"02", hex"01"].sort_desc()"#),
            Ok(Val::list(vec![bytes(&[0x02]), bytes(&[0x01])]))
        );
        assert_eq!(
            run_src(r#"[hex"02", hex"01"].min()"#),
            Ok(some(bytes(&[0x01])))
        );
        assert_eq!(
            run_src(r#"[hex"02", hex"01"].max()"#),
            Ok(some(bytes(&[0x02])))
        );
        assert_eq!(
            run_src(r#"[hex"00", hex"01"][$ >= hex"01"]"#),
            Ok(Val::list(vec![bytes(&[0x01])]))
        );
    }
}
