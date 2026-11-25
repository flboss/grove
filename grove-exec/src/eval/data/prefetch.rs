use std::collections::HashSet;

use grove_query::methods;
use grove_query::{IdentBinding, TypedExpr, TypedExprKind};
use grove_schema::validated::{Field, FieldId, ValidatedSchema};

use super::owning::resolve_owning_values;
use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::method_meta;
use crate::value::Val;

pub(crate) fn owning_refs_in(
    expr: &TypedExpr,
    schema: &ValidatedSchema,
    skip: &HashSet<FieldId>,
) -> HashSet<FieldId> {
    let mut fields = HashSet::new();
    collect_owning_refs_in(expr, schema, skip, &mut fields);
    fields
}

/// Resolve the given owning Ref fields across values before per-element work
pub(crate) fn prefetch_owning(
    ctx: &mut EvalCtx,
    values: &mut [Val],
    fields: &HashSet<FieldId>,
) -> Result<(), ExecError> {
    if fields.is_empty() {
        return Ok(());
    }
    resolve_owning_values(ctx, values, Some(fields))
}

/// Collect owning Ref fields referenced by an expression
pub(crate) fn collect_owning_refs_in(
    expr: &TypedExpr,
    schema: &ValidatedSchema,
    skip: &HashSet<FieldId>,
    fields: &mut HashSet<FieldId>,
) {
    let mut frames = vec![true];
    collect_owning_refs_at(expr, schema, skip, &mut frames, fields);
}

fn collect_owning_refs_at(
    expr: &TypedExpr,
    schema: &ValidatedSchema,
    skip: &HashSet<FieldId>,
    // collectability flag per pushed element/projection scope, outermost first
    frames: &mut Vec<bool>,
    fields: &mut HashSet<FieldId>,
) {
    let current = frames.len() - 1;
    match &expr.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(field),
            depth,
            ..
        } => {
            if let Some(frame) = current.checked_sub(*depth)
                && frames[frame]
                && schema.field_is_ref_owning(*field) == Some(true)
                && !skip.contains(field)
            {
                fields.insert(*field);
            }
        }
        TypedExprKind::Field { base, name, .. } => {
            if let Some(frame) = rooted_frame(base, schema, frames)
                && frames[frame]
                && let Some(struct_id) = base.ty.schema_record()
                && let Some((field, _)) = schema.struct_field(struct_id, name)
                && schema.field_is_ref_owning(field) == Some(true)
                && !skip.contains(&field)
            {
                fields.insert(field);
            }
            collect_owning_refs_at(base, schema, skip, frames, fields);
        }
        TypedExprKind::Method {
            method, base, args, ..
        } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
            if methods::method(*method).is_scoped() {
                let nested = rooted_frame(base, schema, frames).is_some_and(|frame| frames[frame]);
                frames.push(nested);
                for arg in args {
                    collect_owning_refs_at(&arg.expr, schema, skip, frames, fields);
                }
                frames.pop();
            } else {
                for arg in args {
                    collect_owning_refs_at(&arg.expr, schema, skip, frames, fields);
                }
            }
        }
        TypedExprKind::TupleAccess { base, .. } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            collect_owning_refs_at(lhs, schema, skip, frames, fields);
            collect_owning_refs_at(rhs, schema, skip, frames, fields);
        }
        TypedExprKind::Unary { expr, .. }
        | TypedExprKind::Cast { expr, .. }
        | TypedExprKind::RowRef { expr }
        | TypedExprKind::Some { value: expr } => {
            collect_owning_refs_at(expr, schema, skip, frames, fields);
        }
        TypedExprKind::If { arms, default } => {
            for (condition, value) in arms {
                collect_owning_refs_at(condition, schema, skip, frames, fields);
                collect_owning_refs_at(value, schema, skip, frames, fields);
            }
            collect_owning_refs_at(default, schema, skip, frames, fields);
        }
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            for element in elements {
                collect_owning_refs_at(element, schema, skip, frames, fields);
            }
        }
        TypedExprKind::Struct { fields: values } => {
            for (_, value) in values {
                collect_owning_refs_at(value, schema, skip, frames, fields);
            }
        }
        TypedExprKind::Projection { base, items } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
            let nested = rooted_frame(base, schema, frames).is_some_and(|frame| frames[frame]);
            frames.push(nested);
            for item in items {
                collect_owning_refs_at(&item.value, schema, skip, frames, fields);
            }
            frames.pop();
        }
        TypedExprKind::Literal(_)
        | TypedExprKind::TypeConstant { .. }
        | TypedExprKind::EnumVariant { .. }
        | TypedExprKind::Ident { .. } => {}
    }
}

/// Frame from which `expr`'s value is reachable through `$`/owning Refs, if any
fn rooted_frame(expr: &TypedExpr, schema: &ValidatedSchema, frames: &[bool]) -> Option<usize> {
    let current = frames.len().checked_sub(1)?;
    match &expr.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::ListElement,
            depth,
            ..
        } => current.checked_sub(*depth),
        TypedExprKind::Ident {
            binding: IdentBinding::Field(field),
            depth,
            ..
        } => {
            let frame = current.checked_sub(*depth)?;
            (schema.field_is_ref_owning(*field) == Some(true)).then_some(frame)
        }
        TypedExprKind::Field { base, name, .. } => {
            let frame = rooted_frame(base, schema, frames)?;
            let (field, def) = base
                .ty
                .schema_record()
                .and_then(|struct_id| schema.struct_field(struct_id, name))?;
            (matches!(def, Field::Value { .. } | Field::Array { .. })
                || schema.field_is_ref_owning(field) == Some(true))
            .then_some(frame)
        }
        TypedExprKind::Method { method, base, .. }
            if method_meta::is_element_preserving(*method) =>
        {
            rooted_frame(base, schema, frames)
        }
        _ => None,
    }
}
