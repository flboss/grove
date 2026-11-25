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
