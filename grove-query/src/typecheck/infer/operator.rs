use super::infer;
use crate::ast::{BinaryOp, Expr, UnaryOp};
use crate::typecheck::compat::types_compatible;
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{QueryType, TypedExpr, TypedExprKind};
use grove_schema::validated::{ScalarType, ValidatedSchema};
use grove_types::{Span, Spanned};

pub(crate) fn infer_binary(
    op: &Spanned<BinaryOp>,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_lhs = infer(lhs, env)?;
    let mut typed_rhs = infer(rhs, env)?;
    let span = typed_lhs.span.merge(typed_rhs.span);

    if typed_lhs.ty.contains_row_ref() || typed_rhs.ty.contains_row_ref() {
        return Err(TypeError::RowRefOperationNotAllowed {
            operation: op.value.to_string(),
            span,
        });
    }

    let result_ty = match op.value {
        BinaryOp::In => infer_binary_in(&mut typed_lhs.ty, &mut typed_rhs.ty, span, env.schema())?,
        BinaryOp::And | BinaryOp::Or => {
            if !typed_lhs.ty.is_bool() || !typed_rhs.ty.is_bool() {
                return Err(TypeError::BinaryOpTypeMismatch {
                    left: typed_lhs.ty.to_string(),
                    op: op.value.to_string(),
                    right: typed_rhs.ty.to_string(),
                    span,
                });
            }
            QueryType::Scalar(ScalarType::Bool)
        }
        BinaryOp::Eq | BinaryOp::Ne => {
            infer_equality_op(&mut typed_lhs.ty, op, &mut typed_rhs.ty, span, env.schema())?
        }
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            infer_comparison_op(&mut typed_lhs.ty, op, &mut typed_rhs.ty, span, env.schema())?
        }
        _ => infer_arithmetic_op(&mut typed_lhs.ty, op, &mut typed_rhs.ty, span)?,
    };

    Ok(env.typed(
        TypedExprKind::Binary {
            op: op.clone(),
            lhs: Box::new(typed_lhs),
            rhs: Box::new(typed_rhs),
        },
        result_ty,
        span,
    ))
}

pub(crate) fn infer_binary_in(
    lhs: &mut QueryType,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    let rhs_inner = match rhs {
        QueryType::Tuple(elems) if !elems.is_empty() => elems,
        _ => return Err(TypeError::InRequiresTuple { span }),
    };
    for elem in rhs_inner {
        if types_compatible(lhs, elem, schema) {
            return Ok(QueryType::Scalar(ScalarType::Bool));
        }
    }
    Err(TypeError::BinaryOpTypeMismatch {
        left: lhs.to_string(),
        op: "in".into(),
        right: rhs.to_string(),
        span,
    })
}

pub(crate) fn infer_equality_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    if !types_compatible(lhs, rhs, schema) {
        return Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        });
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

pub(crate) fn infer_comparison_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    if !types_compatible(lhs, rhs, schema) || !lhs.has_defined_order() {
        return Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        });
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

pub(crate) fn infer_arithmetic_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
) -> Result<QueryType, TypeError> {
    use ScalarType::*;

    match (lhs, rhs) {
        (QueryType::Scalar(Int), QueryType::Scalar(Int)) => Ok(QueryType::Scalar(Int)),
        (QueryType::Scalar(Float), QueryType::Scalar(Float)) => Ok(QueryType::Scalar(Float)),
        (QueryType::Scalar(Dec), QueryType::Scalar(Dec)) => Ok(QueryType::Scalar(Dec)),
        (lhs @ QueryType::Scalar(Instant), rhs @ QueryType::Scalar(Duration)) => {
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                Ok(QueryType::Scalar(Instant))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Instant), rhs @ QueryType::Scalar(Instant)) => {
            if matches!(op, BinaryOp::Sub) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Duration), rhs @ QueryType::Scalar(Duration)) => {
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                Ok(QueryType::Scalar(Duration))
            } else if matches!(op, BinaryOp::Div) {
                Ok(QueryType::Scalar(Dec))
            } else if matches!(op, BinaryOp::Rem | BinaryOp::Mod) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs, rhs @ QueryType::Scalar(Duration)) if lhs.is_numeric() => {
            if matches!(op, BinaryOp::Mul) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Duration), rhs) if rhs.is_numeric() => {
            if matches!(op, BinaryOp::Mul | BinaryOp::Div) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs, rhs) => Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        }),
    }
}

pub(crate) fn infer_unary(
    op: &Spanned<UnaryOp>,
    expr: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let typed_expr = infer(expr, env)?;
    let span = op.span.merge(typed_expr.span);

    let result_ty = match op.value {
        UnaryOp::Neg => match &typed_expr.ty {
            QueryType::Scalar(ScalarType::Int) => QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::Float) => QueryType::Scalar(ScalarType::Float),
            QueryType::Scalar(ScalarType::Dec) => QueryType::Scalar(ScalarType::Dec),
            QueryType::Scalar(ScalarType::Duration) => QueryType::Scalar(ScalarType::Duration),
            _ => {
                return Err(TypeError::UnaryOpTypeMismatch {
                    op: op.to_string(),
                    operand: typed_expr.ty.to_string(),
                    span,
                });
            }
        },
        UnaryOp::Not => {
            if !typed_expr.ty.is_bool() {
                return Err(TypeError::UnaryOpTypeMismatch {
                    op: op.to_string(),
                    operand: typed_expr.ty.to_string(),
                    span,
                });
            }
            QueryType::Scalar(ScalarType::Bool)
        }
    };

    Ok(env.typed(
        TypedExprKind::Unary {
            op: op.clone(),
            expr: Box::new(typed_expr),
        },
        result_ty,
        span,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::infer::infer;
    use crate::typecheck::tests::{enum_schema, error_codes, infer_ok, test_schema};

    #[test]
    fn enum_equality_and_membership() {
        for source in [
            "Priority::High == Priority::Low",
            "Priority::High != Priority::Low",
            "Priority::High in (Priority::Low, Priority::High)",
            "[Priority::Low, Priority::High].contains(Priority::High)",
        ] {
            assert_eq!(
                infer_ok(source, &enum_schema()).ty,
                QueryType::Scalar(ScalarType::Bool),
                "source: {source}"
            );
        }
    }

    #[test]
    fn enum_raw_value_comparison_rejected() {
        assert_eq!(
            error_codes(r#"Priority::High == "high""#, &enum_schema()),
            vec!["QT0007"]
        );
    }

    #[test]
    fn different_enum_comparison_rejected() {
        assert_eq!(
            error_codes("Priority::Low == Level::Low", &enum_schema()),
            vec!["QT0007"]
        );
    }

    #[test]
    fn enum_ordering_rejected() {
        let schema = enum_schema();
        assert_eq!(
            error_codes("Priority::High > Priority::Low", &schema),
            vec!["QT0007"]
        );
        assert_eq!(
            error_codes("[Priority::Low, Priority::High].sort()", &schema),
            vec!["QT0003"]
        );
    }

    #[test]
    fn bytes_comparison() {
        let schema = test_schema();
        for source in [
            r#"hex"00" == hex"01""#,
            r#"hex"00" != hex"01""#,
            r#"hex"00" < hex"01""#,
            r#"hex"00" <= hex"01""#,
            r#"hex"00" > hex"01""#,
            r#"hex"00" >= hex"01""#,
            r#"hex"00" in (hex"00", hex"01")"#,
        ] {
            assert_eq!(
                infer_ok(source, &schema).ty,
                QueryType::Scalar(ScalarType::Bool),
                "source: {source}"
            );
        }
        assert_eq!(
            infer_ok(r#"[hex"02", hex"01"].sort()"#, &schema).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Bytes)))
        );
        assert_eq!(
            infer_ok(r#"[hex"02", hex"01"].min()"#, &schema).ty,
            QueryType::Scalar(ScalarType::Bytes).wrap_optional()
        );
        assert_eq!(
            infer_ok(r#"[hex"02", hex"01"].max()"#, &schema).ty,
            QueryType::Scalar(ScalarType::Bytes).wrap_optional()
        );
    }

    #[test]
    fn tuple_in_membership() {
        assert_eq!(
            infer_ok(r#"1 in (1, 2, 3)"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_int_add() {
        assert_eq!(
            infer_ok("1 + 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_sub() {
        assert_eq!(
            infer_ok("5 - 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_mul() {
        assert_eq!(
            infer_ok("4 * 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_div() {
        assert_eq!(
            infer_ok("10 / 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_rem() {
        assert_eq!(
            infer_ok("10 % 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_mod() {
        assert_eq!(
            infer_ok("10 mod 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_float_add() {
        assert_eq!(
            infer_ok("1.0f + 2.0f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn binary_dec_add() {
        assert_eq!(
            infer_ok("1.0 + 2.0", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn binary_int_float_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 + 2.0f");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_float_dec_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f + 2.0");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_add() {
        assert_eq!(
            infer_ok("#30d + #5h", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_sub() {
        assert_eq!(
            infer_ok("#30d - #5h", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mul_int() {
        assert_eq!(
            infer_ok("#30d * 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_int_mul_duration() {
        assert_eq!(
            infer_ok("3 * #30d", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mul_float() {
        assert_eq!(
            infer_ok("#30d * 1.5f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mul_dec() {
        assert_eq!(
            infer_ok("#30d * 1.5", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_div_int() {
        assert_eq!(
            infer_ok("#30d / 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_rem_int_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d % 7");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_mod_int_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d mod 7");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_div_duration() {
        assert_eq!(
            infer_ok("#90s / #30s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn binary_duration_rem_duration() {
        assert_eq!(
            infer_ok("#90s % #7s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mod_duration() {
        assert_eq!(
            infer_ok("#90s mod #7s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_add_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d + 3");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_mul_duration_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d * #5h");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_comparison_eq() {
        assert_eq!(
            infer_ok("1 == 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_ne() {
        assert_eq!(
            infer_ok("1.0f != 2e5f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_lt() {
        assert_eq!(
            infer_ok("1 < 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_gt() {
        assert_eq!(
            infer_ok("10.5 > 11.0", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_le() {
        assert_eq!(
            infer_ok("20 <= 40", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_ge() {
        assert_eq!(
            infer_ok("1001 >= 1000", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_string_eq() {
        assert_eq!(
            infer_ok(r#""a" == "b""#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_string_lt() {
        assert_eq!(
            infer_ok(r#""x" < "y""#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 == "a""#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_logical_and() {
        assert_eq!(
            infer_ok("true && false", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_logical_or() {
        assert_eq!(
            infer_ok("false || true", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_logical_and_non_bool_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 && 2");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_in_string_tuple() {
        assert_eq!(
            infer_ok(r#""a" in ("a", "b", "c")"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_in_non_tuple_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 in 2");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_in_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 in ("a", "b")"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_eq_optional() {
        assert_eq!(
            infer_ok(
                "users.first().unwrap().profile == users.first().unwrap().profile",
                &test_schema(),
            )
            .ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn optional_eq_non_optional_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none != 1");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_ordering_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none < none");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_neg_int() {
        assert_eq!(
            infer_ok("-1", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn unary_neg_float() {
        assert_eq!(
            infer_ok("-3e6f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn unary_neg_dec() {
        assert_eq!(
            infer_ok("-12.5", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn unary_neg_duration() {
        assert_eq!(
            infer_ok("-#30d", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn unary_neg_instant_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-@now");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_neg_string_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"-"hello""#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_not_bool() {
        assert_eq!(
            infer_ok("!true", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn unary_not_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("!1");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_arithmetic_composition() {
        assert_eq!(
            infer_ok("1 + 2 * (3 / 2) % (7 - 2)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_arithmetic_comparison() {
        assert_eq!(
            infer_ok("(1 + 2 - 3) >= 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_field_arithmetic() {
        assert_eq!(
            infer_ok("users.first().unwrap().age + 1", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_field_comparison() {
        assert_eq!(
            infer_ok("users.first().unwrap().age < 18", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_duration_arithmetic() {
        assert_eq!(
            infer_ok("#30d + #24h - #7d + #7.8s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }
}
