use super::infer;
use crate::ast::Expr;
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{
    QueryType, RecordSource, TargetProvenance, TypedExpr, TypedExprKind,
};
use grove_types::Span;

pub(crate) fn infer_row_ref(
    expr: &Expr,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    if !env.allow_row_refs {
        return Err(TypeError::RowRefNotAllowed { span });
    }
    let typed = infer(expr, env)?;
    let ty = row_ref_shape(&typed.ty, typed.provenance).ok_or_else(|| {
        TypeError::RowRefSourceNotTraceable {
            got: typed.ty.to_string(),
            span,
        }
    })?;
    Ok(env.typed(
        TypedExprKind::RowRef {
            expr: Box::new(typed),
        },
        ty,
        span,
    ))
}

/// Map traceable schema records to RowRefs through optional and list layers
/// (apply `&`-operation)
pub(crate) fn row_ref_shape(ty: &QueryType, provenance: TargetProvenance) -> Option<QueryType> {
    match ty {
        QueryType::Record(RecordSource::Schema(struct_id)) if provenance.rows => {
            Some(QueryType::RowRef(*struct_id))
        }
        QueryType::Optional(inner) => {
            row_ref_shape(inner, provenance).map(|inner| QueryType::Optional(Box::new(inner)))
        }
        QueryType::List(inner) => {
            row_ref_shape(inner, provenance).map(|inner| QueryType::List(Box::new(inner)))
        }
        _ => None,
    }
}

pub(crate) fn infer_mutation_arg(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    let previous = env.allow_row_refs;
    env.allow_row_refs = true;
    let result = infer(expr, env);
    env.allow_row_refs = previous;
    result
}
