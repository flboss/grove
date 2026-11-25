use std::collections::HashSet;

use grove_query::TypedExpr;
use grove_schema::validated::{FieldId, StructId, ValidatedSchema};

use super::cascade::delete_owned_children;
use super::insert::insert_owned_field;
use super::records::{
    SplitMode, collect_update_target_records, record_from_value, split_record_fields,
};
use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::storage::{LinkMode, WriteTxn};
use crate::value::{RecordVal, RowOrigin, Val, unwrap_optional_depth};

pub(super) fn exec_update(
    base: &TypedExpr,
    arg: &TypedExpr,
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
    let targets = collect_update_target_records(target)?;
    if targets.is_empty() {
        return Ok(());
    }

    let mut updates = Vec::with_capacity(targets.len());
    for (origin, record) in targets {
        ctx.push_binding_scope("prev", record);
        let value = eval(arg, ctx);
        ctx.env.pop_scope();
        updates.push((origin, record_from_value(value?)?));
    }

    for (origin, record) in updates {
        let parts = resolve_update_parts(ctx.schema, origin.struct_id, record)?;
        apply_update(ctx, write, origin, parts)?;
    }
    Ok(())
}

pub(super) struct UpdateParts {
    /// ids and new values of modified base and array fields
    pub(super) base: Vec<(FieldId, Val)>,
    /// ids of modified array fields (values in `base`)
    pub(super) arrays: Vec<FieldId>,
    /// ids and new values of modified owning ref fields
    pub(super) owned: Vec<(FieldId, Val)>,
    /// ids and target row references of modified non-owning fields
    pub(super) links: Vec<(FieldId, Vec<RowOrigin>)>,
}

/// Apply a prepared update after all argument values have been computed
fn apply_update(
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
    origin: RowOrigin,
    parts: UpdateParts,
) -> Result<(), ExecError> {
    let mut seen = HashSet::new();
    for (field_id, _) in &parts.owned {
        delete_owned_children(ctx, write, *field_id, origin.key, &mut seen)?;
    }
    for field_id in &parts.arrays {
        write.delete_array_field(*field_id, &[origin.key])?;
    }
    if !parts.base.is_empty() {
        write.update_row(origin.struct_id, origin.key, &parts.base)?;
    }
    for (field_id, targets) in &parts.links {
        write.link_row_refs(*field_id, origin.key, targets, LinkMode::Replace)?;
    }
    for (field_id, value) in parts.owned {
        insert_owned_field(ctx.schema, field_id, value, origin.key, write)?;
    }
    Ok(())
}

pub(super) fn resolve_update_parts(
    schema: &ValidatedSchema,
    struct_id: StructId,
    record: RecordVal,
) -> Result<UpdateParts, ExecError> {
    let parts = split_record_fields(schema, struct_id, record, SplitMode::Update)?;
    Ok(UpdateParts {
        base: parts.base,
        arrays: parts.arrays,
        owned: parts.owned,
        links: parts.links,
    })
}
