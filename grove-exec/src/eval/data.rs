use std::collections::HashMap;
use std::sync::Arc;

use grove_query::typecheck::types::TypedExpr;
use grove_schema::validated::{Field, FieldId, StructId};

use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::{Key, Row};
use crate::value::Val;

/// Read root table into a List<Record> (owning Refs resolved)
pub(crate) fn exec_root(ctx: &mut EvalCtx, root_idx: usize) -> Result<Val, ExecError> {
    let root = ctx
        .schema
        .roots
        .get(root_idx)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown root {root_idx} (internal error)"),
        })?;
    let fields = value_and_array_fields(ctx, root.struct_id);
    let rows = ctx.storage.fetch(root.struct_id, &fields)?;
    materialize(ctx, root.struct_id, rows)
}

/// FieldIds in declaration order, excluding Refs
fn value_and_array_fields(ctx: &EvalCtx, struct_id: StructId) -> Vec<FieldId> {
    ctx.schema.structs[struct_id.index()]
        .fields
        .iter()
        .enumerate()
        .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
        .map(|(local, _)| FieldId::new(struct_id, local))
        .collect()
}

/// Build Records from storage rows, resolving owning Refs
pub(crate) fn materialize(
    ctx: &mut EvalCtx,
    struct_id: StructId,
    rows: Vec<Row>,
) -> Result<Val, ExecError> {
    let struct_ = &ctx.schema.structs[struct_id.index()];
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = row.values.into_iter();
        let mut record = Vec::with_capacity(struct_.fields.len());
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
                _ => record.push((
                    name,
                    values.next().ok_or_else(|| ExecError::Integrity {
                        message: "row has fewer values than fields (internal error)".to_string(),
                    })?,
                )),
            }
        }
        if values.next().is_some() {
            return Err(ExecError::Integrity {
                message: "row has more values than fields (internal error)".to_string(),
            });
        }
        items.push(Val::Record(record));
    }
    resolve_owning(ctx, items)
}

type ResolvedLink = ((FieldId, Key), Val);

/// Recursively resolve owning Refs
fn resolve_owning(ctx: &mut EvalCtx, items: Vec<Val>) -> Result<Val, ExecError> {
    let mut by_field: HashMap<FieldId, Vec<Key>> = HashMap::new();
    let mut order: Vec<FieldId> = Vec::new();
    collect_owning(&items, &mut by_field, &mut order, ctx.schema)?;
    // batched fetch per distinct field
    let mut resolved: HashMap<(FieldId, Key), Val> =
        HashMap::with_capacity(order.len() + by_field.values().map(Vec::len).sum::<usize>());
    for field in order {
        let parents = &by_field[&field];
        resolved.extend(resolve_field(ctx, field, parents)?);
    }
    let spliced = items
        .into_iter()
        .map(|item| splice_owning(item, &resolved))
        .collect::<Result<_, _>>()?;
    Ok(Val::List(spliced))
}

/// Gather parent rowids from top-level owning Ref fields
fn collect_owning(
    items: &[Val],
    by_field: &mut HashMap<FieldId, Vec<Key>>,
    order: &mut Vec<FieldId>,
    schema: &grove_schema::validated::ValidatedSchema,
) -> Result<(), ExecError> {
    for item in items {
        let Val::Record(record) = item else {
            return Err(ExecError::Integrity {
                message: "materialized value is not a record (internal error)".to_string(),
            });
        };
        for value in record.iter().map(|(_, v)| v) {
            let Val::Ref { field, parent } = value else {
                continue;
            };
            let owning = match &schema.structs[field.struct_id().index()].fields[field.local()] {
                Field::Ref { owning, .. } => *owning,
                _ => {
                    return Err(ExecError::Integrity {
                        message: "Ref link names a non-Ref field (internal error)".to_string(),
                    });
                }
            };
            if owning {
                by_field
                    .entry(*field)
                    .or_insert_with(|| {
                        order.push(*field);
                        Vec::new()
                    })
                    .push(*parent);
            }
        }
    }
    Ok(())
}

/// Fetch an owning Ref field for a batch of parents
fn resolve_field(
    ctx: &mut EvalCtx,
    field: FieldId,
    parents: &[Key],
) -> Result<Vec<ResolvedLink>, ExecError> {
    let (target, optional, is_list, field_name) =
        match &ctx.schema.structs[field.struct_id().index()].fields[field.local()] {
            Field::Ref {
                target,
                optional,
                is_list,
                name,
                ..
            } => (*target, *optional, *is_list, name),
            _ => {
                return Err(ExecError::Integrity {
                    message: "Ref link names a non-Ref field (internal error)".to_string(),
                });
            }
        };
    let relation = ctx
        .schema
        .relation_of_field(field)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("Ref field `{field_name}` has no relation (internal error)"),
        })?;
    let fetch_from_first = ctx.schema.relations[relation.index()].endpoints()[0] == field;
    let fields = value_and_array_fields(ctx, target);
    let groups = ctx
        .storage
        .fetch_related(relation, fetch_from_first, parents, &fields)?;
    let mut resolved_pairs = Vec::with_capacity(parents.len());
    for (parent, group) in parents.iter().zip(groups) {
        // Recursion terminates: ownership chain ends at a root
        let value = materialize(ctx, target, group)?;
        let wrapped = if is_list {
            value
        // ?List<...> refs don't exist in schema
        } else if optional {
            match value {
                Val::List(mut items) => match items.len() {
                    0 => Val::None,
                    1 => Val::Some(Box::new(items.pop().unwrap())),
                    n => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "optional Ref `{field_name}` resolved to {n} rows (for parent key {})",
                                parent.0
                            ),
                        });
                    }
                },
                other => {
                    return Err(ExecError::Integrity {
                        message: format!("optional Ref resolved to {other} (internal error)"),
                    });
                }
            }
        } else {
            match value {
                Val::List(mut items) => match items.len() {
                    1 => items.pop().unwrap(),
                    n => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "required Ref `{field_name} resolved to {n} rows (for parent key {})",
                                parent.0
                            ),
                        });
                    }
                },
                other => {
                    return Err(ExecError::Integrity {
                        message: format!("required Ref resolved to {other} (internal error)"),
                    });
                }
            }
        };
        resolved_pairs.push(((field, *parent), wrapped));
    }
    Ok(resolved_pairs)
}

/// Recursively replace owning links with resolved data
fn splice_owning(value: Val, resolved: &HashMap<(FieldId, Key), Val>) -> Result<Val, ExecError> {
    match value {
        Val::Record(record) => {
            let spliced = record
                .into_iter()
                .map(|(name, field_value)| {
                    splice_owning(field_value, resolved).map(|val| (name, val))
                })
                .collect::<Result<_, _>>()?;
            Ok(Val::Record(spliced))
        }
        Val::List(items) => {
            let spliced = items
                .into_iter()
                .map(|item| splice_owning(item, resolved))
                .collect::<Result<_, _>>()?;
            Ok(Val::List(spliced))
        }
        Val::Ref { field, parent } => Ok(resolved
            .get(&(field, parent))
            .cloned()
            .unwrap_or(Val::Ref { field, parent })),
        other => Ok(other),
    }
}

/// Perform a record field access
pub(crate) fn exec_field(
    base: &TypedExpr,
    name: &str,
    optional: bool,
    ctx: &mut EvalCtx,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    match base {
        Val::Record(fields) => record_field(fields, name),
        Val::List(items) => {
            let mut mapped = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Val::Record(fields) => mapped.push(record_field(fields, name)?),
                    Val::None => mapped.push(Val::None),
                    other => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "field `{name}` on non-record list element: {other} (internal error)"
                            ),
                        });
                    }
                }
            }
            Ok(Val::List(mapped))
        }
        _ => Err(ExecError::Integrity {
            message: format!("field `{name}` on non-record value (internal error)"),
        }),
    }
}

fn record_field(
    fields: impl IntoIterator<Item = (Arc<str>, Val)>,
    name: &str,
) -> Result<Val, ExecError> {
    fields
        .into_iter()
        .find(|(field_name, _)| field_name.as_ref() == name)
        .map(|(_, value)| value)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown field `{name}` (internal error)"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{eval::harness::*, testdata, value::Scalar};

    #[test]
    fn root_owning_resolved() {
        match run_seeded("users") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                // Ada (has profile, 2 orders)
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                let (_, profile) = first
                    .iter()
                    .find(|(name, _)| name.as_ref() == "profile")
                    .expect("profile field");
                match profile {
                    Val::Some(inner) => {
                        let Val::Record(profile) = inner.as_ref() else {
                            panic!("expected profile record, got {inner}");
                        };
                        assert!(profile.iter().any(|(name, value)| {
                            name.as_ref() == "bio" && *value == str("AdaBio")
                        }));
                        assert!(profile.iter().any(|(name, value)| {
                            matches!(value, Val::Ref { .. }) && name.as_ref() == "user"
                        }));
                    }
                    got => panic!("expected some(profile), got {got}"),
                }
                let (_, orders) = first
                    .iter()
                    .find(|(name, _)| name.as_ref() == "orders")
                    .expect("orders field");
                let Val::List(orders) = orders else {
                    panic!("expected orders list, got {orders}");
                };
                assert_eq!(orders.len(), 2);
                for name in ["roles", "manager"] {
                    assert!(
                        first.iter().any(|(n, value)| {
                            n.as_ref() == name && matches!(value, Val::Ref { .. })
                        }),
                        "expected unresolved {name} link"
                    );
                }
                assert!(first.iter().any(|(name, value)| {
                    name.as_ref() == "nickname" && *value == some(str("A"))
                }));
                // Bob (no profile, 1 order)
                let Val::Record(second) = &rows[1] else {
                    panic!("expected record");
                };
                assert!(
                    second
                        .iter()
                        .any(|(name, value)| { name.as_ref() == "profile" && *value == Val::None })
                );
                let (_, orders) = second
                    .iter()
                    .find(|(name, _)| name.as_ref() == "orders")
                    .expect("orders field");
                assert!(matches!(orders, Val::List(items) if items.len() == 1));
                // Eli (no orders)
                let Val::Record(fifth) = &rows[4] else {
                    panic!("expected record");
                };
                let (_, orders) = fifth
                    .iter()
                    .find(|(name, _)| name.as_ref() == "orders")
                    .expect("orders field");
                assert!(matches!(orders, Val::List(items) if items.is_empty()));
            }
            got => panic!("expected user rows, got {got:?}"),
        }
    }

    #[test]
    fn unresolved_ref_display() {
        match run_seeded("users") {
            Ok(Val::List(rows)) => {
                let text = rows[0].to_string();
                assert_eq!(
                    text.split("ref(").count(),
                    7,
                    "expected link rendering: {text}"
                );
            }
            got => panic!("expected user rows, got {got:?}"),
        }
    }

    #[test]
    fn ref_links_comparison_unknown() {
        let user_struct = testdata::struct_id(&testdata::schema(), "User");
        let manager_field = testdata::field_id(&testdata::schema(), user_struct, "manager");
        let link = Val::Ref {
            field: manager_field,
            parent: Key(1),
        };
        assert_eq!(
            link.to_string(),
            format!("ref({}:{})", user_struct.index(), manager_field.local())
        );
        assert_eq!(
            link.eq(&Val::Scalar(Scalar::Int(1))),
            None,
            "Ref equality is unknown, not false"
        );
        assert_eq!(link.eq(&link.clone()), None);
    }

    #[test]
    fn root_let_binding_non_owning_unresolved() {
        match run_seeded("let r = roles; r") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 2);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                assert_eq!(first.len(), 2);
                assert!(
                    first
                        .iter()
                        .any(|(name, value)| { name.as_ref() == "name" && *value == str("admin") })
                );
                assert!(first.iter().any(|(name, value)| {
                    name.as_ref() == "users" && matches!(value, Val::Ref { .. })
                }));
                let Val::Record(second) = &rows[1] else {
                    panic!("expected record");
                };
                assert!(
                    second.iter().any(|(name, value)| {
                        name.as_ref() == "name" && *value == str("editor")
                    })
                );
                assert!(second.iter().any(|(name, value)| {
                    name.as_ref() == "users" && matches!(value, Val::Ref { .. })
                }));
            }
            got => panic!("expected role rows, got {got:?}"),
        }
    }
}
