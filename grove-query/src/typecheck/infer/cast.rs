use super::infer;
use crate::ast::{Expr, TypeExpr, TypeName};
use crate::casts::{self, CastMode};
use crate::typecheck::compat::{check, types_compatible};
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{ProjectionField, QueryType, RecordSource, TypedExpr, TypedExprKind};
use grove_schema::validated::{ScalarType, ValidatedSchema};
use grove_types::Spanned;

pub(crate) fn resolve_type_expr(
    ty: &Spanned<TypeExpr>,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    match &ty.value {
        &TypeExpr::Scalar(scalar) => Ok(scalar.into()),
        TypeExpr::Named(name) => {
            if let Some((enum_id, _)) = schema.enum_by_name(name) {
                return Ok(QueryType::Enum(enum_id));
            }
            if let Some((struct_id, _)) = schema.struct_by_name(name) {
                return Ok(QueryType::Record(RecordSource::Schema(struct_id)));
            }
            Err(TypeError::UnknownTypeName {
                name: name.clone(),
                span: ty.span,
            })
        }
        TypeExpr::Optional { inner, .. } => Ok(resolve_type_expr(inner, schema)?.wrap_optional()),
        TypeExpr::List { element, .. } => Ok(QueryType::List(Box::new(resolve_type_expr(
            element, schema,
        )?))),
        TypeExpr::Tuple { elements, .. } => elements
            .iter()
            .map(|element| resolve_type_expr(element, schema))
            .collect::<Result<Vec<_>, _>>()
            .map(QueryType::Tuple),
        TypeExpr::Record { fields, .. } => {
            let mut projection = Vec::with_capacity(fields.len());
            for (name, field_ty) in fields {
                projection.push(ProjectionField {
                    name: name.value.clone(),
                    ty: resolve_type_expr(field_ty, schema)?,
                });
            }
            Ok(QueryType::Record(RecordSource::Projection(projection)))
        }
    }
}

pub(crate) fn infer_cast(
    expr: &Expr,
    target: &Spanned<TypeExpr>,
    fallible: bool,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_expr = infer(expr, env)?;
    let span = typed_expr.span.merge(target.span);
    let mut target_ty = resolve_type_expr(target, env.schema())?;
    // target_ty can never contain QueryType::Unknown, so it won't get
    // mutated in types_compatible() or check(); no clone necessary
    debug_assert!(!target_ty.is_unknown());

    if fallible {
        if typed_expr.ty.is_unknown() {
            return Err(TypeError::AmbiguousCast {
                from: typed_expr.ty.to_string(),
                to: target_ty.to_string(),
                span,
            });
        }
        let kind = casts::lookup(&typed_expr.ty, &target_ty, CastMode::Fallible, env.schema())
            .ok_or_else(|| TypeError::InvalidCast {
                from: typed_expr.ty.to_string(),
                to: target_ty.to_string(),
                span,
            })?;
        return Ok(env.typed(
            TypedExprKind::Cast {
                expr: Box::new(typed_expr),
                kind,
            },
            kind.target().wrap_optional(),
            span,
        ));
    }

    if typed_expr.ty.is_unknown() {
        let mut refined = typed_expr.ty.clone();
        if !types_compatible(&mut refined, &mut target_ty, env.schema()) {
            return Err(TypeError::AmbiguousCast {
                from: typed_expr.ty.to_string(),
                to: target_ty.to_string(),
                span,
            });
        }
        typed_expr.ty = refined;
        check(&mut typed_expr, &mut target_ty, env)?;
        if typed_expr.ty.eq(&target_ty, env.schema()) {
            return Ok(typed_expr);
        }
        return Err(TypeError::AmbiguousCast {
            from: typed_expr.ty.to_string(),
            to: target_ty.to_string(),
            span,
        });
    }

    if typed_expr.ty.eq(&target_ty, env.schema()) {
        return Ok(typed_expr);
    }

    let kind = casts::lookup(
        &typed_expr.ty,
        &target_ty,
        CastMode::Infallible,
        env.schema(),
    )
    .ok_or_else(|| TypeError::InvalidCast {
        from: typed_expr.ty.to_string(),
        to: target_ty.to_string(),
        span,
    })?;

    Ok(env.typed(
        TypedExprKind::Cast {
            expr: Box::new(typed_expr),
            kind,
        },
        kind.target(),
        span,
    ))
}

pub(crate) fn infer_type_constant(
    type_name: &Spanned<TypeName>,
    name: &Spanned<crate::ast::ConstantName>,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let scalar_ty = match type_name.value {
        TypeName::Int => ScalarType::Int,
        TypeName::Float => ScalarType::Float,
        TypeName::Dec => ScalarType::Dec,
        TypeName::String => unreachable!("String has no type constants"),
        TypeName::Bytes => unreachable!("Bytes has no type constants"),
        TypeName::Bool => unreachable!("Bool has no type constants"),
        TypeName::Instant => unreachable!("Instant has no type constants"),
        TypeName::Duration => unreachable!("Duration has no type constants"),
    };

    let result_ty = QueryType::Scalar(scalar_ty);
    let span = type_name.span.merge(name.span);

    Ok(env.typed(
        TypedExprKind::TypeConstant {
            ty: type_name.clone(),
            name: name.clone(),
        },
        result_ty,
        span,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::infer::infer;
    use crate::typecheck::tests::{
        annotation_schema, enum_schema, error_codes, infer_ok, test_schema,
    };

    #[test]
    fn enum_cast_to_underlying_value() {
        let schema = enum_schema();

        let result = infer_ok("Priority::High as String", &schema);
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
        assert!(matches!(result.kind, TypedExprKind::Cast { .. }));

        assert_eq!(
            infer_ok("Level::High as Int", &schema).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn enum_cast_to_wrong_underlying_type_rejected() {
        let schema = enum_schema();
        assert_eq!(
            error_codes("Priority::High as Int", &schema),
            vec!["QT0013"]
        );
        assert_eq!(
            error_codes("Level::High as String", &schema),
            vec!["QT0013"]
        );
    }

    #[test]
    fn bytes_cast_from_string() {
        assert_eq!(
            infer_ok(r#""hello" as Bytes"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bytes)
        );
    }

    #[test]
    fn annotations_disambiguation_only() {
        for (source, expected) in [
            (
                "[] as List<Int>",
                QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))),
            ),
            (
                "none as ?String",
                QueryType::Scalar(ScalarType::String).wrap_optional(),
            ),
            (
                "some(none) as ??Int",
                QueryType::Scalar(ScalarType::Int)
                    .wrap_optional()
                    .wrap_optional(),
            ),
            ("1 as Int", QueryType::Scalar(ScalarType::Int)),
        ] {
            let result = infer_ok(source, &test_schema());
            assert_eq!(result.ty, expected, "source: {source}");
            assert!(
                !matches!(&result.kind, TypedExprKind::Cast { .. }),
                "source should be a no-op: {source}"
            );
        }
    }

    #[test]
    fn annotations_complex_and_named() {
        let schema = test_schema();
        let result = infer_ok(
            "[] as List<(?Int, List<Duration>, ??String, {a: Instant, d: Dec})>",
            &schema,
        );
        assert!(!result.ty.is_unknown());
        assert!(matches!(result.ty, QueryType::List(_)));
        assert!(!matches!(&result.kind, TypedExprKind::Cast { .. }));

        let user_id = schema.roots[0].struct_id;
        assert_eq!(
            infer_ok("none as ?User", &schema).ty,
            QueryType::Record(RecordSource::Schema(user_id)).wrap_optional()
        );

        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;
        assert_eq!(
            infer_ok("none as ?Priority", &schema).ty,
            QueryType::Enum(priority_id).wrap_optional()
        );
    }

    #[test]
    fn annotations_ambiguous_and_invalid_casts() {
        let schema = test_schema();
        for source in ["none as Int", "[] as Int", "[none] as List<Int>"] {
            assert_eq!(
                error_codes(source, &schema),
                vec!["QT0034"],
                "source: {source}"
            );
        }
        assert_eq!(
            error_codes("{a = none} as Item", &annotation_schema()),
            vec!["QT0034"]
        );

        for source in [
            "none as ?String as Int",
            "[] as List<Int> as List<Float>",
            "1 as Bool",
            "1 as User",
        ] {
            assert_eq!(
                error_codes(source, &schema),
                vec!["QT0013"],
                "source: {source}"
            );
        }
    }

    #[test]
    fn cast_int_to_float() {
        assert_eq!(
            infer_ok("42 as Float", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn cast_int_to_dec() {
        assert_eq!(
            infer_ok("42 as Dec", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn cast_float_to_int() {
        assert_eq!(
            infer_ok("1.0f as Int", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn cast_float_to_dec() {
        assert_eq!(
            infer_ok("1.0f as Dec", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn cast_dec_to_int() {
        assert_eq!(
            infer_ok("1.0 as Int", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn cast_dec_to_float() {
        assert_eq!(
            infer_ok("1.0 as Float", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn cast_bool_to_int() {
        assert_eq!(
            infer_ok("true as Int", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn cast_disallowed_type() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#""hello" as Int"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn cast_composed_expr() {
        assert_eq!(
            infer_ok("users.first().unwrap().age as Float + 1.0f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn fallible_cast_optional_result() {
        assert_eq!(
            infer_ok(r#""42" as? Int"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
        assert_eq!(
            infer_ok(r#""true" as? Bool"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool).wrap_optional()
        );
    }

    #[test]
    fn fallible_cast_strict_rejection() {
        for source in [
            "1 as? Int",
            "some(1) as? Float",
            "1 as? ?Int",
            "true as? Float",
        ] {
            assert_eq!(
                error_codes(source, &test_schema()),
                vec!["QT0013"],
                "source: {source}"
            );
        }
    }

    #[test]
    fn fallible_enum_cast_matching_underlying_type_required() {
        let schema = enum_schema();
        assert_eq!(error_codes(r#""low" as? Level"#, &schema), vec!["QT0013"]);
        assert_eq!(error_codes("2 as? Priority", &schema), vec!["QT0013"]);
    }

    #[test]
    fn type_constant_int() {
        assert_eq!(
            infer_ok("Int::MAX", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn type_constant_float() {
        assert_eq!(
            infer_ok("Float::MIN", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn type_constant_dec() {
        assert_eq!(
            infer_ok("Dec::MAX", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }
}
