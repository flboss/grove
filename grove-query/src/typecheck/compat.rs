use grove_schema::validated::{StructId, ValidatedSchema};

use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{ProjectionField, QueryType, RecordSource, TypedExpr, TypedExprKind};

pub(super) fn types_compatible(
    a: &mut QueryType,
    b: &mut QueryType,
    schema: &ValidatedSchema,
) -> bool {
    match (a, b) {
        (QueryType::Unknown, QueryType::Unknown) => true,
        (a @ QueryType::Unknown, b) => {
            *a = b.clone();
            types_compatible(a, b, schema)
        }
        (a, b @ QueryType::Unknown) => {
            *b = a.clone();
            types_compatible(a, b, schema)
        }
        (QueryType::Optional(a), QueryType::Optional(b)) => types_compatible(a, b, schema),
        (QueryType::List(a), QueryType::List(b)) => types_compatible(a, b, schema),
        (QueryType::Scalar(a), QueryType::Scalar(b)) => a == b,
        (QueryType::Enum(a), QueryType::Enum(b)) => a == b,
        (QueryType::Tuple(a), QueryType::Tuple(b)) => {
            a.len() == b.len()
                && a.iter_mut()
                    .zip(b.iter_mut())
                    .all(|(a, b)| types_compatible(a, b, schema))
        }
        (QueryType::Record(a), QueryType::Record(b)) => record_fields_match(a, b, schema),
        (a, b) => a == b,
    }
}

pub(super) fn record_fields_match(
    a: &mut RecordSource,
    b: &mut RecordSource,
    schema: &ValidatedSchema,
) -> bool {
    match (a, b) {
        (RecordSource::Schema(a_id), RecordSource::Schema(b_id)) => a_id == b_id,
        (RecordSource::Schema(sid), RecordSource::Projection(proj))
        | (RecordSource::Projection(proj), RecordSource::Schema(sid)) => {
            record_schema_matches_projection(*sid, proj, schema)
        }
        (RecordSource::Projection(a_proj), RecordSource::Projection(b_proj)) => {
            if a_proj.len() != b_proj.len() {
                return false;
            }
            a_proj.iter_mut().all(|a_field| {
                b_proj.iter_mut().any(|b_field| {
                    b_field.name == a_field.name
                        && types_compatible(&mut a_field.ty, &mut b_field.ty, schema)
                })
            })
        }
    }
}

pub(super) fn record_schema_matches_projection(
    sid: StructId,
    proj: &mut Vec<ProjectionField>,
    schema: &ValidatedSchema,
) -> bool {
    let struct_ = &schema.structs[sid.index()];
    for proj_field in proj {
        let Some(schema_field) = struct_.fields.iter().find(|f| f.name() == proj_field.name) else {
            return false;
        };
        let mut schema_ty = QueryType::from(schema_field);
        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return false;
        }
    }
    true
}

pub(super) fn check(
    expr: &mut TypedExpr,
    expected: &mut QueryType,
    env: &mut TypeEnv,
) -> Result<(), TypeError> {
    let type_mismatch = |exp: String, got: String| {
        Err(TypeError::UnexpectedType {
            expected: exp.to_string(),
            got: got.to_string(),
            span: expr.span,
        })
    };

    if !types_compatible(&mut expr.ty, expected, env.schema()) {
        return type_mismatch(expected.to_string(), expr.ty.to_string());
    }

    if expr.ty.is_unknown() {
        return Err(TypeError::AmbiguousType { span: expr.span });
    }

    match &mut expr.kind {
        TypedExprKind::Literal(_)
        | TypedExprKind::Ident { .. }
        | TypedExprKind::EnumVariant { .. } => {}
        // no propagation
        TypedExprKind::Field { base, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        // no propagation
        TypedExprKind::TupleAccess { base, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        // no propagation
        TypedExprKind::Method { base, args, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
            for arg in args {
                let mut arg_ty = arg.expr.ty.clone();
                check(&mut arg.expr, &mut arg_ty, env)?;
            }
        }
        // no propagation
        TypedExprKind::Binary { lhs, rhs, .. } => {
            let mut lhs_ty = lhs.ty.clone();
            let mut rhs_ty = rhs.ty.clone();
            check(lhs, &mut lhs_ty, env)?;
            check(rhs, &mut rhs_ty, env)?;
        }
        TypedExprKind::Unary { expr: operand, .. } => {
            check(operand, &mut expr.ty, env)?;
        }
        TypedExprKind::RowRef { expr: operand } => {
            let mut ty = operand.ty.clone();
            check(operand, &mut ty, env)?;
        }
        // no propagation
        TypedExprKind::Cast { expr: operand, .. } => {
            let mut ty = operand.ty.clone();
            check(operand, &mut ty, env)?;
        }
        TypedExprKind::If { arms, default } => {
            arms.iter_mut()
                .map(|(_, b)| b)
                .chain(std::iter::once(default.as_mut()))
                .try_for_each(|b| check(b, expected, env))?;
        }
        TypedExprKind::Some { value } => {
            let QueryType::Optional(inner) = &mut expr.ty else {
                return type_mismatch("?T".into(), expr.ty.to_string());
            };
            check(value.as_mut(), inner, env)?;
        }
        TypedExprKind::Tuple { elements } => {
            let QueryType::Tuple(inners) = &mut expr.ty else {
                return type_mismatch("Tuple".into(), expr.ty.to_string());
            };
            elements
                .iter_mut()
                .zip(inners.iter_mut())
                .try_for_each(|(elem, ty)| check(elem, ty, env))?;
        }
        TypedExprKind::Array { elements } => {
            let QueryType::List(inner) = &mut expr.ty else {
                return type_mismatch("List<T>".into(), expr.ty.to_string());
            };
            elements.iter_mut().try_for_each(|e| check(e, inner, env))?;
        }
        TypedExprKind::Struct { fields } => {
            let QueryType::Record(RecordSource::Projection(proj_fields)) = &mut expr.ty else {
                return type_mismatch("Record".into(), expr.ty.to_string());
            };
            fields.iter_mut().try_for_each(|(name, expr)| {
                let Some(field) = proj_fields
                    .iter_mut()
                    .find(|field| field.name == name.as_str())
                else {
                    return type_mismatch(
                        format!("Record with field `{}`", name.as_str()),
                        expr.ty.to_string(),
                    );
                };
                check(expr, &mut field.ty, env)
            })?;
        }
        // no propagation
        TypedExprKind::Projection { base, items } => {
            items.iter_mut().try_for_each(|item| {
                let mut item_ty = item.value.ty.clone();
                check(&mut item.value, &mut item_ty, env)
            })?;
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        TypedExprKind::TypeConstant { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::tests::{annotation_schema, infer_ok};

    #[test]
    fn annotations_record_equality() {
        let schema = annotation_schema();
        for source in [
            "{a = none, b = (1, 2)} as Item",
            "{b = (1, 2), a = none} as {b: (Int, Int), a: ?String}",
            "items.first().unwrap() as {a: ?String, b: (Int, Int)}",
        ] {
            let result = infer_ok(source, &schema);
            assert!(!result.ty.is_unknown(), "source: {source}");
            assert!(
                !matches!(&result.kind, TypedExprKind::Cast { .. }),
                "source should be a no-op: {source}"
            );
        }
    }
}
