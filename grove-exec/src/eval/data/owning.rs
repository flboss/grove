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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Env;
    use crate::eval::data::tests::{record_field_value, run_seeded_recording};
    use crate::eval::harness::{FIXED_NOW, int, rec, some, str};
    use crate::pushdown::PushdownPlan;
    use crate::sqlite::SqliteBackend;
    use crate::storage::SortKey;
    use crate::testdata;
    use crate::value::Scalar;

    #[test]
    fn deep_resolver_value_shape_traversal() {
        let schema = testdata::schema();
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        testdata::seed(&mut conn).expect("testdata initialization");
        let backend = SqliteBackend::new(conn, &schema).expect("backend");
        let plan = PushdownPlan::disabled(&schema);
        let mut env = Env::new();
        let mut ctx = EvalCtx::new(&mut env, FIXED_NOW, &backend, &schema, &plan);

        let user = testdata::struct_id(&schema, "User");
        let order = testdata::struct_id(&schema, "Order");
        let profile_field = testdata::field_id(&schema, user, "profile");
        let orders_field = testdata::field_id(&schema, user, "orders");
        let order_user_field = testdata::field_id(&schema, order, "user");

        let mut value = Val::Tuple(vec![
            some(rec(vec![(
                "p",
                Val::Ref {
                    field: profile_field,
                    parent: Key(1),
                },
            )])),
            Val::list(vec![Val::Tuple(vec![Val::Ref {
                field: orders_field,
                parent: Key(1),
            }])]),
            rec(vec![(
                "n",
                Val::Ref {
                    field: order_user_field,
                    parent: Key(1),
                },
            )]),
        ]);

        resolve_owning_values(&mut ctx, std::slice::from_mut(&mut value), None)
            .expect("resolve owning refs");

        let Val::Tuple(parts) = &value else {
            panic!("expected tuple");
        };
        let Val::Some(slot) = &parts[0] else {
            panic!("expected some slot");
        };
        let profile_value = record_field_value(slot, "p");
        let Val::Some(profile) = profile_value else {
            panic!("expected some(profile), got {profile_value:?}");
        };
        assert_eq!(record_field_value(profile, "bio"), &str("AdaBio"));
        assert!(matches!(
            record_field_value(profile, "user"),
            Val::Ref { .. }
        ));

        let Val::List(wrapper) = &parts[1] else {
            panic!("expected list wrapper");
        };
        let Val::Tuple(inner) = &wrapper[0] else {
            panic!("expected tuple wrapper");
        };
        let Val::List(orders) = &inner[0] else {
            panic!("expected resolved orders, got {:?}", inner[0]);
        };
        assert_eq!(orders.len(), 2);
        assert_eq!(record_field_value(&orders[0], "amount"), &int(100));
        assert_eq!(record_field_value(&orders[1], "amount"), &int(200));
        assert!(matches!(
            record_field_value(&orders[0], "user"),
            Val::Ref { .. }
        ));
        assert!(matches!(
            record_field_value(&parts[2], "n"),
            Val::Ref { .. }
        ));
    }

    #[test]
    fn deep_resolver_field_filter() {
        let schema = testdata::schema();
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        testdata::seed(&mut conn).expect("testdata initialization");
        let backend = SqliteBackend::new(conn, &schema).expect("backend");
        let plan = PushdownPlan::disabled(&schema);
        let mut env = Env::new();
        let mut ctx = EvalCtx::new(&mut env, FIXED_NOW, &backend, &schema, &plan);

        let user = testdata::struct_id(&schema, "User");
        let profile_field = testdata::field_id(&schema, user, "profile");
        let orders_field = testdata::field_id(&schema, user, "orders");

        let mut value = rec(vec![
            (
                "p",
                Val::Ref {
                    field: profile_field,
                    parent: Key(1),
                },
            ),
            (
                "o",
                Val::Ref {
                    field: orders_field,
                    parent: Key(1),
                },
            ),
        ]);

        let only_orders: HashSet<FieldId> = [orders_field].into_iter().collect();
        resolve_owning_values(
            &mut ctx,
            std::slice::from_mut(&mut value),
            Some(&only_orders),
        )
        .expect("resolve filtered owning refs");
        assert!(matches!(record_field_value(&value, "p"), Val::Ref { .. }));
        assert!(matches!(
            record_field_value(&value, "o"),
            Val::List(rows) if rows.len() == 2
        ));

        resolve_owning_values(&mut ctx, std::slice::from_mut(&mut value), None)
            .expect("resolve remaining owning refs");
        assert!(matches!(record_field_value(&value, "p"), Val::Some(_)));
    }

    fn unpack_nested_ints(value: &Val, field: &str) -> Vec<i64> {
        let Val::List(rows) = value else {
            panic!("expected list, got {value:?}");
        };
        rows.iter()
            .map(|row| match record_field_value(row, field) {
                Val::Scalar(Scalar::Int(value)) => *value,
                other => panic!("expected int, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn owning_ident_chain_pushed_into_fetch_related() {
        let schema = testdata::schema();
        let (result, recording) = run_seeded_recording(
            "users[id <= 2] { o = orders[amount > 150].sort_desc_over(amount).take(1) { amount } }",
        )
        .expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 2);
        assert!(
            recording
                .related_parent_counts
                .borrow()
                .iter()
                .all(|count| *count == 1)
        );
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        for constraints in related.iter() {
            assert_eq!(constraints.filters.len(), 1);
            assert_eq!(
                constraints.sort,
                vec![SortKey {
                    field: amount,
                    desc: true,
                }]
            );
            assert_eq!(constraints.offset, 0);
            assert_eq!(constraints.limit, Some(1));
        }

        let Val::List(rows) = result else {
            panic!("expected user rows, got {result:?}");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            unpack_nested_ints(record_field_value(&rows[0], "o"), "amount"),
            [200]
        );
        assert_eq!(
            unpack_nested_ints(record_field_value(&rows[1], "o"), "amount"),
            [400]
        );
    }

    #[test]
    fn mixed_constrained_and_plain_refs() {
        let (result, recording) = run_seeded_recording(
            "users[id <= 1] { a = orders.take(1) { amount }, b = orders.len() }",
        )
        .expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 2);
        assert_eq!(related[0].limit, Some(1));
        assert!(related[1].is_empty());

        let Val::List(rows) = result else {
            panic!("expected user rows, got {result:?}");
        };
        let first = &rows[0];
        assert_eq!(
            unpack_nested_ints(record_field_value(first, "a"), "amount"),
            [100]
        );
        assert_eq!(record_field_value(first, "b"), &int(2));
    }
}
