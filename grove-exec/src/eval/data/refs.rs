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
