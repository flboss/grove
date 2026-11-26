use std::collections::HashMap;

use grove_schema::validated::FieldId;

use super::fetch::{batch_fetch, lookup_ref, push_ref};
use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::value::{Key, Val};

pub(crate) fn resolve_projection_refs(ctx: &mut EvalCtx, value: Val) -> Result<Val, ExecError> {
    let mut by_field: HashMap<FieldId, Vec<Key>> = HashMap::new();
    let mut order: Vec<FieldId> = Vec::new();
    collect_projection_refs(&value, &mut by_field, &mut order);
    let resolved = batch_fetch(ctx, &by_field, &order)?;
    splice_projection_refs(value, &resolved)
}

/// Gather parent rowids from top-level Ref fields in projection output
fn collect_projection_refs(
    value: &Val,
    by_field: &mut HashMap<FieldId, Vec<Key>>,
    order: &mut Vec<FieldId>,
) {
    match value {
        Val::Record(record) => {
            for (_, item) in record {
                collect_item_refs(item, by_field, order);
            }
        }
        Val::List(elements) => {
            for element in elements {
                if let Val::Record(record) = element {
                    for (_, item) in record {
                        collect_item_refs(item, by_field, order);
                    }
                }
            }
        }
        Val::Ref { .. }
        | Val::Some(_)
        | Val::None
        | Val::Scalar(_)
        | Val::Enum(_, _)
        | Val::Tuple(_)
        | Val::RowRef(_) => {}
    }
}

/// Gather bare links at one item position
fn collect_item_refs(
    value: &Val,
    by_field: &mut HashMap<FieldId, Vec<Key>>,
    order: &mut Vec<FieldId>,
) {
    match value {
        Val::Ref { field, parent } => push_ref(by_field, order, *field, *parent),
        Val::List(items) => {
            for item in items {
                if let Val::Ref { field, parent } = item {
                    push_ref(by_field, order, *field, *parent);
                }
            }
        }
        Val::Record(_)
        | Val::Some(_)
        | Val::None
        | Val::Scalar(_)
        | Val::Enum(_, _)
        | Val::Tuple(_)
        | Val::RowRef(_) => {}
    }
}

/// Replace top-level Ref fields in projection output
fn splice_projection_refs(
    value: Val,
    resolved: &HashMap<(FieldId, Key), Val>,
) -> Result<Val, ExecError> {
    match value {
        Val::Record(record) => {
            let origin = record.origin;
            let spliced = record
                .into_iter()
                .map(|(name, item)| splice_item_refs(item, resolved).map(|val| (name, val)))
                .collect::<Result<_, _>>()?;
            Ok(Val::record_with_origin(spliced, origin))
        }
        Val::List(rows) => {
            let origin = rows.origin;
            let spliced = rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(record) => {
                        let origin = record.origin;
                        let spliced = record
                            .into_iter()
                            .map(|(name, item)| {
                                splice_item_refs(item, resolved).map(|val| (name, val))
                            })
                            .collect::<Result<_, _>>()?;
                        Ok(Val::record_with_origin(spliced, origin))
                    }
                    other => Ok(other),
                })
                .collect::<Result<_, _>>()?;
            Ok(Val::list_with_origin(spliced, origin))
        }
        other => Ok(other),
    }
}

/// Replace bare links at one item position
fn splice_item_refs(value: Val, resolved: &HashMap<(FieldId, Key), Val>) -> Result<Val, ExecError> {
    match value {
        Val::Ref { field, parent } => lookup_ref(field, parent, resolved),
        Val::List(items) => {
            let origin = items.origin;
            let spliced = items
                .into_iter()
                .map(|item| match item {
                    Val::Ref { field, parent } => lookup_ref(field, parent, resolved),
                    other => Ok(other),
                })
                .collect::<Result<_, _>>()?;
            Ok(Val::list_with_origin(spliced, origin))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::data::tests::{related_fields_for, run_seeded_recording};
    use crate::testdata;

    #[test]
    fn projection_prefetch_batched_owning_refs() {
        let schema = testdata::schema();
        let (_, recording) =
            run_seeded_recording("users { o = orders.len(), p = profile?.bio }").expect("query");
        let user = testdata::struct_id(&schema, "User");
        let profile = testdata::field_id(&schema, user, "profile");
        let orders = testdata::field_id(&schema, user, "orders");
        let bio = testdata::field_id(&schema, testdata::struct_id(&schema, "Profile"), "bio");
        let related = recording.related_fetches.borrow();
        assert_eq!(
            related.len(),
            2,
            "expected two batched fetches: {related:?}"
        );
        assert_eq!(
            related_fields_for(&schema, &related, orders),
            Some([].as_slice())
        );
        assert_eq!(
            related_fields_for(&schema, &related, profile),
            Some([bio].as_slice())
        );
    }

    #[test]
    fn projection_prefetch_only_needed_owning_ref() {
        let schema = testdata::schema();
        let (_, recording) = run_seeded_recording("users { o = orders.len() }").expect("query");
        let user = testdata::struct_id(&schema, "User");
        let orders = testdata::field_id(&schema, user, "orders");
        let profile = testdata::field_id(&schema, user, "profile");
        let related = recording.related_fetches.borrow();
        assert_eq!(related.len(), 1, "expected only orders: {related:?}");
        assert_eq!(
            related_fields_for(&schema, &related, orders),
            Some([].as_slice())
        );
        assert_eq!(related_fields_for(&schema, &related, profile), None);
        assert_eq!(recording.related_parent_counts.borrow().as_slice(), &[5]);
    }
}
