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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::data::tests::run_seeded_recording;
    use crate::eval::harness::{run_seeded, some, str};
    use crate::sqlite::SqliteBackend;
    use crate::storage::Storage;
    use crate::testdata;
    use crate::value::Scalar;

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
    fn field_optional_ref_resolved() {
        match run_seeded("users.manager") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let names: Vec<Option<&str>> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Some(user) => match user.as_ref() {
                            Val::Record(fields) => Some(
                                fields
                                    .iter()
                                    .find(|(name, _)| name.as_ref() == "name")
                                    .map(|(_, value)| match value {
                                        Val::Scalar(Scalar::String(s)) => s.as_str(),
                                        other => panic!("expected name string, got {other}"),
                                    })
                                    .expect("name field"),
                            ),
                            other => panic!("expected manager record, got {other}"),
                        },
                        Val::None => None,
                        other => panic!("expected optional manager, got {other}"),
                    })
                    .collect();
                assert_eq!(names, [Some("Bob"), None, Some("Bob"), Some("Ada"), None]);
            }
            got => panic!("expected manager list, got {got:?}"),
        }
    }

    #[test]
    fn field_list_ref_resolved() {
        match run_seeded("users.roles") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let role_names: Vec<Vec<&str>> = rows
                    .iter()
                    .map(|row| match row {
                        Val::List(roles) => roles
                            .iter()
                            .map(|role| match role {
                                Val::Record(fields) => fields
                                    .iter()
                                    .find(|(name, _)| name.as_ref() == "name")
                                    .map(|(_, value)| match value {
                                        Val::Scalar(Scalar::String(s)) => s.as_str(),
                                        other => panic!("expected name string, got {other}"),
                                    })
                                    .expect("name field"),
                                other => panic!("expected role record, got {other}"),
                            })
                            .collect(),
                        other => panic!("expected role list, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    role_names,
                    [
                        vec!["admin", "editor", "editor"],
                        vec![],
                        vec!["editor"],
                        vec![],
                        vec![],
                    ]
                );
            }
            got => panic!("expected roles list, got {got:?}"),
        }
    }

    #[test]
    fn dangling_fk_required_ref_error() {
        let schema = testdata::schema();
        let user_struct = testdata::struct_id(&schema, "User");
        let order_struct = testdata::struct_id(&schema, "Order");
        let user_field = testdata::field_id(&schema, order_struct, "user");
        let orders_field = testdata::field_id(&schema, user_struct, "orders");
        let relation = schema.relation_of_field(user_field).expect("user relation");
        assert_eq!(
            schema.relations[relation.index()].endpoints(),
            [user_field, orders_field],
        );
        let mut conn =
            rusqlite::Connection::open_in_memory().expect("in-memory database should open");
        testdata::seed(&mut conn).expect("testdata initialization should succeed");
        let backend = SqliteBackend::new(conn, &schema).expect("backend should verify");
        // 100/Ada, 200/Ada, 300/NULL, 400/Bob
        let rows = backend
            .fetch(
                order_struct,
                &[testdata::field_id(&schema, order_struct, "amount")],
                &FetchConstraints::EMPTY,
            )
            .expect("fetch orders");
        assert_eq!(rows.len(), 4);
        let keys: Vec<Key> = rows.iter().map(|row| row.rowid).collect();
        let groups = Storage::fetch_related(
            &backend,
            relation,
            true,
            &keys,
            &testdata_value_fields(&schema, user_struct),
            &FetchConstraints::EMPTY,
        )
        .expect("fetch_related succeeds");
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[3].len(), 1);
        let empty: Val = Val::list(Vec::new());
        match wrap_fetched(empty, user_field, Key(2), &schema) {
            Err(ExecError::Integrity { message }) => assert!(
                message.contains("resolved to 0 rows"),
                "expected dangling-FK message, got: {message}"
            ),
            got => panic!("expected integrity error, got {got:?}"),
        }
    }

    fn testdata_value_fields(schema: &ValidatedSchema, struct_id: StructId) -> Vec<FieldId> {
        schema.structs[struct_id.index()]
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
            .map(|(local, _)| FieldId::new(struct_id, local))
            .collect()
    }

    #[test]
    fn field_pushdown_projection_fields_pruned() {
        let schema = testdata::schema();
        let (_, recording) = run_seeded_recording("users { name }").expect("query");
        let user = testdata::struct_id(&schema, "User");
        let name = testdata::field_id(&schema, user, "name");
        assert_eq!(*recording.fetches.borrow(), vec![(user, vec![name])]);
        let related = recording.related_fetches.borrow();
        assert!(related.is_empty(), "ref not lazily resolved: {related:?}");
    }

    #[test]
    fn field_pushdown_whole_record_fields_kept() {
        let schema = testdata::schema();
        let (_, recording) = run_seeded_recording("users").expect("query");
        let user = testdata::struct_id(&schema, "User");
        assert_eq!(
            *recording.fetches.borrow(),
            vec![(user, testdata_value_fields(&schema, user))]
        );
        let related = recording.related_fetches.borrow();
        assert_eq!(
            related.len(),
            3,
            "expected profile, orders, items: {related:?}"
        );
        assert_eq!(
            recording.related_parent_counts.borrow().as_slice(),
            &[5, 5, 3]
        );
        for (rel, from_first, fields) in related.iter() {
            let relation = &schema.relations[rel.index()];
            let endpoint = if *from_first {
                relation.endpoints()[0]
            } else {
                relation.endpoints()[1]
            };
            let target =
                match &schema.structs[endpoint.struct_id().index()].fields[endpoint.local()] {
                    Field::Ref { target, .. } => *target,
                    _ => panic!("relation endpoint is not a ref"),
                };
            assert_eq!(fields, &testdata_value_fields(&schema, target));
        }
    }

    #[test]
    fn root_fetch_constraints_pushdown() {
        let schema = testdata::schema();
        let (_, recording) =
            run_seeded_recording("users[id > 2].sort_over(name).take(2)").expect("query");
        let constraints = recording.constraints.borrow();
        let first = constraints.first().expect("root fetch");
        assert_eq!(first.filters.len(), 1);
        assert_eq!(first.sort.len(), 1);
        assert_eq!(
            first.sort[0].field,
            testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "name")
        );
        assert!(!first.sort[0].desc);
        assert_eq!(first.offset, 0);
        assert_eq!(first.limit, Some(2));
    }
}
