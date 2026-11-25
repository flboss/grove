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
