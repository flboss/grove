use std::collections::HashSet;

use grove_query::methods;
use grove_query::{IdentBinding, TypedExpr, TypedExprKind};
use grove_schema::validated::{Field, FieldId, ValidatedSchema};

use super::owning::resolve_owning_values;
use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::method_meta;
use crate::value::Val;

pub(crate) fn owning_refs_in(
    expr: &TypedExpr,
    schema: &ValidatedSchema,
    skip: &HashSet<FieldId>,
) -> HashSet<FieldId> {
    let mut fields = HashSet::new();
    collect_owning_refs_in(expr, schema, skip, &mut fields);
    fields
}

/// Resolve the given owning Ref fields across values before per-element work
pub(crate) fn prefetch_owning(
    ctx: &mut EvalCtx,
    values: &mut [Val],
    fields: &HashSet<FieldId>,
) -> Result<(), ExecError> {
    if fields.is_empty() {
        return Ok(());
    }
    resolve_owning_values(ctx, values, Some(fields))
}

/// Collect owning Ref fields referenced by an expression
pub(crate) fn collect_owning_refs_in(
    expr: &TypedExpr,
    schema: &ValidatedSchema,
    skip: &HashSet<FieldId>,
    fields: &mut HashSet<FieldId>,
) {
    let mut frames = vec![true];
    collect_owning_refs_at(expr, schema, skip, &mut frames, fields);
}

fn collect_owning_refs_at(
    expr: &TypedExpr,
    schema: &ValidatedSchema,
    skip: &HashSet<FieldId>,
    // collectability flag per pushed element/projection scope, outermost first
    frames: &mut Vec<bool>,
    fields: &mut HashSet<FieldId>,
) {
    let current = frames.len() - 1;
    match &expr.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(field),
            depth,
            ..
        } => {
            if let Some(frame) = current.checked_sub(*depth)
                && frames[frame]
                && schema.field_is_ref_owning(*field) == Some(true)
                && !skip.contains(field)
            {
                fields.insert(*field);
            }
        }
        TypedExprKind::Field { base, name, .. } => {
            if let Some(frame) = rooted_frame(base, schema, frames)
                && frames[frame]
                && let Some(struct_id) = base.ty.schema_record()
                && let Some((field, _)) = schema.struct_field(struct_id, name)
                && schema.field_is_ref_owning(field) == Some(true)
                && !skip.contains(&field)
            {
                fields.insert(field);
            }
            collect_owning_refs_at(base, schema, skip, frames, fields);
        }
        TypedExprKind::Method {
            method, base, args, ..
        } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
            if methods::method(*method).is_scoped() {
                let nested = rooted_frame(base, schema, frames).is_some_and(|frame| frames[frame]);
                frames.push(nested);
                for arg in args {
                    collect_owning_refs_at(&arg.expr, schema, skip, frames, fields);
                }
                frames.pop();
            } else {
                for arg in args {
                    collect_owning_refs_at(&arg.expr, schema, skip, frames, fields);
                }
            }
        }
        TypedExprKind::TupleAccess { base, .. } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            collect_owning_refs_at(lhs, schema, skip, frames, fields);
            collect_owning_refs_at(rhs, schema, skip, frames, fields);
        }
        TypedExprKind::Unary { expr, .. }
        | TypedExprKind::Cast { expr, .. }
        | TypedExprKind::RowRef { expr }
        | TypedExprKind::Some { value: expr } => {
            collect_owning_refs_at(expr, schema, skip, frames, fields);
        }
        TypedExprKind::If { arms, default } => {
            for (condition, value) in arms {
                collect_owning_refs_at(condition, schema, skip, frames, fields);
                collect_owning_refs_at(value, schema, skip, frames, fields);
            }
            collect_owning_refs_at(default, schema, skip, frames, fields);
        }
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            for element in elements {
                collect_owning_refs_at(element, schema, skip, frames, fields);
            }
        }
        TypedExprKind::Struct { fields: values } => {
            for (_, value) in values {
                collect_owning_refs_at(value, schema, skip, frames, fields);
            }
        }
        TypedExprKind::Projection { base, items } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
            let nested = rooted_frame(base, schema, frames).is_some_and(|frame| frames[frame]);
            frames.push(nested);
            for item in items {
                collect_owning_refs_at(&item.value, schema, skip, frames, fields);
            }
            frames.pop();
        }
        TypedExprKind::Literal(_)
        | TypedExprKind::TypeConstant { .. }
        | TypedExprKind::EnumVariant { .. }
        | TypedExprKind::Ident { .. } => {}
    }
}

/// Frame from which `expr`'s value is reachable through `$`/owning Refs, if any
fn rooted_frame(expr: &TypedExpr, schema: &ValidatedSchema, frames: &[bool]) -> Option<usize> {
    let current = frames.len().checked_sub(1)?;
    match &expr.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::ListElement,
            depth,
            ..
        } => current.checked_sub(*depth),
        TypedExprKind::Ident {
            binding: IdentBinding::Field(field),
            depth,
            ..
        } => {
            let frame = current.checked_sub(*depth)?;
            (schema.field_is_ref_owning(*field) == Some(true)).then_some(frame)
        }
        TypedExprKind::Field { base, name, .. } => {
            let frame = rooted_frame(base, schema, frames)?;
            let (field, def) = base
                .ty
                .schema_record()
                .and_then(|struct_id| schema.struct_field(struct_id, name))?;
            (matches!(def, Field::Value { .. } | Field::Array { .. })
                || schema.field_is_ref_owning(field) == Some(true))
            .then_some(frame)
        }
        TypedExprKind::Method { method, base, .. }
            if method_meta::is_element_preserving(*method) =>
        {
            rooted_frame(base, schema, frames)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::data::tests::{record_field_value, related_fields_for, run_seeded_recording};
    use crate::eval::harness::{int, rec, str};
    use crate::testdata;
    use crate::value::Scalar;

    #[test]
    fn filter_prefetch_batched_owning_refs() {
        let schema = testdata::schema();
        let (_, recording) =
            run_seeded_recording("users[orders.len() > 1] { name }").expect("query");
        let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
        let related = recording.related_fetches.borrow();
        assert_eq!(related.len(), 1, "expected one batched fetch: {related:?}");
        assert_eq!(
            related_fields_for(&schema, &related, orders),
            Some([].as_slice())
        );
        assert_eq!(recording.related_parent_counts.borrow().as_slice(), &[5]);
    }

    #[test]
    fn prefetch_refs_behind_non_owning_refs_skipped() {
        let (result, recording) =
            run_seeded_recording("users[manager?.orders?.len() == some(1)] { name }")
                .expect("query");
        let counts = recording.related_parent_counts.borrow();
        assert!(!counts.is_empty(), "manager chain resolves lazily");
        assert!(
            counts.iter().all(|count| *count == 1),
            "expected only per-element lazy fetches, got {counts:?}"
        );
        match result {
            Val::List(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(record_field_value(&rows[0], "name"), &str("Ada"));
                assert_eq!(record_field_value(&rows[1], "name"), &str("Cyd"));
            }
            other => panic!("expected projected rows, got {other:?}"),
        }
    }

    #[test]
    fn map_prefetch_batched_record_field_refs() {
        let schema = testdata::schema();
        let (_, recording) = run_seeded_recording("users.map($.orders.len())").expect("query");
        let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
        let related = recording.related_fetches.borrow();
        assert_eq!(related.len(), 1, "expected one batched fetch: {related:?}");
        assert_eq!(
            related_fields_for(&schema, &related, orders),
            Some([].as_slice())
        );
        assert_eq!(recording.related_parent_counts.borrow().as_slice(), &[5]);
    }

    #[test]
    fn prefetch_nested_owned_scopes() {
        let (result, recording) =
            run_seeded_recording("users[orders[items.len() > 0].len() > 0] { name }")
                .expect("query");

        let counts = recording.related_parent_counts.borrow();
        assert_eq!(
            counts.as_slice(),
            &[5, 3],
            "expected batched User.orders then Order.items fetches: {counts:?}"
        );
        assert_eq!(result, Val::list(vec![rec(vec![("name", str("Bob"))])]));
    }

    #[test]
    fn first_record_projection_no_owning_refs_fetched() {
        let (_, recording) = run_seeded_recording("users.first()?.name").expect("query");
        assert!(recording.related_fetches.borrow().is_empty(),);
    }

    #[test]
    fn field_trigger_owning_pushed_into_fetch_related() {
        let (result, recording) =
            run_seeded_recording("users[id <= 2].map($.orders[amount > 150].take(1).len())")
                .expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 2);
        for constraints in related.iter() {
            assert_eq!(constraints.filters.len(), 1);
            assert!(constraints.sort.is_empty());
            assert_eq!(constraints.offset, 0);
            assert_eq!(constraints.limit, Some(1));
        }
        assert_eq!(result, Val::list(vec![int(1), int(1)]));
    }

    #[test]
    fn field_trigger_non_owning_pushed_into_fetch_related() {
        let (result, recording) =
            run_seeded_recording("users[id <= 2].map($.roles.take(1).len())").expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 2);
        for constraints in related.iter() {
            assert!(constraints.filters.is_empty());
            assert!(constraints.sort.is_empty());
            assert_eq!(constraints.offset, 0);
            assert_eq!(constraints.limit, Some(1));
        }
        assert_eq!(result, Val::list(vec![int(1), int(0)]));
    }

    #[test]
    fn ident_trigger_filter_predicate() {
        let (result, recording) =
            run_seeded_recording("users[orders[amount > 150].take(1).len() > 0] { name }")
                .expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 5);
        for constraints in related.iter() {
            assert_eq!(constraints.filters.len(), 1);
            assert_eq!(constraints.limit, Some(1));
        }
        assert_eq!(
            result,
            Val::list(vec![
                rec(vec![("name", str("Ada"))]),
                rec(vec![("name", str("Bob"))]),
            ])
        );
    }

    fn unpack_optional_int(value: &Val) -> Option<i64> {
        match value {
            Val::Some(inner) => match inner.as_ref() {
                Val::Scalar(Scalar::Int(value)) => Some(*value),
                other => panic!("expected int, got {other:?}"),
            },
            Val::None => None,
            other => panic!("expected optional int, got {other:?}"),
        }
    }

    #[test]
    fn related_fetch_first_nth() {
        let (result, recording) = run_seeded_recording(
            "users[id <= 2] { a = orders.first()?.amount, b = orders.nth(1)?.amount }",
        )
        .expect("query");

        let related = recording.related_constraints.borrow();
        assert_eq!(related.len(), 4);
        for pair in related.chunks(2) {
            assert_eq!(pair[0].offset, 0);
            assert_eq!(pair[0].limit, Some(1));
            assert_eq!(pair[1].offset, 1);
            assert_eq!(pair[1].limit, Some(1));
        }

        let Val::List(rows) = result else {
            panic!("expected user rows, got {result:?}");
        };
        assert_eq!(
            unpack_optional_int(record_field_value(&rows[0], "a")),
            Some(100)
        );
        assert_eq!(
            unpack_optional_int(record_field_value(&rows[0], "b")),
            Some(200)
        );
        assert_eq!(
            unpack_optional_int(record_field_value(&rows[1], "a")),
            Some(400)
        );
        assert_eq!(unpack_optional_int(record_field_value(&rows[1], "b")), None);
    }
}
