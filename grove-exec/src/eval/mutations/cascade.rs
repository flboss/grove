use std::collections::HashSet;

use grove_schema::validated::{Field, FieldId, Relation, RelationId, StructId, ValidatedSchema};

use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::storage::{FetchConstraints, WriteTxn};
use crate::value::{Key, RowOrigin};

fn fetch_owned_keys(
    ctx: &mut EvalCtx<'_>,
    field_id: FieldId,
    target: StructId,
    parents: &[Key],
    seen: &mut HashSet<RowOrigin>,
) -> Result<Vec<Key>, ExecError> {
    let relation = ctx
        .schema
        .relation_of_field(field_id)
        .ok_or_else(|| ExecError::Internal {
            message: "owning ref field has no relation".to_string(),
        })?;
    let fetch_from_first = ctx.schema.relations[relation.index()].endpoints()[0] == field_id;
    let groups = ctx.storage.fetch_related(
        relation,
        fetch_from_first,
        parents,
        &[],
        &FetchConstraints::EMPTY,
    )?;
    let mut keys = Vec::new();
    for group in groups {
        for row in group {
            let origin = RowOrigin {
                struct_id: target,
                key: row.rowid,
            };
            if seen.insert(origin) {
                keys.push(row.rowid);
            }
        }
    }
    Ok(keys)
}

pub(super) fn delete_owned_children(
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
    field_id: FieldId,
    parent: Key,
    seen: &mut HashSet<RowOrigin>,
) -> Result<(), ExecError> {
    let Some(Field::Ref {
        target,
        owning: true,
        ..
    }) = ctx.schema.field(field_id)
    else {
        return Err(ExecError::Internal {
            message: "update ref field is not an owning ref".to_string(),
        });
    };
    let keys = fetch_owned_keys(ctx, field_id, *target, &[parent], seen)?;
    delete_owned_cascade(ctx, write, *target, keys, seen)
}

pub(super) fn delete_owned_cascade(
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
    struct_id: StructId,
    keys: Vec<Key>,
    seen: &mut HashSet<RowOrigin>,
) -> Result<(), ExecError> {
    if keys.is_empty() {
        return Ok(());
    }
    let struct_def =
        ctx.schema
            .structs
            .get(struct_id.index())
            .ok_or_else(|| ExecError::Internal {
                message: format!("unknown struct id {}", struct_id.index()),
            })?;
    let owned_fields: Vec<(FieldId, StructId)> = struct_def
        .fields
        .iter()
        .enumerate()
        .filter_map(|(local, field)| match field {
            Field::Ref {
                owning: true,
                target,
                ..
            } => Some((FieldId::new(struct_id, local), *target)),
            _ => None,
        })
        .collect();

    for (field_id, target) in owned_fields {
        let child_keys = fetch_owned_keys(ctx, field_id, target, &keys, seen)?;
        delete_owned_cascade(ctx, write, target, child_keys, seen)?;
    }

    let array_fields: Vec<FieldId> = struct_def
        .fields
        .iter()
        .enumerate()
        .filter_map(|(local, field)| match field {
            Field::Array { .. } => Some(FieldId::new(struct_id, local)),
            Field::Value { ty, .. } if ty.has_array() => Some(FieldId::new(struct_id, local)),
            _ => None,
        })
        .collect();
    for field_id in array_fields {
        write.delete_array_field(field_id, &keys)?;
    }

    cleanup_non_owned_links(ctx.schema, struct_id, &keys, write)?;
    write.delete_rows(struct_id, &keys)
}

/// Clear non-owning FKs and many-to-many joins that reference deleted rows
fn cleanup_non_owned_links(
    schema: &ValidatedSchema,
    struct_id: StructId,
    keys: &[Key],
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let table = schema.structs[struct_id.index()].table;
    for (index, relation) in schema.relations.iter().enumerate() {
        let rel = RelationId::new(index);
        match relation {
            Relation::OneToOne { pk, .. }
            | Relation::ManyToOne { pk, .. }
            | Relation::OneToMany { pk, .. } => {
                if pk.table_id() == table && !schema.relation_has_owning_endpoint(relation) {
                    write.clear_foreign_keys(rel, keys)?;
                }
            }
            Relation::ManyToMany { a_ref, b_ref, .. } => {
                if a_ref.struct_id() == struct_id {
                    write.delete_join_rows(rel, true, keys)?;
                }
                if b_ref.struct_id() == struct_id {
                    write.delete_join_rows(rel, false, keys)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{int, rec};
    use crate::eval::mutations::tests::*;
    use crate::storage::Storage;
    use crate::value::Val;

    #[test]
    fn delete_list_target_recursive_cascade() {
        let schema = cascade_schema();
        let backend = cascade_backend(&schema);
        let user = schema.roots[0].struct_id;
        let order = schema.struct_by_name("Order").unwrap().0;
        let item = schema.struct_by_name("Item").unwrap().0;

        let result = run(
            r#"
                users.insert([
                    {
                        id = 1,
                        name = "A",
                        orders = [{
                            id = 10,
                            total = 1,
                            items = [{ id = 100 }, { id = 101 }],
                        }],
                    },
                    {
                        id = 2,
                        name = "B",
                        orders = [{ id = 20, total = 2, items = [] }],
                    },
                ]);
                users[id == 1].delete();
                users.len()
                "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(int(1)));
        assert_eq!(
            backend
                .fetch(user, &[], &FetchConstraints::EMPTY)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            backend
                .fetch(order, &[], &FetchConstraints::EMPTY)
                .unwrap()
                .len(),
            1
        );
        assert!(
            backend
                .fetch(item, &[], &FetchConstraints::EMPTY)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delete_one_to_many_fk_cleared() {
        let schema = one_to_many_schema();
        let backend = one_to_many_backend(&schema);

        let result = run(
            r#"
                parents.first().unwrap().delete();
                children.first().unwrap() { id, parent }
                "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(rec(vec![("id", int(10)), ("parent", Val::None)]))
        );
    }

    #[test]
    fn delete_self_ref_fk_cleared() {
        let schema = self_ref_schema();
        let backend = self_ref_backend(&schema);

        let result = run(
            r#"
                users.first().unwrap().delete();
                users.first().unwrap() { id, manager }
                "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(rec(vec![("id", int(2)), ("manager", Val::None)]))
        );
    }

    #[test]
    fn owning_delete_nested_non_owning_cleaned() {
        let schema = nested_link_schema();
        let backend = nested_link_backend(&schema);

        let result = run(
            r#"
                users.insert({
                    id = 1,
                    name = "u",
                    favorite = some(&images[id == 1].first().unwrap()),
                    orders = [{
                        id = 10,
                        total = 1,
                        image = some(&images[id == 1].first().unwrap()),
                        tags = &tags,
                    }],
                });
                images[id == 1].first().unwrap().delete();
                tags[id == 1].first().unwrap().delete();
                (
                    users.find(id == 1).unwrap().favorite?.id,
                    users.find(id == 1).unwrap().orders.first().unwrap().image?.id,
                    users.find(id == 1).unwrap().orders.first().unwrap().tags.id,
                    users.len(),
                    images.len(),
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(
            result,
            Some(Val::Tuple(vec![
                Val::None,
                Val::None,
                Val::list(vec![int(2)]),
                int(1),
                int(1),
            ]))
        );
    }

    #[test]
    fn delete_many_to_many_join_cleaned_for_user() {
        let schema = many_to_many_schema();
        let backend = many_to_many_backend(&schema);

        let result = run(
            r#"
                users.first().unwrap().delete();
                roles.map($.users.len())
                "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(Val::list(vec![int(1), int(0)])));
    }

    #[test]
    fn delete_many_to_many_join_cleaned_for_role() {
        let schema = many_to_many_schema();
        let backend = many_to_many_backend(&schema);

        let result = run(
            r#"
                roles.first().unwrap().delete();
                users.map($.roles.len())
                "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(Val::list(vec![int(1), int(0)])));
    }
}
