use std::collections::{HashMap, HashSet};

use grove_query::TypedExpr;
use grove_schema::validated::StructId;

use super::cascade::delete_owned_cascade;
use super::records::records_from_value;
use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::WriteTxn;
use crate::value::{Key, RowOrigin, Val, unwrap_optional_depth};

pub(super) fn exec_delete(
    base: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let target = eval(base, ctx)?;
    let target = if opt_depth > 0 {
        let (target, peeled) = unwrap_optional_depth(target, opt_depth)?;
        if peeled < opt_depth {
            return Ok(());
        }
        target
    } else {
        target
    };
    let mut origins = collect_delete_target_origins(target)?;
    if origins.is_empty() {
        return Ok(());
    }

    let mut groups: HashMap<StructId, Vec<Key>> = HashMap::new();
    for origin in &origins {
        groups.entry(origin.struct_id).or_default().push(origin.key);
    }
    for (struct_id, keys) in groups {
        delete_owned_cascade(ctx, write, struct_id, keys, &mut origins)?;
    }
    Ok(())
}

/// Extract row origins from a record or list delete target
fn collect_delete_target_origins(value: Val) -> Result<HashSet<RowOrigin>, ExecError> {
    records_from_value(value, "delete target")?
        .into_iter()
        .map(|record| {
            record.origin.ok_or_else(|| ExecError::Internal {
                message: "delete target record has no row origin".to_string(),
            })
        })
        .collect()
}
