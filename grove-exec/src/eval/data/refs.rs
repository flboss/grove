use std::collections::HashMap;

use grove_schema::validated::FieldId;

use super::fetch::{batch_fetch, lookup_ref, push_ref};
use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::value::{Key, Val};

pub(crate) fn resolve_refs(ctx: &mut EvalCtx, value: Val) -> Result<Val, ExecError> {
    let mut by_field: HashMap<FieldId, Vec<Key>> = HashMap::new();
    let mut order: Vec<FieldId> = Vec::new();
    collect_refs(&value, &mut by_field, &mut order)?;
    let resolved = batch_fetch(ctx, &by_field, &order)?;
    splice_refs(value, &resolved)
}

/// Gather parent rowids from bare (non-owning) Ref fields
fn collect_refs(
    value: &Val,
    by_field: &mut HashMap<FieldId, Vec<Key>>,
    order: &mut Vec<FieldId>,
) -> Result<(), ExecError> {
    match value {
        Val::Ref { field, parent } => {
            push_ref(by_field, order, *field, *parent);
            Ok(())
        }
        Val::List(items) => {
            for item in items {
                collect_refs(item, by_field, order)?;
            }
            Ok(())
        }
        Val::Record(_)
        | Val::Some(_)
        | Val::None
        | Val::Scalar(_)
        | Val::Enum(_, _)
        | Val::Tuple(_)
        | Val::RowRef(_) => Ok(()),
    }
}

/// Replace bare (non-owning) Refs with resolved data
fn splice_refs(value: Val, resolved: &HashMap<(FieldId, Key), Val>) -> Result<Val, ExecError> {
    match value {
        Val::Ref { field, parent } => lookup_ref(field, parent, resolved),
        Val::List(items) => {
            let origin = items.origin;
            let spliced = items
                .into_iter()
                .map(|item| splice_refs(item, resolved))
                .collect::<Result<_, _>>()?;
            Ok(Val::list_with_origin(spliced, origin))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::data::tests::{record_field_value, run_seeded_recording};
    use crate::eval::harness::{int, run_seeded, str};
    use crate::testdata;
    use crate::value::Scalar;

    #[test]
    fn ref_links_ignored_in_equality() {
        let user_struct = testdata::struct_id(&testdata::schema(), "User");
        let manager_field = testdata::field_id(&testdata::schema(), user_struct, "manager");
        let link = Val::Ref {
            field: manager_field,
            parent: Key(1),
        };
        let other_link = Val::Ref {
            field: manager_field,
            parent: Key(2),
        };
        assert_eq!(
            link.to_string(),
            format!("ref({}:{})", user_struct.index(), manager_field.local())
        );
        assert_eq!(link.eq(&other_link), Some(true));
        assert_eq!(link.eq(&int(1)), None);
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

    #[test]
    fn non_owning_ident_chain_pushed_into_fetch_related() {
        let (result, recording) = run_seeded_recording(
            "users[id <= 2] { r = roles[name.contains(\"\")].take(1) { name } }",
        )
        .expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 2);
        for constraints in related.iter() {
            assert_eq!(constraints.filters.len(), 1);
            assert!(constraints.sort.is_empty());
            assert_eq!(constraints.offset, 0);
            assert_eq!(constraints.limit, Some(1));
        }

        let Val::List(rows) = result else {
            panic!("expected user rows, got {result:?}");
        };
        let first = record_field_value(&rows[0], "r");
        let second = record_field_value(&rows[1], "r");
        let first_names: Vec<&str> = match first {
            Val::List(roles) => roles
                .iter()
                .map(|role| match record_field_value(role, "name") {
                    Val::Scalar(Scalar::String(name)) => name.as_str(),
                    other => panic!("expected name, got {other:?}"),
                })
                .collect(),
            other => panic!("expected role list, got {other:?}"),
        };
        assert_eq!(first_names, ["admin"]);
        assert!(matches!(second, Val::List(roles) if roles.is_empty()));
    }

    #[test]
    fn lazy_non_owning_ref_record_origin() {
        let user = testdata::struct_id(&testdata::schema(), "User");
        let Ok(value) = run_seeded("users.first().unwrap().orders.first().unwrap().user") else {
            panic!("expected user");
        };
        let origin = value.row_origin().expect("user row origin");
        assert_eq!(origin.struct_id, user);
        assert!(origin.key.0 > 0);
    }
}
