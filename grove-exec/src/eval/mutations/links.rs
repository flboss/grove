use std::collections::HashSet;

use grove_schema::validated::{Field, FieldId, ValidatedSchema};

use crate::error::ExecError;
use crate::value::{RowOrigin, Val};

pub(super) fn link_targets(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
) -> Result<Vec<RowOrigin>, ExecError> {
    let mut targets = Vec::new();
    collect_link_targets(schema, field_id, value, &mut targets)?;
    let mut seen = HashSet::new();
    targets.retain(|origin| seen.insert(*origin));
    Ok(targets)
}

fn collect_link_targets(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
    targets: &mut Vec<RowOrigin>,
) -> Result<(), ExecError> {
    let Some(Field::Ref { name, target, .. }) = schema.field(field_id) else {
        return Err(ExecError::Internal {
            message: "link field is not a ref".to_string(),
        });
    };
    match value {
        Val::RowRef(origin) => {
            if origin.struct_id != *target {
                return Err(ExecError::Internal {
                    message: format!("row reference target mismatch for field `{name}`"),
                });
            }
            targets.push(origin);
            Ok(())
        }
        Val::None => Ok(()),
        Val::Some(inner) => collect_link_targets(schema, field_id, *inner, targets),
        Val::List(list) => {
            for item in list.items {
                collect_link_targets(schema, field_id, item, targets)?;
            }
            Ok(())
        }
        other => Err(ExecError::Internal {
            message: format!("link field contains non-RowRef value {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::update::resolve_update_parts;
    use super::*;
    use crate::eval::harness::{int, some};
    use crate::eval::mutations::tests::*;
    use crate::storage::{LinkMode, Storage};
    use crate::value::{Key, RecordVal};
    use std::rc::Rc;

    #[test]
    fn link_split_mismatched_target_rejected() {
        let schema = mutation_parts_schema();
        let (user, order, _) = mutation_parts_ids(&schema);
        let wrong = RowOrigin {
            struct_id: order,
            key: Key(1),
        };
        let record = RecordVal::new(vec![(Rc::from("manager"), some(Val::RowRef(wrong)))]);
        assert!(matches!(
            resolve_update_parts(&schema, user, record),
            Err(ExecError::Internal { .. })
        ));
    }

    #[test]
    fn link_targets_deduplication() {
        let schema = one_to_many_schema();
        let parent = schema.struct_by_name("Parent").unwrap().0;
        let child = schema.struct_by_name("Child").unwrap().0;
        let field = schema.struct_field(parent, "children").unwrap().0;
        let first = RowOrigin {
            struct_id: child,
            key: Key(10),
        };
        let second = RowOrigin {
            struct_id: child,
            key: Key(11),
        };
        let targets = link_targets(
            &schema,
            field,
            Val::list(vec![
                Val::RowRef(first),
                Val::RowRef(second),
                Val::RowRef(first),
            ]),
        )
        .unwrap();
        assert_eq!(targets, vec![first, second]);
    }

    #[test]
    fn self_ref_non_owning_mutations() {
        let schema = self_ref_schema();
        let backend = self_ref_backend(&schema);

        let result = run(
            r#"
                users.insert({ id = 3, name = "third", manager = none });
                users[id == 1].update({ subordinates = &users[id == 2] });
                (
                    users.find(id == 1).unwrap().subordinates.len(),
                    users.find(id == 3).unwrap().manager?.id
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(result, Some(Val::Tuple(vec![int(1), Val::None])));

        let result = run(
            r#"
                users.find(id == 1).unwrap().subordinates.insert(&users[id == 3]);
                (
                    users[id == 1].first().unwrap().subordinates.len(),
                    users[id == 3].first().unwrap().manager?.id
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(result, Some(Val::Tuple(vec![int(2), some(int(1))])));

        let result = run(
            r#"
                users.find(id == 3)?.update({ manager = some(&users.find(id == 1).unwrap()) });
                users.find(id == 3)?.manager??.id
                "#,
            &schema,
            &backend,
        );
        assert_eq!(result, Some(some(some(int(1)))));

        let result = run(
            r#"
                users[id == 3].update({ manager = none });
                users.find(id == 3).unwrap().manager?.id
                "#,
            &schema,
            &backend,
        );
        assert_eq!(result, Some(Val::None));
    }

    #[test]
    fn link_api_missing_targets_rejected() {
        let schema = one_to_many_schema();
        let backend = one_to_many_backend(&schema);
        let parent = schema.struct_by_name("Parent").unwrap().0;
        let child = schema.struct_by_name("Child").unwrap().0;
        let parent_field = schema.struct_field(child, "parent").unwrap().0;
        let children_field = schema.struct_field(parent, "children").unwrap().0;
        let missing_parent = RowOrigin {
            struct_id: parent,
            key: Key(999),
        };
        let missing_child = RowOrigin {
            struct_id: child,
            key: Key(999),
        };

        let mut txn = backend.begin_write().unwrap();
        assert!(matches!(
            txn.link_row_refs(parent_field, Key(1), &[missing_parent], LinkMode::Append),
            Err(ExecError::Integrity { .. })
        ));
        assert!(matches!(
            txn.link_row_refs(children_field, Key(1), &[missing_child], LinkMode::Append),
            Err(ExecError::Integrity { .. })
        ));
        txn.rollback().unwrap();
    }

    #[test]
    fn many_to_many_link_missing_targets_rejected() {
        let schema = many_to_many_schema();
        let backend = many_to_many_backend(&schema);
        let user = schema.struct_by_name("User").unwrap().0;
        let role = schema.struct_by_name("Role").unwrap().0;
        let roles_field = schema.struct_field(user, "roles").unwrap().0;
        let missing = RowOrigin {
            struct_id: role,
            key: Key(999),
        };

        let mut txn = backend.begin_write().unwrap();
        assert!(matches!(
            txn.link_row_refs(roles_field, Key(1), &[missing], LinkMode::Append),
            Err(ExecError::Integrity { .. })
        ));
        txn.rollback().unwrap();
    }

    #[test]
    fn many_to_many_append_idempotence() {
        let schema = many_to_many_schema();
        let backend = many_to_many_backend(&schema);

        let result = run(
            r#"
                users.find(id == 2).unwrap().roles.insert(&roles[id == 2]);
                users.find(id == 2).unwrap().roles.insert(&roles[id == 2]);
                roles.find(id == 1).unwrap().users.insert(&users[id == 2]);
                (
                    users.find(id == 2).unwrap().roles.len(),
                    roles.find(id == 1).unwrap().users.len(),
                    roles.len()
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(result, Some(Val::Tuple(vec![int(2), int(2), int(2)])));
    }

    #[test]
    fn many_to_many_replace_any_endpoint() {
        let schema = many_to_many_schema();
        let backend = many_to_many_backend(&schema);

        let result = run(
            r#"
                users.find(id == 1)?.update({ roles = &roles[id == 2] });
                (
                    users.find(id == 1).unwrap().roles.id,
                    roles.len()
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(
            result,
            Some(Val::Tuple(vec![Val::list(vec![int(2)]), int(2),]))
        );

        let result = run(
            r#"
                roles.find(id == 2)?.update({ users = &users[id == 2] });
                (
                    roles.find(id == 2).unwrap().users.id,
                    users.find(id == 1).unwrap().roles.len(),
                    users.find(id == 2).unwrap().roles.id,
                    users.len(),
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(
            result,
            Some(Val::Tuple(vec![
                Val::list(vec![int(2)]),
                int(0),
                Val::list(vec![int(1), int(2)]),
                int(2),
            ]))
        );
    }

    #[test]
    fn many_to_many_clear_join_table() {
        let schema = many_to_many_schema();
        let backend = many_to_many_backend(&schema);

        let result = run(
            r#"
                users.find(id == 1)?.update({ roles = [] });
                (
                    users.find(id == 1).unwrap().roles.len(),
                    roles.len(),
                    users.len(),
                )
                "#,
            &schema,
            &backend,
        );
        assert_eq!(result, Some(Val::Tuple(vec![int(0), int(2), int(2)])));
    }
}
