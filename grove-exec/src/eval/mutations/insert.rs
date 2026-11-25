use grove_query::{CollectionOriginKind, TypedExpr};
use grove_schema::validated::{Field, FieldId, RelationId, StructId, ValidatedSchema};

use super::links::link_targets;
use super::records::{collect_insert_records, record_from_value, record_parts};
use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::{LinkMode, WriteTxn};
use crate::value::{CollectionOrigin, Key, RecordVal, RowOrigin, Val, unwrap_optional_depth};

pub(super) fn exec_insert(
    base: &TypedExpr,
    arg: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(target) = resolve_insert_target(base, opt_depth, ctx)? else {
        return Ok(());
    };
    match target {
        InsertTarget::Link {
            struct_id: _,
            field,
            owner,
        } => {
            let arg = eval(arg, ctx)?;
            let targets = link_targets(ctx.schema, field, arg)?;
            write.link_row_refs(field, owner.key, &targets, LinkMode::Append)
        }
        target => {
            let arg = eval(arg, ctx)?;
            let records = collect_insert_records(arg)?;
            insert_group(ctx.schema, target, records, write)
        }
    }
}

enum InsertTarget {
    Root {
        struct_id: StructId,
    },
    Ref {
        struct_id: StructId,
        relation: RelationId,
        parent: Key,
        parent_is_first: bool,
    },
    Link {
        struct_id: StructId,
        field: FieldId,
        owner: RowOrigin,
    },
}

impl InsertTarget {
    fn struct_id(&self) -> StructId {
        match self {
            InsertTarget::Root { struct_id }
            | InsertTarget::Ref { struct_id, .. }
            | InsertTarget::Link { struct_id, .. } => *struct_id,
        }
    }
}

/// Resolve an insert target to a root or parented collection
fn resolve_insert_target(
    base: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
) -> Result<Option<InsertTarget>, ExecError> {
    let Some(origin) = base.provenance.collection else {
        return Err(ExecError::Unsupported {
            message: "insert target has no collection origin".to_string(),
        });
    };
    match origin {
        CollectionOriginKind::Root(root_idx) => {
            let root = ctx
                .schema
                .roots
                .get(root_idx)
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown root {root_idx}"),
                })?;
            if opt_depth > 0 {
                let target = eval(base, ctx)?;
                let (_, peeled) = unwrap_optional_depth(target, opt_depth)?;
                if peeled < opt_depth {
                    return Ok(None);
                }
            }
            Ok(Some(InsertTarget::Root {
                struct_id: root.struct_id,
            }))
        }
        CollectionOriginKind::Ref(field_id) => {
            let target = eval(base, ctx)?;
            let target = if opt_depth > 0 {
                let (target, peeled) = unwrap_optional_depth(target, opt_depth)?;
                if peeled < opt_depth {
                    return Ok(None);
                }
                target
            } else {
                target
            };
            let Val::List(list) = target else {
                return Err(ExecError::Internal {
                    message: "nested insert target did not evaluate to a list".to_string(),
                });
            };
            let Some(CollectionOrigin::Ref { field, parent }) = list.origin else {
                return Err(ExecError::Internal {
                    message: "nested insert target has no ref collection origin".to_string(),
                });
            };
            if field != field_id {
                return Err(ExecError::Internal {
                    message: "nested insert origin field mismatch".to_string(),
                });
            }
            let Some(Field::Ref {
                target: struct_id,
                is_list: true,
                owning,
                ..
            }) = ctx.schema.field(field)
            else {
                return Err(ExecError::Internal {
                    message: "insert through unsupported reference".to_string(),
                });
            };
            if !*owning {
                let relation =
                    ctx.schema
                        .relation_of_field(field)
                        .ok_or_else(|| ExecError::Internal {
                            message: "insert target Ref has no relation".to_string(),
                        })?;
                if ctx
                    .schema
                    .relation_has_owning_endpoint(&ctx.schema.relations[relation.index()])
                {
                    return Err(ExecError::Internal {
                        message: "cannot insert through owning-relation backref".to_string(),
                    });
                }
                return Ok(Some(InsertTarget::Link {
                    struct_id: *struct_id,
                    field,
                    owner: parent,
                }));
            }
            let relation =
                ctx.schema
                    .relation_of_field(field)
                    .ok_or_else(|| ExecError::Internal {
                        message: "insert target Ref has no relation".to_string(),
                    })?;
            let parent_is_first = ctx.schema.relations[relation.index()].endpoints()[0] == field;
            Ok(Some(InsertTarget::Ref {
                struct_id: *struct_id,
                relation,
                parent: parent.key,
                parent_is_first,
            }))
        }
    }
}

/// Insert base records, then recursively insert owned child refs
fn insert_group(
    schema: &ValidatedSchema,
    target: InsertTarget,
    records: Vec<RecordVal>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    if records.is_empty() {
        return Ok(());
    }
    let struct_id = target.struct_id();
    let mut values = Vec::with_capacity(records.len());
    let mut owned = Vec::with_capacity(records.len());
    let mut links = Vec::with_capacity(records.len());
    for record in records {
        let parts = record_parts(schema, struct_id, record)?;
        values.push(parts.base);
        owned.push(parts.owned);
        links.push(parts.links);
    }

    let keys = match &target {
        InsertTarget::Root { .. } => write.insert_rows(struct_id, &values)?,
        InsertTarget::Ref {
            relation,
            parent,
            parent_is_first,
            ..
        } => write.insert_rows_with_parent(
            struct_id,
            &values,
            *relation,
            *parent,
            *parent_is_first,
        )?,
        InsertTarget::Link { .. } => {
            return Err(ExecError::Internal {
                message: "link target reached record insertion".to_string(),
            });
        }
    };
    if keys.len() != values.len() {
        return Err(ExecError::Internal {
            message: "insert returned an unexpected number of row keys".to_string(),
        });
    }

    for ((children, links), key) in owned.into_iter().zip(links).zip(keys) {
        for (field_id, value) in children {
            insert_owned_field(schema, field_id, value, key, write)?;
        }
        for (field_id, targets) in links {
            write.link_row_refs(field_id, key, &targets, LinkMode::Append)?;
        }
    }
    Ok(())
}

pub(super) fn insert_owned_field(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
    parent: Key,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(Field::Ref {
        target,
        optional,
        is_list,
        owning,
        ..
    }) = schema.field(field_id)
    else {
        return Err(ExecError::Internal {
            message: "owned insert field is not a ref".to_string(),
        });
    };
    if !*owning {
        return Err(ExecError::Internal {
            message: "attempted recursive insert through non-owning ref".to_string(),
        });
    }
    let records = if *is_list {
        let Val::List(list) = value else {
            return Err(ExecError::Internal {
                message: "list ref field did not contain a list".to_string(),
            });
        };
        list.into_iter()
            .map(record_from_value)
            .collect::<Result<Vec<_>, _>>()?
    } else if *optional {
        match value {
            Val::None => Vec::new(),
            Val::Some(value) => vec![record_from_value(*value)?],
            other => {
                return Err(ExecError::Internal {
                    message: format!("optional ref field contained {other}"),
                });
            }
        }
    } else {
        vec![record_from_value(value)?]
    };
    if records.is_empty() {
        return Ok(());
    }
    let relation = schema
        .relation_of_field(field_id)
        .ok_or_else(|| ExecError::Internal {
            message: "owned insert field has no relation".to_string(),
        })?;
    let parent_is_first = schema.relations[relation.index()].endpoints()[0] == field_id;
    insert_group(
        schema,
        InsertTarget::Ref {
            struct_id: *target,
            relation,
            parent,
            parent_is_first,
        },
        records,
        write,
    )
}
