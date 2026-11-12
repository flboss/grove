use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use grove_query::typecheck::types::{IdentBinding, TypedExpr, TypedExprKind};
use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};
use grove_types::NodeId;

use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
use crate::pushdown::FetchConstraints;
use crate::storage::{Key, Row};
use crate::value::{Val, rewrap_optional};

/// Read root table into a List<Record> (owning Refs resolved)
pub(crate) fn exec_root(
    ctx: &mut EvalCtx,
    root_idx: usize,
    node: NodeId,
) -> Result<Val, ExecError> {
    let root = ctx
        .schema
        .roots
        .get(root_idx)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown root {root_idx} (internal error)"),
        })?;
    let fields = ctx.plan.fields_for(root.struct_id).to_vec();
    let rows = if let Some(constraints) = ctx.plan.constraints_for(node) {
        ctx.storage.fetch(root.struct_id, &fields, constraints)?
    } else {
        ctx.storage
            .fetch(root.struct_id, &fields, &FetchConstraints::EMPTY)?
    };
    ctx.applied_triggers.insert(node);
    materialize(ctx, root.struct_id, &fields, rows)
}

/// Fetch a Ref field for a batch of parents
fn resolve_field(
    ctx: &mut EvalCtx,
    field: FieldId,
    parents: &[Key],
    constraints: &FetchConstraints,
) -> Result<Vec<ResolvedLink>, ExecError> {
    let (target, field_name) =
        match &ctx.schema.structs[field.struct_id().index()].fields[field.local()] {
            Field::Ref { target, name, .. } => (*target, name),
            Field::Value { name, .. } | Field::Array { name, .. } => {
                return Err(ExecError::Integrity {
                    message: format!("Ref link names a non-Ref field `{name}` (internal error)"),
                });
            }
        };
    let relation = ctx
        .schema
        .relation_of_field(field)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("Ref field `{field_name}` has no relation (internal error)"),
        })?;
    let fetch_from_first = ctx.schema.relations[relation.index()].endpoints()[0] == field;
    let fields = ctx.plan.fields_for(target).to_vec();
    let groups =
        ctx.storage
            .fetch_related(relation, fetch_from_first, parents, &fields, constraints)?;
    let mut resolved_pairs = Vec::with_capacity(parents.len());
    for (parent, group) in parents.iter().zip(groups) {
        // Recursion terminates: ownership chain ends at a root
        let value = materialize(ctx, target, &fields, group)?;
        let wrapped = wrap_fetched(value, field, *parent, ctx.schema)?;
        resolved_pairs.push(((field, *parent), wrapped));
    }
    Ok(resolved_pairs)
}

/// Wrap fetch result according to the Ref field's shape
fn wrap_fetched(
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
                return Err(ExecError::Integrity {
                    message: "Ref link names a non-Ref field (internal error)".to_string(),
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
                1 => Ok(Val::Some(Box::new(items.pop().unwrap()))),
                n => Err(ExecError::Integrity {
                    message: format!(
                        "optional Ref `{field_name}` resolved to {n} rows (for parent key {})",
                        parent.0
                    ),
                }),
            },
            other => Err(ExecError::Integrity {
                message: format!("optional Ref resolved to {other} (internal error)"),
            }),
        };
    }
    match value {
        Val::List(mut items) => match items.len() {
            1 => Ok(items.pop().unwrap()),
            n => Err(ExecError::Integrity {
                message: format!(
                    "required Ref `{field_name}` resolved to {n} rows (for parent key {})",
                    parent.0
                ),
            }),
        },
        other => Err(ExecError::Integrity {
            message: format!("required Ref resolved to {other} (internal error)"),
        }),
    }
}

/// Execute batched `fetch_related` per distinct field
fn batch_fetch(
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
                            values.next().ok_or_else(|| ExecError::Integrity {
                                message: "row has fewer values than fields (internal error)"
                                    .to_string(),
                            })?,
                        ));
                        next_field += 1;
                    }
                }
            }
        }
        if next_field != fields.len() {
            return Err(ExecError::Integrity {
                message: "row is missing requested fields (internal error)".to_string(),
            });
        }
        if values.next().is_some() {
            return Err(ExecError::Integrity {
                message: "row has more values than fields (internal error)".to_string(),
            });
        }
        items.push(Val::Record(record));
    }
    Ok(Val::List(items))
}

type ResolvedLink = ((FieldId, Key), Val);

/// Determine whether a Ref field is owning
fn is_owning_ref(schema: &ValidatedSchema, field: FieldId) -> Result<bool, ExecError> {
    let struct_ = schema
        .structs
        .get(field.struct_id().index())
        .ok_or_else(|| ExecError::Integrity {
            message: format!(
                "unknown struct id {} (internal error)",
                field.struct_id().index()
            ),
        })?;
    match struct_.fields.get(field.local()) {
        Some(Field::Ref { owning, .. }) => Ok(*owning),
        Some(Field::Value { name, .. } | Field::Array { name, .. }) => Err(ExecError::Integrity {
            message: format!("Ref link names a non-Ref field `{name}` (internal error)"),
        }),
        None => Err(ExecError::Integrity {
            message: format!("unknown field {field:?} (internal error)"),
        }),
    }
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
        Val::List(items) | Val::Tuple(items) => {
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
        Val::None | Val::Scalar(_) | Val::Enum(_, _) => Ok(()),
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
        Val::List(items) | Val::Tuple(items) => {
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
        Val::None | Val::Scalar(_) | Val::Enum(_, _) => Ok(()),
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
pub(crate) fn resolve_owning_ident(
    ctx: &mut EvalCtx,
    field: FieldId,
    value: Val,
) -> Result<Val, ExecError> {
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
                    None => Err(ExecError::Integrity {
                        message: "constrained Ref resolved to no value (internal error)"
                            .to_string(),
                    }),
                }
            }
            other => resolve_owning_ident(ctx, field, other),
        };
    }
    resolve_owning_ident(ctx, field, value)
}

/// Owning Ref fields referenced by an expression
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
            base, name, args, ..
        } => {
            collect_owning_refs_at(base, schema, skip, frames, fields);
            if is_scoped_method(name) {
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

fn is_scoped_method(method: &str) -> bool {
    matches!(
        method,
        "filter"
            | "sort_over"
            | "sort_asc_over"
            | "sort_desc_over"
            | "map"
            | "sum_over"
            | "avg_over"
            | "max_by"
            | "min_by"
    )
}

/// Methods whose result is still made of the same underlying element values
/// as their base
fn is_element_preserving_method(method: &str) -> bool {
    matches!(
        method,
        // subset
        "filter"
        // reordering
            | "sort"
            | "sort_asc"
            | "sort_desc"
            | "sort_over"
            | "sort_asc_over"
            | "sort_desc_over"
        // subsequence
            | "take"
            | "skip"
        // removes nesting level
            | "flatten"
        // single element exposed
            | "first"
            | "nth"
            | "unwrap"
    )
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
        TypedExprKind::Method { base, name, .. } if is_element_preserving_method(name) => {
            rooted_frame(base, schema, frames)
        }
        _ => None,
    }
}

/// Perform a record field access
pub(crate) fn exec_field(
    base: &TypedExpr,
    name: &str,
    optional: bool,
    node: NodeId,
    ctx: &mut EvalCtx,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    let result = match base {
        Val::Record(fields) => {
            let value = record_field(fields, name)?;
            let constraints = ctx
                .plan
                .constraints_for(node)
                .filter(|constraints| !constraints.is_empty());
            match (constraints, value) {
                (Some(constraints), Val::Ref { field, parent }) => {
                    let mut resolved = resolve_field(ctx, field, &[parent], constraints)?;
                    ctx.applied_triggers.insert(node);
                    let value = match resolved.pop() {
                        Some((_, value)) => value,
                        None => {
                            return Err(ExecError::Integrity {
                                message: "constrained Ref resolved to no value (internal error)"
                                    .to_string(),
                            });
                        }
                    };
                    return Ok(rewrap_optional(optional, value));
                }
                (_, value) => value,
            }
        }
        Val::List(items) => {
            let mut mapped = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Val::Record(fields) => mapped.push(record_field(fields, name)?),
                    Val::None => mapped.push(Val::None),
                    other => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "field `{name}` on non-record list element: {other} (internal error)"
                            ),
                        });
                    }
                }
            }
            Val::List(mapped)
        }
        _ => {
            return Err(ExecError::Integrity {
                message: format!("field `{name}` on non-record value (internal error)"),
            });
        }
    };
    resolve_refs(ctx, optional, result)
}

/// Resolve a bare Ref base before navigation
pub(crate) fn resolve_base_link(ctx: &mut EvalCtx, base: Val) -> Result<Val, ExecError> {
    match base {
        Val::Ref { field, parent } => {
            let mut resolved = resolve_field(ctx, field, &[parent], &FetchConstraints::EMPTY)?;
            match resolved.pop() {
                Some((_, value)) => Ok(value),
                None => Err(ExecError::Integrity {
                    message: "base Ref resolved to no value (internal error)".to_string(),
                }),
            }
        }
        Val::List(items) => {
            let mut resolved_items = Vec::with_capacity(items.len());
            for item in items {
                resolved_items.push(resolve_base_link(ctx, item)?);
            }
            Ok(Val::List(resolved_items))
        }
        Val::Record(_)
        | Val::Some(_)
        | Val::None
        | Val::Scalar(_)
        | Val::Enum(_, _)
        | Val::Tuple(_) => Ok(base),
    }
}

fn record_field(
    fields: impl IntoIterator<Item = (Rc<str>, Val)>,
    name: &str,
) -> Result<Val, ExecError> {
    fields
        .into_iter()
        .find(|(field_name, _)| field_name.as_ref() == name)
        .map(|(_, value)| value)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown field `{name}` (internal error)"),
        })
}

/// Resolve bare top-level (non-owning) Refs in a field access result
fn resolve_refs(ctx: &mut EvalCtx, optional: bool, value: Val) -> Result<Val, ExecError> {
    let mut by_field: HashMap<FieldId, Vec<Key>> = HashMap::new();
    let mut order: Vec<FieldId> = Vec::new();
    collect_refs(&value, &mut by_field, &mut order)?;
    let resolved = batch_fetch(ctx, &by_field, &order)?;
    let resolved_value = splice_refs(value, &resolved)?;
    Ok(rewrap_optional(optional, resolved_value))
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
        | Val::Tuple(_) => Ok(()),
    }
}

/// Replace bare (non-owning) Refs with resolved data
fn splice_refs(value: Val, resolved: &HashMap<(FieldId, Key), Val>) -> Result<Val, ExecError> {
    match value {
        Val::Ref { field, parent } => lookup_ref(field, parent, resolved),
        Val::List(items) => {
            let spliced = items
                .into_iter()
                .map(|item| splice_refs(item, resolved))
                .collect::<Result<_, _>>()?;
            Ok(Val::List(spliced))
        }
        other => Ok(other),
    }
}

/// Record one parent rowid for a link
fn push_ref(
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
fn lookup_ref(
    field: FieldId,
    parent: Key,
    resolved: &HashMap<(FieldId, Key), Val>,
) -> Result<Val, ExecError> {
    resolved
        .get(&(field, parent))
        .cloned()
        .ok_or_else(|| ExecError::Integrity {
            message: "Ref data missing from batch fetch result (internal error)".to_string(),
        })
}

/// Resolve Refs at projection output positions one level
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
        | Val::Tuple(_) => {}
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
        | Val::Tuple(_) => {}
    }
}

/// Replace top-level Ref fields in projection output
fn splice_projection_refs(
    value: Val,
    resolved: &HashMap<(FieldId, Key), Val>,
) -> Result<Val, ExecError> {
    match value {
        Val::Record(record) => {
            let spliced = record
                .into_iter()
                .map(|(name, item)| splice_item_refs(item, resolved).map(|val| (name, val)))
                .collect::<Result<_, _>>()?;
            Ok(Val::Record(spliced))
        }
        Val::List(rows) => {
            let spliced = rows
                .into_iter()
                .map(|row| match row {
                    Val::Record(record) => {
                        let spliced = record
                            .into_iter()
                            .map(|(name, item)| {
                                splice_item_refs(item, resolved).map(|val| (name, val))
                            })
                            .collect::<Result<_, _>>()?;
                        Ok(Val::Record(spliced))
                    }
                    other => Ok(other),
                })
                .collect::<Result<_, _>>()?;
            Ok(Val::List(spliced))
        }
        other => Ok(other),
    }
}

/// Replace bare links at one item position
fn splice_item_refs(value: Val, resolved: &HashMap<(FieldId, Key), Val>) -> Result<Val, ExecError> {
    match value {
        Val::Ref { field, parent } => lookup_ref(field, parent, resolved),
        Val::List(items) => {
            let spliced = items
                .into_iter()
                .map(|item| match item {
                    Val::Ref { field, parent } => lookup_ref(field, parent, resolved),
                    other => Ok(other),
                })
                .collect::<Result<_, _>>()?;
            Ok(Val::List(spliced))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use grove_schema::validated::{Field, FieldId, RelationId, StructId, ValidatedSchema};

    use super::*;

    use crate::{
        eval::{Env, ExecOptions, harness::*},
        pushdown::{PushdownPlan, SortKey},
        sqlite::SqliteBackend,
        storage::Storage,
        testdata,
        value::Scalar,
    };

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
    fn ref_links_comparison_unknown() {
        let user_struct = testdata::struct_id(&testdata::schema(), "User");
        let manager_field = testdata::field_id(&testdata::schema(), user_struct, "manager");
        let link = Val::Ref {
            field: manager_field,
            parent: Key(1),
        };
        assert_eq!(
            link.to_string(),
            format!("ref({}:{})", user_struct.index(), manager_field.local())
        );
        assert_eq!(link.eq(&int(1)), None);
        assert_eq!(link.eq(&link.clone()), None);
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
    fn optional_field_access_nested_optional_preserved() {
        assert_eq!(
            run_src("some({a=if true {none} else {some(1)}})?.a"),
            Ok(some(Val::None))
        );
        assert_eq!(run_src("some({a=some(1)})?.a"), Ok(some(some(int(1)))));
    }

    #[test]
    fn optional_tuple_and_method_preserve_nested_optional() {
        assert_eq!(
            run_src("some((if true {none} else {some(1)}, 2))?.0"),
            Ok(some(Val::None))
        );
        assert_eq!(run_src("some([1, 2])?.nth(5)"), Ok(some(Val::None)));
        assert_eq!(run_src("some([1, 2])?.nth(0)"), Ok(some(some(int(1)))));
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
    fn field_unowned_shallow_ref_resolution() {
        match run_seeded("users.manager") {
            Ok(Val::List(rows)) => match &rows[0] {
                Val::Some(user) => {
                    let Val::Record(fields) = user.as_ref() else {
                        panic!("expected manager record");
                    };
                    assert!(fields.iter().any(|(name, value)| {
                        name.as_ref() == "manager" && matches!(value, Val::Ref { .. })
                    }));
                }
                other => panic!("expected some(manager), got {other}"),
            },
            got => panic!("expected manager list, got {got:?}"),
        }
    }

    #[test]
    fn field_owned_shallow_ref_resolution() {
        match run_seeded("users.profile") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                match &rows[0] {
                    Val::Some(profile) => {
                        let Val::Record(profile) = profile.as_ref() else {
                            panic!("expected profile record");
                        };
                        assert!(profile.iter().any(|(name, value)| {
                            name.as_ref() == "user" && matches!(value, Val::Ref { .. })
                        }));
                        assert!(profile.iter().any(|(name, value)| {
                            name.as_ref() == "bio" && *value == str("AdaBio")
                        }));
                    }
                    other => panic!("expected some(profile), got {other}"),
                }
                assert!(matches!(&rows[1], Val::None));
            }
            got => panic!("expected profile list, got {got:?}"),
        }
    }

    #[test]
    fn field_optional_owning_resolved() {
        match run_seeded("users.profile") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                match &rows[0] {
                    Val::Some(profile) => {
                        let Val::Record(profile) = profile.as_ref() else {
                            panic!("expected profile record");
                        };
                        assert!(profile.iter().any(|(name, value)| {
                            name.as_ref() == "bio" && *value == str("AdaBio")
                        }));
                    }
                    other => panic!("expected some(profile), got {other}"),
                }
                assert!(matches!(&rows[1], Val::None));
            }
            got => panic!("expected profile list, got {got:?}"),
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
        let empty: Val = Val::List(Vec::new());
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
    fn field_access_ref_base_resolved() {
        match run_seeded("users { o = orders { n = user.name } }") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "o") {
                    Some((_, Val::List(orders))) => {
                        assert_eq!(orders.len(), 2);
                        for order in orders {
                            let Val::Record(order) = order else {
                                panic!("expected order record, got {order}");
                            };
                            assert!(order.iter().any(|(name, value)| {
                                name.as_ref() == "n" && *value == str("Ada")
                            }));
                        }
                    }
                    got => panic!("expected orders list, got {got:?}"),
                }
            }
            got => panic!("expected projected users, got {got:?}"),
        }
    }

    #[test]
    fn field_access_optional_ref_base_resolved() {
        match run_seeded("users { m = manager?.name }") {
            Ok(Val::List(rows)) => {
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                match first.iter().find(|(name, _)| name.as_ref() == "m") {
                    Some((_, Val::Some(name))) => {
                        assert!(matches!(name.as_ref(), Val::Scalar(_)));
                    }
                    got => panic!("expected some(name), got {got:?}"),
                }
                assert!(matches!(
                    &rows[1], Val::Record(second)
                      if matches!(second.first().unwrap().1, Val::None)
                ));
            }
            got => panic!("expected projected records, got {got:?}"),
        }
    }

    struct RecordingBackend<'a, 's> {
        inner: &'a SqliteBackend<'s>,
        recording: Recording,
    }

    #[derive(Default)]
    struct Recording {
        fetches: RefCell<Vec<(StructId, Vec<FieldId>)>>,
        constraints: RefCell<Vec<FetchConstraints>>,
        related_fetches: RefCell<Vec<(RelationId, bool, Vec<FieldId>)>>,
        related_constraints: RefCell<Vec<FetchConstraints>>,
        related_parent_counts: RefCell<Vec<usize>>,
    }

    impl<'a, 's> RecordingBackend<'a, 's> {
        pub fn new(backend: &'a SqliteBackend<'s>) -> RecordingBackend<'a, 's> {
            RecordingBackend {
                inner: backend,
                recording: Default::default(),
            }
        }
    }

    impl Storage for RecordingBackend<'_, '_> {
        fn fetch(
            &self,
            struct_id: StructId,
            fields: &[FieldId],
            constraints: &FetchConstraints,
        ) -> Result<Vec<Row>, ExecError> {
            self.recording
                .fetches
                .borrow_mut()
                .push((struct_id, fields.to_vec()));
            self.recording
                .constraints
                .borrow_mut()
                .push(constraints.clone());
            self.inner.fetch(struct_id, fields, constraints)
        }

        fn fetch_related(
            &self,
            rel: RelationId,
            fetch_from_first: bool,
            parents: &[Key],
            fields: &[FieldId],
            constraints: &FetchConstraints,
        ) -> Result<Vec<Vec<Row>>, ExecError> {
            self.recording.related_fetches.borrow_mut().push((
                rel,
                fetch_from_first,
                fields.to_vec(),
            ));
            self.recording
                .related_constraints
                .borrow_mut()
                .push(constraints.clone());
            self.recording
                .related_parent_counts
                .borrow_mut()
                .push(parents.len());
            self.inner
                .fetch_related(rel, fetch_from_first, parents, fields, constraints)
        }
    }

    fn run_seeded_recording(src: &str) -> Result<(Val, Recording), ExecError> {
        let schema = testdata::schema();
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        testdata::seed(&mut conn).expect("testdata initialization");
        let backend = SqliteBackend::new(conn, &schema).expect("backend");
        let recording = RecordingBackend::new(&backend);
        let result = run_with_storage_opts(src, &schema, &recording, ExecOptions::default())?;
        // run without recording for differential tests
        run_with_storage(src, &schema, &backend)?;
        Ok((result, recording.recording))
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
    fn root_fetch_constraints_reach_storage() {
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

    #[test]
    fn pushed_root_operations_correctness() {
        assert_eq!(
            run_seeded("users.sort_desc_over(name).take(2).name"),
            Ok(Val::List(vec![str("Eli"), str("Dan")]))
        );
        assert_eq!(
            run_seeded("users.skip(1).take(2).name"),
            Ok(Val::List(vec![str("Bob"), str("Cyd")]))
        );
        assert_eq!(
            run_seeded("users.take(2).sort_desc_over(name).name"),
            Ok(Val::List(vec![str("Bob"), str("Ada")]))
        );
        assert_eq!(
            run_seeded("users.sort_over(name).take(10).skip(2).take(2).name"),
            Ok(Val::List(vec![str("Cyd"), str("Dan")]))
        );
        assert_eq!(
            run_seeded("users[id > 2][orders.len() > 1].map($.name)"),
            Ok(Val::List(Vec::new()))
        );
        assert_eq!(run_seeded("users.first()?.name"), Ok(some(str("Ada"))));
        assert_eq!(run_seeded("users.nth(-1)?.name"), Ok(some(str("Eli"))));
        assert_eq!(
            run_seeded("users.take(10).nth(3)?.name"),
            Ok(some(str("Dan")))
        );
        assert_eq!(
            run_seeded("users.skip(1).nth(2)?.name"),
            Ok(some(str("Dan")))
        );
    }

    fn record_field_value<'a>(value: &'a Val, name: &str) -> &'a Val {
        let Val::Record(fields) = value else {
            panic!("expected record, got {value:?}");
        };
        fields
            .iter()
            .find(|(field_name, _)| field_name.as_ref() == name)
            .map(|(_, value)| value)
            .expect("expected record field")
    }

    #[test]
    fn deep_resolver_value_shape_traversal() {
        let schema = testdata::schema();
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        testdata::seed(&mut conn).expect("testdata initialization");
        let backend = SqliteBackend::new(conn, &schema).expect("backend");
        let plan = PushdownPlan::disabled(&schema);
        let mut ctx = EvalCtx::new(Env::new(), FIXED_NOW, &backend, &schema, &plan);

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
            Val::List(vec![Val::Tuple(vec![Val::Ref {
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
        let mut ctx = EvalCtx::new(Env::new(), FIXED_NOW, &backend, &schema, &plan);

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

    fn related_fields_for<'a>(
        schema: &ValidatedSchema,
        related: &'a [(RelationId, bool, Vec<FieldId>)],
        field: FieldId,
    ) -> Option<&'a [FieldId]> {
        let relation = schema.relation_of_field(field).expect("relation for field");
        related
            .iter()
            .find(|(rel, _, _)| *rel == relation)
            .map(|(_, _, fields)| fields.as_slice())
    }

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

    fn unpack_nested_i64s(value: &Val, field: &str) -> Vec<i64> {
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

    fn unpack_optional_i64(value: &Val) -> Option<i64> {
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
    fn owning_ident_chain_pushes_into_fetch_related() {
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
            unpack_nested_i64s(record_field_value(&rows[0], "o"), "amount"),
            [200]
        );
        assert_eq!(
            unpack_nested_i64s(record_field_value(&rows[1], "o"), "amount"),
            [400]
        );
    }

    #[test]
    fn non_owning_ident_chain_pushes_into_fetch_related() {
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
            unpack_optional_i64(record_field_value(&rows[0], "a")),
            Some(100)
        );
        assert_eq!(
            unpack_optional_i64(record_field_value(&rows[0], "b")),
            Some(200)
        );
        assert_eq!(
            unpack_optional_i64(record_field_value(&rows[1], "a")),
            Some(400)
        );
        assert_eq!(unpack_optional_i64(record_field_value(&rows[1], "b")), None);
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
            unpack_nested_i64s(record_field_value(first, "a"), "amount"),
            [100]
        );
        assert_eq!(record_field_value(first, "b"), &int(2));
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
        assert_eq!(result, Val::List(vec![rec(vec![("name", str("Bob"))])]));
    }

    #[test]
    fn first_record_projection_no_owning_refs_fetched() {
        let (_, recording) = run_seeded_recording("users.first()?.name").expect("query");
        assert!(recording.related_fetches.borrow().is_empty(),);
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

    #[test]
    fn field_trigger_owning_pushes_into_fetch_related() {
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
        assert_eq!(result, Val::List(vec![int(1), int(1)]));
    }

    #[test]
    fn field_trigger_non_owning_pushes_into_fetch_related() {
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
        assert_eq!(result, Val::List(vec![int(1), int(0)]));
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
            Val::List(vec![
                rec(vec![("name", str("Ada"))]),
                rec(vec![("name", str("Bob"))]),
            ])
        );
    }
}
