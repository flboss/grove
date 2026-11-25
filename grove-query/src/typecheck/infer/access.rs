use crate::typecheck::env::{TypeEnv, record_field_by_name, record_field_provenance};
use crate::typecheck::error::TypeError;
use crate::typecheck::infer::method::row_ref_container_leaf;
use crate::typecheck::types::{QueryType, TypedExpr, TypedExprKind};

use grove_types::Spanned;

pub(crate) fn infer_field(
    base: TypedExpr,
    name: &Spanned<String>,
    opt_depth: usize,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let schema = env.schema();
    let lookup_ty = base.ty.unwrap_optional_depth(opt_depth).ok_or_else(|| {
        TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: base.ty.to_string(),
            span: name.span,
        }
    })?;

    let (inner, was_list) = match lookup_ty {
        QueryType::List(inner) => (inner.as_ref(), true),
        other => (other, false),
    };

    if row_ref_container_leaf(inner) {
        return Err(TypeError::RowRefOperationNotAllowed {
            operation: format!("field access `.{}`", name.value),
            span: name.span,
        });
    }

    let mut field_ty =
        record_field_by_name(inner, name, schema).ok_or_else(|| TypeError::UnknownField {
            field: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        })?;
    let provenance = record_field_provenance(inner, name, schema);

    if was_list {
        field_ty = QueryType::List(Box::new(field_ty));
    }
    let field_ty = field_ty.wrap_optional_depth(opt_depth);

    let span = base.span.merge(name.span);
    Ok(env.typed_with_provenance(
        TypedExprKind::Field {
            base: Box::new(base),
            name: name.clone(),
            opt_depth,
        },
        field_ty,
        provenance,
        span,
    ))
}

pub(crate) fn infer_tuple_access(
    base: TypedExpr,
    index: &Spanned<i64>,
    opt_depth: usize,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let lookup_ty = base.ty.unwrap_optional_depth(opt_depth).ok_or_else(|| {
        TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: base.ty.to_string(),
            span: index.span,
        }
    })?;
    let QueryType::Tuple(elems) = lookup_ty else {
        return Err(TypeError::TupleAccessOnNonTuple {
            base_ty: base.ty.to_string(),
            span: index.span,
        });
    };
    let pos = index.value as usize;
    if pos >= elems.len() {
        return Err(TypeError::TupleIndexOutOfRange {
            index: pos,
            len: elems.len(),
            span: index.span,
        });
    }
    let elem_ty = elems[pos].clone().wrap_optional_depth(opt_depth);
    let span = base.span.merge(index.span);
    Ok(env.typed(
        TypedExprKind::TupleAccess {
            base: Box::new(base),
            index: pos,
            opt_depth,
        },
        elem_ty,
        span,
    ))
}

pub(crate) fn element_ty(ty: &QueryType) -> QueryType {
    match ty {
        QueryType::List(inner) | QueryType::Optional(inner) => inner.as_ref().clone(),
        _ => QueryType::Unknown,
    }
}
