mod access;
mod cast;
mod construct;
mod method;
mod operator;
mod row_ref;

use self::row_ref::infer_row_ref;

pub(crate) use self::row_ref::infer_mutation_arg;
use crate::ast::{Expr, Literal};
use crate::typecheck::compat::types_compatible;
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::infer::access::{infer_field, infer_tuple_access};
use crate::typecheck::infer::cast::{infer_cast, infer_type_constant};
use crate::typecheck::infer::construct::{infer_projection, infer_struct_literal};
use crate::typecheck::infer::method::infer_method;
use crate::typecheck::infer::operator::{infer_binary, infer_unary};
use crate::typecheck::types::{QueryType, TypedExpr, TypedExprKind};
use grove_schema::validated::ScalarType;
use grove_types::Span;

pub(crate) fn infer_literal(literal: &Literal) -> QueryType {
    match literal {
        Literal::Int(_) => QueryType::Scalar(ScalarType::Int),
        Literal::Float(_) => QueryType::Scalar(ScalarType::Float),
        Literal::Dec(_) => QueryType::Scalar(ScalarType::Dec),
        Literal::String(_) => QueryType::Scalar(ScalarType::String),
        Literal::Bytes(_) => QueryType::Scalar(ScalarType::Bytes),
        Literal::Bool(_) => QueryType::Scalar(ScalarType::Bool),
        Literal::Instant(_) | Literal::Now | Literal::Today(_) => {
            QueryType::Scalar(ScalarType::Instant)
        }
        Literal::Duration(_) => QueryType::Scalar(ScalarType::Duration),
        Literal::None => QueryType::Unknown.wrap_optional(),
    }
}
pub(crate) fn infer(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(lit) => {
            let ty = infer_literal(lit);
            Ok(env.typed(TypedExprKind::Literal(lit.clone()), ty, lit.span))
        }
        Expr::Ident(name) => {
            let (entry, depth) = env
                .resolve(name)
                .ok_or_else(|| TypeError::UnknownIdentifier {
                    name: name.to_string(),
                    span: name.span,
                })?;
            let binding = entry.binding.clone();
            let ty = entry.ty.clone();
            let provenance = entry.provenance;
            Ok(env.typed_with_provenance(
                TypedExprKind::Ident {
                    name: name.clone(),
                    binding,
                    depth,
                },
                ty,
                provenance,
                name.span,
            ))
        }
        Expr::EnumVariant {
            enum_name,
            variant,
            span,
        } => {
            let Some((enum_id, enum_def)) = env.schema().enum_by_name(enum_name) else {
                return Err(TypeError::UnknownEnum {
                    name: enum_name.value.clone(),
                    span: enum_name.span,
                });
            };
            let Some(variant) = enum_def.variant_index(variant) else {
                return Err(TypeError::UnknownEnumVariant {
                    enum_name: enum_name.value.clone(),
                    variant: variant.value.clone(),
                    span: variant.span,
                });
            };
            Ok(env.typed(
                TypedExprKind::EnumVariant { enum_id, variant },
                QueryType::Enum(enum_id),
                *span,
            ))
        }
        Expr::Field {
            base,
            name,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_field(typed_base, name, *opt_depth, env)
        }
        Expr::TupleAccess {
            base,
            index,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_tuple_access(typed_base, index, *opt_depth, env)
        }
        Expr::Method {
            base,
            name,
            args,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_method(typed_base, name, args, *opt_depth, env)
        }
        Expr::Binary { op, lhs, rhs, .. } => infer_binary(op, lhs, rhs, env),
        Expr::Unary { op, expr, .. } => infer_unary(op, expr, env),
        Expr::RowRef { expr, span } => infer_row_ref(expr, *span, env),
        Expr::Tuple { elements, .. } => {
            let mut typed_elems = Vec::new();
            for elem in elements {
                typed_elems.push(infer(elem, env)?);
            }
            let ty = QueryType::Tuple(typed_elems.iter().map(|e| e.ty.clone()).collect());
            Ok(env.typed(
                TypedExprKind::Tuple {
                    elements: typed_elems,
                },
                ty,
                expr.span(),
            ))
        }
        Expr::Array { elements, span } => infer_array(elements, *span, env),
        Expr::Some { value, .. } => {
            let typed_value = infer(value, env)?;
            let ty = typed_value.ty.clone().wrap_optional();
            let provenance = typed_value.provenance;
            Ok(env.typed_with_provenance(
                TypedExprKind::Some {
                    value: Box::new(typed_value),
                },
                ty,
                provenance,
                expr.span(),
            ))
        }
        Expr::If {
            arms,
            default,
            span,
        } => infer_if(arms, default, *span, env),
        Expr::Cast {
            expr, ty, fallible, ..
        } => infer_cast(expr, ty, *fallible, env),
        Expr::Projection {
            base,
            items,
            spread,
            span,
        } => infer_projection(base, items, *spread, *span, env),
        Expr::TypeConstant { ty, name } => infer_type_constant(ty, name, env),
        Expr::Struct { fields, span } => infer_struct_literal(fields, *span, env),
    }
}
pub(crate) fn infer_array(
    elements: &[Expr],
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_elems = Vec::with_capacity(elements.len());
    for elem in elements {
        typed_elems.push(infer(elem, env)?);
    }

    let mut ty = QueryType::Unknown;
    for elem in &mut typed_elems {
        if !types_compatible(&mut ty, &mut elem.ty, env.schema()) {
            return Err(TypeError::ArrayElementTypeMismatch {
                expected: ty.to_string(),
                got: elem.ty.to_string(),
                span: elem.span,
            });
        }
    }

    if !matches!(ty, QueryType::Unknown) {
        for elem in &mut typed_elems {
            types_compatible(&mut ty, &mut elem.ty, env.schema());
        }
    }

    let ty = QueryType::List(Box::new(ty));

    Ok(env.typed(
        TypedExprKind::Array {
            elements: typed_elems,
        },
        ty,
        span,
    ))
}
pub(crate) fn infer_if(
    arms: &[(Expr, Expr)],
    default: &Expr,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_arms = Vec::with_capacity(arms.len());
    let mut typed_default = infer(default, env)?;

    for (cond, body) in arms {
        let typed_cond = infer(cond, env)?;
        if !typed_cond.ty.is_bool() {
            return Err(TypeError::IfConditionNotBool {
                span: typed_cond.span,
            });
        }
        let typed_body = infer(body, env)?;
        typed_arms.push((typed_cond, typed_body));
    }

    for (_, body) in &mut typed_arms {
        if !types_compatible(&mut typed_default.ty, &mut body.ty, env.schema()) {
            return Err(TypeError::IfBranchTypeMismatch {
                expected: typed_default.ty.to_string(),
                got: body.ty.to_string(),
                span: body.span,
            });
        }
    }

    for (_, body) in &mut typed_arms {
        types_compatible(&mut typed_default.ty, &mut body.ty, env.schema());
    }

    let ty = typed_default.ty.clone();
    let mut provenance = typed_default.provenance;
    for (_, body) in &typed_arms {
        provenance = provenance.merge_branches(body.provenance);
    }

    Ok(env.typed_with_provenance(
        TypedExprKind::If {
            arms: typed_arms,
            default: Box::new(typed_default),
        },
        ty,
        provenance,
        span,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::tests::{enum_schema, infer_ok, test_schema};

    #[test]
    fn infer_enum_variant() {
        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;
        let result = infer_ok("Priority::Medium", &schema);
        assert_eq!(result.ty, QueryType::Enum(priority_id));
        assert!(matches!(
            result.kind,
            TypedExprKind::EnumVariant {
                enum_id,
                variant: 1
            } if enum_id == priority_id
        ));
    }

    #[test]
    fn unknown_enum_rejected() {
        assert_eq!(error_codes_for_enum("Missing::High"), vec!["QT0030"]);
    }

    #[test]
    fn unknown_enum_variant_rejected() {
        assert_eq!(error_codes_for_enum("Priority::Urgent"), vec!["QT0031"]);
    }

    #[test]
    fn infer_int_literal() {
        assert_eq!(
            infer_ok("42", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn infer_float_literal() {
        assert_eq!(
            infer_ok("3.14f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn infer_dec_literal() {
        assert_eq!(
            infer_ok("3.14", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn infer_string_literal() {
        assert_eq!(
            infer_ok("\"hello\"", &test_schema()).ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn infer_bytes_literal() {
        for source in [r#"hex"deadbeef""#, r#"b64"3q2+7w==""#] {
            assert_eq!(
                infer_ok(source, &test_schema()).ty,
                QueryType::Scalar(ScalarType::Bytes),
                "source: {source}"
            );
        }
    }

    #[test]
    fn infer_bool_literal() {
        assert_eq!(
            infer_ok("true", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn infer_instant_literal() {
        assert_eq!(
            infer_ok("@now", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Instant)
        );
    }

    #[test]
    fn infer_duration_literal() {
        assert_eq!(
            infer_ok("#30d", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn none_without_context_unknown() {
        assert_eq!(
            infer_ok("none", &test_schema()).ty,
            QueryType::Unknown.wrap_optional()
        );
    }

    #[test]
    fn if_else_basic() {
        assert_eq!(
            infer_ok(r#"if true { 1 } else { 2 }"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn if_else_bool_result() {
        assert_eq!(
            infer_ok(r#"if true { false } else { true }"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn if_else_condition_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if 42 { 1 } else { 2 }"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn if_else_branch_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if true { 1 } else { "two" }"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn if_else_if_chain() {
        assert_eq!(
            infer_ok(
                r#"if true { "A" } else if false { "B" } else { "C" }"#,
                &test_schema(),
            )
            .ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn if_else_comparison_condition() {
        assert_eq!(
            infer_ok(
                r#"if users.first().unwrap().age > 18 { "adult" } else { "minor" }"#,
                &test_schema(),
            )
            .ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn infer_if_else_optional_list() {
        assert_eq!(
            infer_ok(
                "if true { none }
                else {
                    if false { some([]) }
                    else { some([1, 2, 3]) }
                }",
                &test_schema(),
            )
            .ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))).wrap_optional()
        );
    }

    #[test]
    fn infer_if_else_ambiguous() {
        let (file, _) = crate::parse_query("if true { none } else { none }");
        let (_typed, diags) = crate::typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn some_basic() {
        assert_eq!(
            infer_ok(r#"some("hello")"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::String).wrap_optional()
        );
    }

    #[test]
    fn some_complex_expr() {
        assert_eq!(
            infer_ok("some(users.first().unwrap().age)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn some_none() {
        assert_eq!(
            infer_ok("none", &test_schema()).ty,
            QueryType::Unknown.wrap_optional()
        );
    }

    #[test]
    fn some_nested_optional() {
        assert_eq!(
            infer_ok(r#"some(some("a"))"#, &test_schema()).ty,
            QueryType::Optional(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                ScalarType::String
            )))))
        );
    }

    #[test]
    fn infer_optional_comparison_error() {
        let (file, _) = crate::parse_query("some(2) == 1");
        let (_typed, diags) = crate::typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn infer_optional_ambiguous() {
        let (file, _) = crate::parse_query("none == none");
        let (_typed, diags) = crate::typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn infer_nested_optional() {
        assert_eq!(
            infer_ok(r#"some(some("a")) != some(none)"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn infer_array_optional() {
        assert_eq!(
            infer_ok("[none, some(3), none]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
        );
    }

    #[test]
    fn array_int() {
        assert_eq!(
            infer_ok("[1, 2, 3]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn array_string() {
        assert_eq!(
            infer_ok(r#"["a", "b", "c"]"#, &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn array_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn array_types_inferred() {
        let result = infer_ok("[none, some(1), none]", &test_schema());
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
        );
        match result.kind {
            TypedExprKind::Array { elements } => {
                assert_eq!(
                    elements.into_iter().map(|e| e.ty).collect::<Vec<_>>(),
                    vec![QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int))); 3]
                )
            }
            _ => panic!(),
        }
    }

    #[test]
    fn array_empty() {
        assert_eq!(
            infer_ok("[]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Unknown))
        );
    }

    #[test]
    fn tuple_mixed_types() {
        assert_eq!(
            infer_ok(r#"(1, "hello", false, @now, 1.7e-5)"#, &test_schema()).ty,
            QueryType::Tuple(vec![
                QueryType::Scalar(ScalarType::Int),
                QueryType::Scalar(ScalarType::String),
                QueryType::Scalar(ScalarType::Bool),
                QueryType::Scalar(ScalarType::Instant),
                QueryType::Scalar(ScalarType::Dec),
            ])
        );
    }

    #[test]
    fn infer_if_else_tuple() {
        assert_eq!(
            infer_ok("if true { (1, 2) } else { (3, 4) }", &test_schema()).ty,
            QueryType::Tuple(vec![
                QueryType::Scalar(ScalarType::Int),
                QueryType::Scalar(ScalarType::Int),
            ])
        );
    }

    #[test]
    fn infer_tuple_mismatch() {
        let (file, _) = crate::parse_query(r#"if true { (1, 2, 4) } else { (4, "abc") }"#);
        let (_typed, diags) = crate::typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    fn error_codes_for_enum(source: &str) -> Vec<String> {
        crate::typecheck::tests::error_codes(source, &enum_schema())
    }
}
