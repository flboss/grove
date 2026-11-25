use std::collections::{HashMap, HashSet};

use grove_schema::validated::{Field, FieldId, ValidatedSchema};
use grove_types::NodeId;

use super::fetch::{batch_fetch, lookup_ref, push_ref, resolve_field};
use super::field::resolve_base_link;
use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::value::{Key, Val};

fn is_owning_ref(schema: &ValidatedSchema, field: FieldId) -> Result<bool, ExecError> {
    schema
        .field_is_ref_owning(field)
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown field {field:?}"),
        })
}

/// Gather parent rowids from owning Refs anywhere in a value tree
fn collect_owning_refs(
    value: &Val,
    filter: Option<&HashSet<FieldId>>,
    by_field: &mut HashMap<FieldId, Vec<Key>>,
    order: &mut Vec<FieldId>,
    schema: &ValidatedSchema,
) -> Result<(), ExecError> {
    match value {
        Val::Ref { field, parent } => {
            if is_owning_ref(schema, *field)? && filter.is_none_or(|fields| fields.contains(field))
            {
                push_ref(by_field, order, *field, *parent);
            }
            Ok(())
        }
        Val::List(items) => {
            for item in items {
                collect_owning_refs(item, filter, by_field, order, schema)?;
            }
            Ok(())
        }
        Val::Tuple(items) => {
            for item in items {
                collect_owning_refs(item, filter, by_field, order, schema)?;
            }
            Ok(())
        }
        Val::Some(inner) => collect_owning_refs(inner, filter, by_field, order, schema),
        Val::Record(fields) => {
            for (_, field_value) in fields {
                collect_owning_refs(field_value, filter, by_field, order, schema)?;
            }
            Ok(())
        }
        Val::None | Val::Scalar(_) | Val::Enum(_, _) | Val::RowRef(_) => Ok(()),
    }
}

/// Replace owning Refs anywhere in a value tree with resolved data
fn splice_owning_refs(
    value: &mut Val,
    resolved: &HashMap<(FieldId, Key), Val>,
    filter: Option<&HashSet<FieldId>>,
    schema: &ValidatedSchema,
) -> Result<(), ExecError> {
    match value {
        Val::Ref { field, parent } => {
            if is_owning_ref(schema, *field)? && filter.is_none_or(|fields| fields.contains(field))
            {
                *value = lookup_ref(*field, *parent, resolved)?;
            }
            Ok(())
        }
        Val::List(items) => {
            for item in items {
                splice_owning_refs(item, resolved, filter, schema)?;
            }
            Ok(())
        }
        Val::Tuple(items) => {
            for item in items {
                splice_owning_refs(item, resolved, filter, schema)?;
            }
            Ok(())
        }
        Val::Some(inner) => splice_owning_refs(inner, resolved, filter, schema),
        Val::Record(fields) => {
            for (_, field_value) in fields {
                splice_owning_refs(field_value, resolved, filter, schema)?;
            }
            Ok(())
        }
        Val::None | Val::Scalar(_) | Val::Enum(_, _) | Val::RowRef(_) => Ok(()),
    }
}

/// Resolve owning Refs anywhere in the given values, batching per FieldId
pub(crate) fn resolve_owning_values(
    ctx: &mut EvalCtx,
    values: &mut [Val],
    filter: Option<&HashSet<FieldId>>,
) -> Result<(), ExecError> {
    loop {
        let mut by_field: HashMap<FieldId, Vec<Key>> = HashMap::new();
        let mut order: Vec<FieldId> = Vec::new();
        for value in values.iter() {
            collect_owning_refs(value, filter, &mut by_field, &mut order, ctx.schema)?;
        }
        if order.is_empty() {
            return Ok(());
        }
        let resolved = batch_fetch(ctx, &by_field, &order)?;
        for value in values.iter_mut() {
            splice_owning_refs(value, &resolved, filter, ctx.schema)?;
        }
    }
}

/// Resolve an owning Ref value
fn resolve_owning_ident(ctx: &mut EvalCtx, field: FieldId, value: Val) -> Result<Val, ExecError> {
    if matches!(value, Val::Ref { .. })
        && matches!(
            ctx.schema.field(field),
            Some(Field::Ref { owning: true, .. })
        )
    {
        resolve_base_link(ctx, value)
    } else {
        Ok(value)
    }
}

/// Resolve a Ref value, honoring a planned constrained fetch
pub(crate) fn resolve_ident_value(
    ctx: &mut EvalCtx,
    node: NodeId,
    field: FieldId,
    value: Val,
) -> Result<Val, ExecError> {
    let constraints = ctx
        .plan
        .constraints_for(node)
        .filter(|constraints| !constraints.is_empty());
    if let Some(constraints) = constraints {
        return match value {
            Val::Ref {
                field: linked_field,
                parent,
            } => {
                let mut resolved = resolve_field(ctx, linked_field, &[parent], constraints)?;
                ctx.applied_triggers.insert(node);
                match resolved.pop() {
                    Some((_, value)) => Ok(value),
                    None => Err(ExecError::Internal {
                        message: "constrained Ref resolved to no value".to_string(),
                    }),
                }
            }
            other => resolve_owning_ident(ctx, field, other),
        };
    }
    resolve_owning_ident(ctx, field, value)
}
