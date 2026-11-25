use std::collections::HashMap;

use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};
use grove_types::NodeId;

use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::storage::{FetchConstraints, Row};
use crate::value::{CollectionOrigin, Key, RowOrigin, Val};

pub(crate) fn exec_root(
    ctx: &mut EvalCtx,
    root_idx: usize,
    node: NodeId,
) -> Result<Val, ExecError> {
    let root = ctx
        .schema
        .roots
        .get(root_idx)
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown root {root_idx}"),
        })?;
    let fields = ctx.plan.fields_for(root.struct_id).to_vec();
    let rows = if let Some(constraints) = ctx.plan.constraints_for(node) {
        ctx.storage.fetch(root.struct_id, &fields, constraints)?
    } else {
        ctx.storage
            .fetch(root.struct_id, &fields, &FetchConstraints::EMPTY)?
    };
    ctx.applied_triggers.insert(node);
    let mut value = materialize(ctx, root.struct_id, &fields, rows)?;
    if let Val::List(list) = &mut value {
        list.origin = Some(CollectionOrigin::Root {
            struct_id: root.struct_id,
            root_idx,
        });
    }
    Ok(value)
}

/// Fetch a Ref field for a batch of parents
pub(crate) fn resolve_field(
    ctx: &mut EvalCtx,
    field: FieldId,
    parents: &[Key],
    constraints: &FetchConstraints,
) -> Result<Vec<ResolvedLink>, ExecError> {
    let (target, field_name) =
        match &ctx.schema.structs[field.struct_id().index()].fields[field.local()] {
            Field::Ref { target, name, .. } => (*target, name),
            Field::Value { name, .. } | Field::Array { name, .. } => {
                return Err(ExecError::Internal {
                    message: format!("Ref link names a non-Ref field `{name}`"),
                });
            }
        };
    let relation = ctx
        .schema
        .relation_of_field(field)
        .ok_or_else(|| ExecError::Internal {
            message: format!("Ref field `{field_name}` has no relation"),
        })?;
    let fetch_from_first = ctx.schema.relations[relation.index()].endpoints()[0] == field;
    let fields = ctx.plan.fields_for(target).to_vec();
    let groups =
        ctx.storage
            .fetch_related(relation, fetch_from_first, parents, &fields, constraints)?;
    let mut resolved_pairs = Vec::with_capacity(parents.len());
    for (parent, group) in parents.iter().zip(groups) {
        // Recursion terminates: ownership chain ends at a root
        let mut value = materialize(ctx, target, &fields, group)?;
        if let Val::List(list) = &mut value {
            list.origin = Some(CollectionOrigin::Ref {
                field,
                parent: RowOrigin {
                    struct_id: field.struct_id(),
                    key: *parent,
                },
            });
        }
        let wrapped = wrap_fetched(value, field, *parent, ctx.schema)?;
        resolved_pairs.push(((field, *parent), wrapped));
    }
    Ok(resolved_pairs)
}

/// Wrap fetch result according to the Ref field's shape
pub(crate) fn wrap_fetched(
    value: Val,
    field: FieldId,
    parent: Key,
    schema: &ValidatedSchema,
) -> Result<Val, ExecError> {
    let (optional, is_list, field_name) =
        match &schema.structs[field.struct_id().index()].fields[field.local()] {
            Field::Ref {
                optional,
                is_list,
                name,
                ..
            } => (*optional, *is_list, name),
            _ => {
                return Err(ExecError::Internal {
                    message: "Ref link names a non-Ref field".to_string(),
                });
            }
        };
    if is_list {
        return Ok(value);
    }
    // ?List<...> refs are not possible
    if optional {
        return match value {
            Val::List(mut items) => match items.len() {
                0 => Ok(Val::None),
                1 => Ok(Val::Some(Box::new(items.items.pop().unwrap()))),
                n => Err(ExecError::Integrity {
                    message: format!(
                        "optional Ref `{field_name}` resolved to {n} rows (for parent key {})",
                        parent.0
                    ),
                }),
            },
            other => Err(ExecError::Internal {
                message: format!("optional Ref resolved to {other}"),
            }),
        };
    }
    match value {
        Val::List(mut items) => match items.len() {
            1 => Ok(items.items.pop().unwrap()),
            n => Err(ExecError::Integrity {
                message: format!(
                    "required Ref `{field_name}` resolved to {n} rows (for parent key {})",
                    parent.0
                ),
            }),
        },
        other => Err(ExecError::Internal {
            message: format!("required Ref resolved to {other}"),
        }),
    }
}

/// Execute batched `fetch_related` per distinct field
pub(crate) fn batch_fetch(
    ctx: &mut EvalCtx,
    by_field: &HashMap<FieldId, Vec<Key>>,
    order: &[FieldId],
) -> Result<HashMap<(FieldId, Key), Val>, ExecError> {
    let mut resolved = HashMap::with_capacity(by_field.values().map(Vec::len).sum());
    for field in order {
        resolved.extend(resolve_field(
            ctx,
            *field,
            &by_field[field],
            &FetchConstraints::EMPTY,
        )?);
    }
    Ok(resolved)
}

/// Build Records from storage rows, resolving owning Refs
fn materialize(
    ctx: &mut EvalCtx,
    struct_id: StructId,
    fields: &[FieldId],
    rows: Vec<Row>,
) -> Result<Val, ExecError> {
    let struct_ = &ctx.schema.structs[struct_id.index()];
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = row.values.into_iter();
        let mut record = Vec::with_capacity(struct_.fields.len());
        let mut next_field = 0;
        for (local, field) in struct_.fields.iter().enumerate() {
            let name = ctx.interner.intern(field.name());
            match field {
                Field::Ref { .. } => record.push((
                    name,
                    Val::Ref {
                        field: FieldId::new(struct_id, local),
                        parent: row.rowid,
                    },
                )),
                _ => {
                    if next_field < fields.len() && fields[next_field].local() == local {
                        record.push((
                            name,
                            values.next().ok_or_else(|| ExecError::Internal {
                                message: "row has fewer values than fields".to_string(),
                            })?,
                        ));
                        next_field += 1;
                    }
                }
            }
        }
        if next_field != fields.len() {
            return Err(ExecError::Internal {
                message: "row is missing requested fields".to_string(),
            });
        }
        if values.next().is_some() {
            return Err(ExecError::Internal {
                message: "row has more values than fields".to_string(),
            });
        }
        items.push(Val::record_at(
            record,
            RowOrigin {
                struct_id,
                key: row.rowid,
            },
        ));
    }
    Ok(Val::list(items))
}

type ResolvedLink = ((FieldId, Key), Val);

pub(crate) fn push_ref(
    by_field: &mut HashMap<FieldId, Vec<Key>>,
    order: &mut Vec<FieldId>,
    field: FieldId,
    parent: Key,
) {
    by_field
        .entry(field)
        .or_insert_with(|| {
            order.push(field);
            Vec::new()
        })
        .push(parent);
}

/// Look up one resolved link (absence invalid)
pub(crate) fn lookup_ref(
    field: FieldId,
    parent: Key,
    resolved: &HashMap<(FieldId, Key), Val>,
) -> Result<Val, ExecError> {
    resolved
        .get(&(field, parent))
        .cloned()
        .ok_or_else(|| ExecError::Internal {
            message: "Ref data missing from batch fetch result".to_string(),
        })
}
