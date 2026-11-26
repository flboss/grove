use std::collections::{HashMap, HashSet};

use super::infer;
use crate::ast::{Expr, ProjectionItem};
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{
    ProjectionField, QueryType, RecordSource, TypedExpr, TypedExprKind, TypedProjectionItem,
};
use grove_schema::validated::ValidatedSchema;
use grove_types::{Span, Spanned};

pub(crate) fn infer_struct_literal(
    fields: &[(Spanned<String>, Expr)],
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_fields = Vec::with_capacity(fields.len());
    let mut proj_fields = Vec::with_capacity(fields.len());
    let mut seen_names = HashSet::with_capacity(fields.len());

    for (name, value) in fields {
        if !seen_names.insert(name.as_str()) {
            return Err(TypeError::DuplicateStructField {
                name: name.value.clone(),
                span: name.span,
            });
        }
        let typed_value = infer(value, env)?;
        proj_fields.push(ProjectionField {
            name: name.value.clone(),
            ty: typed_value.ty.clone(),
        });
        typed_fields.push((name.clone(), typed_value));
    }

    let result_ty = QueryType::Record(RecordSource::Projection(proj_fields));
    Ok(env.typed(
        TypedExprKind::Struct {
            fields: typed_fields,
        },
        result_ty,
        span,
    ))
}

pub(crate) fn infer_projection(
    base: &Expr,
    items: &[ProjectionItem],
    spread: Option<Span>,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    if items.is_empty() && spread.is_none() {
        return Err(TypeError::EmptyProjection { span });
    }

    let typed_base = infer(base, env)?;

    match &typed_base.ty {
        QueryType::Record(_) => {}
        QueryType::List(inner) if matches!(**inner, QueryType::Record(_)) => {}
        _ => {
            return Err(TypeError::ProjectionBaseNotRecord {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    }

    let pushed_scope = env.push_record_fields(&typed_base.ty);

    let mut typed_items = Vec::with_capacity(items.len());
    let mut seen_names: HashMap<&str, Span> = HashMap::new();

    for item in items {
        let (name, typed_value) = match (&item.alias, &item.value) {
            (Some(name), value)
            | (None, value @ Expr::Ident(name))
            | (None, value @ Expr::Field { name, .. }) => {
                let typed_value = infer(value, env)?;
                (name, typed_value)
            }
            (None, _) => {
                return Err(TypeError::UnnamedComputedField {
                    span: item.value.span(),
                });
            }
        };

        if let Some(&previous) = seen_names.get(name.as_str()) {
            return Err(TypeError::DuplicateProjectionField {
                name: name.to_string(),
                span: item.value.span(),
                previous,
            });
        }
        seen_names.insert(name.as_str(), name.span);

        typed_items.push(TypedProjectionItem {
            alias: Spanned::new(item.value.span(), name.to_string()),
            value: typed_value,
        });
    }

    if let Some(spread_span) = spread {
        for field_name in projection_base_fields(&typed_base.ty, env.schema()) {
            if seen_names.contains_key(field_name.as_str()) {
                continue;
            }
            let alias = Spanned::new(spread_span, field_name);
            let value = Expr::Ident(alias);
            let typed_value = infer(&value, env)?;
            let Expr::Ident(alias) = value else {
                unreachable!();
            };
            typed_items.push(TypedProjectionItem {
                alias,
                value: typed_value,
            });
        }
    }

    let is_list = matches!(&typed_base.ty, QueryType::List(_));
    let proj_source = RecordSource::Projection(
        typed_items
            .iter()
            .map(|t| ProjectionField {
                name: t.alias.to_string(),
                ty: t.value.ty.clone(),
            })
            .collect(),
    );
    let result_ty = if is_list {
        QueryType::List(Box::new(QueryType::Record(proj_source)))
    } else {
        QueryType::Record(proj_source)
    };

    if pushed_scope {
        env.pop_scope();
    }

    Ok(env.typed(
        TypedExprKind::Projection {
            base: Box::new(typed_base),
            items: typed_items,
        },
        result_ty,
        span,
    ))
}

pub(crate) fn projection_base_fields(ty: &QueryType, schema: &ValidatedSchema) -> Vec<String> {
    match ty {
        QueryType::Record(RecordSource::Schema(struct_id)) => schema.structs[struct_id.index()]
            .fields
            .iter()
            .map(|field| field.name().to_string())
            .collect(),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().map(|field| field.name.clone()).collect()
        }
        QueryType::List(inner) => projection_base_fields(inner, schema),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::tests::{enum_schema, infer_ok, projected_fields, test_schema};
    use grove_schema::validated::ScalarType;

    #[test]
    fn projection_basic() {
        assert!(matches!(
            infer_ok("users { name, age }", &test_schema()).ty,
            QueryType::List(inner)
            if matches!(
                *inner,
                QueryType::Record(RecordSource::Projection(ref fields))
                if fields.len() == 2
            )
        ));
    }

    #[test]
    fn projection_single_field() {
        let result = infer_ok("users { name }", &test_schema());
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 1);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[0].ty, QueryType::Scalar(ScalarType::String));
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_computed_field() {
        let result = infer_ok(
            "users { name, adult = users.first().unwrap().age > 18 }",
            &test_schema(),
        );
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[1].name, "adult");
                    assert_eq!(fields[1].ty, QueryType::Scalar(ScalarType::Bool));
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_spread_all_fields() {
        let result = infer_ok("users { ... }", &test_schema());
        assert_eq!(
            projected_fields(result).0,
            [
                "name",
                "age",
                "score",
                "balance",
                "active",
                "created",
                "profile",
                "maybe_pos",
            ]
        );
    }

    #[test]
    fn projection_spread_existing_preserved() {
        let result = infer_ok("users { active, ... }", &test_schema());
        assert_eq!(
            projected_fields(result).0,
            [
                "active",
                "name",
                "age",
                "score",
                "balance",
                "created",
                "profile",
                "maybe_pos",
            ]
        );
    }

    #[test]
    fn projection_spread_matched_by_name() {
        let result = infer_ok("users { n = name, ... }", &test_schema());
        assert_eq!(
            projected_fields(result).0,
            [
                "n",
                "name",
                "age",
                "score",
                "balance",
                "active",
                "created",
                "profile",
                "maybe_pos",
            ]
        );

        let result = infer_ok("users { name = 1, ... }", &test_schema());
        let fields = projected_fields(result);
        assert_eq!(fields.0[0], "name");
        assert_eq!(fields.1[0], QueryType::Scalar(ScalarType::Int));
        assert_eq!(
            fields.0,
            [
                "name",
                "age",
                "score",
                "balance",
                "active",
                "created",
                "profile",
                "maybe_pos",
            ]
        );
    }

    #[test]
    fn projection_spread_chained_projection() {
        let result = infer_ok("users { name, age } { age, ... }", &test_schema());
        assert_eq!(projected_fields(result).0, ["age", "name"]);
    }

    #[test]
    fn projection_spread_literal() {
        let result = infer_ok("{ a = 1, b = 2 } { b, ... }", &test_schema());
        assert_eq!(projected_fields(result).0, ["b", "a"]);
    }

    #[test]
    fn projection_spread_noop_all_fields_named() {
        let result = infer_ok(
            "users { name, age, score, balance, active, created, profile, maybe_pos, ... }",
            &test_schema(),
        );
        assert_eq!(projected_fields(result).0.len(), 8);
    }

    #[test]
    fn enum_optional_and_projection() {
        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;

        assert_eq!(
            infer_ok("some(Priority::High)", &schema).ty,
            QueryType::Optional(Box::new(QueryType::Enum(priority_id)))
        );

        let result = infer_ok("tickets { priority }", &schema);
        match result.ty {
            QueryType::List(inner) => match *inner {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields[0].ty, QueryType::Enum(priority_id));
                }
                other => panic!("expected projection record, got {other:?}"),
            },
            other => panic!("expected list projection, got {other:?}"),
        }
    }

    #[test]
    fn projection_empty_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { }");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_duplicate_field_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { name, name }");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_on_non_record_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("42 { name }");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_derived_alias() {
        let result = infer_ok("users { name, profile?.username }", &test_schema());
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[1].name, "username");
                    assert_eq!(
                        fields[1].ty,
                        QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String)))
                    );
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_single_record_base() {
        assert!(matches!(
            infer_ok("users.first().unwrap() { name, age }", &test_schema()).ty,
            QueryType::Record(RecordSource::Projection(_))
        ));
    }
}
