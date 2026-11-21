use std::collections::{HashMap, HashSet};

use grove_query::typecheck::types::{CollectionOriginKind, TypedExpr, TypedMutationStmt};
use grove_schema::validated::{
    Field, FieldId, Relation, RelationId, StructId, ValidatedSchema, ValueType,
};

use crate::error::ExecError;
use crate::pushdown::FetchConstraints;
use crate::storage::{Key, WriteTxn};
use crate::value::{CollectionOrigin, RecordVal, RowOrigin, Val, unwrap_optional_depth};

use super::{EvalCtx, eval};

type FieldValues = Vec<(FieldId, Val)>;
type RecordParts = (FieldValues, FieldValues);

/// Dispatch typed mutation statement to its executor
pub(crate) fn exec_mutation(
    stmt: &TypedMutationStmt,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    match stmt {
        TypedMutationStmt::Insert {
            base,
            arg,
            opt_depth,
            ..
        } => exec_insert(base, arg, *opt_depth, ctx, write),
        TypedMutationStmt::Delete {
            base, opt_depth, ..
        } => exec_delete(base, *opt_depth, ctx, write),
        TypedMutationStmt::Update {
            base,
            arg,
            opt_depth,
            ..
        } => exec_update(base, arg, *opt_depth, ctx, write),
    }
}

fn exec_insert(
    base: &TypedExpr,
    arg: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(target) = resolve_insert_target(base, opt_depth, ctx)? else {
        return Ok(());
    };
    let arg = eval(arg, ctx)?;
    let records = collect_insert_records(arg)?;
    insert_group(ctx.schema, target, records, write)
}

enum InsertTarget {
    Root {
        struct_id: StructId,
    },
    Ref {
        struct_id: StructId,
        relation: RelationId,
        parent: Key,
        parent_is_first: bool,
    },
}

impl InsertTarget {
    fn struct_id(&self) -> StructId {
        match self {
            InsertTarget::Root { struct_id } | InsertTarget::Ref { struct_id, .. } => *struct_id,
        }
    }
}

/// Resolve an insert target to a root or parented collection
fn resolve_insert_target(
    base: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
) -> Result<Option<InsertTarget>, ExecError> {
    let Some(origin) = base.provenance.collection else {
        return Err(ExecError::Unsupported {
            message: "insert target has no collection origin".to_string(),
        });
    };
    match origin {
        CollectionOriginKind::Root(root_idx) => {
            let root = ctx
                .schema
                .roots
                .get(root_idx)
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("unknown root {root_idx} (internal error)"),
                })?;
            if opt_depth > 0 {
                let target = eval(base, ctx)?;
                let (_, peeled) = unwrap_optional_depth(target, opt_depth)?;
                if peeled < opt_depth {
                    return Ok(None);
                }
            }
            Ok(Some(InsertTarget::Root {
                struct_id: root.struct_id,
            }))
        }
        CollectionOriginKind::Ref(field_id) => {
            let target = eval(base, ctx)?;
            let target = if opt_depth > 0 {
                let (target, peeled) = unwrap_optional_depth(target, opt_depth)?;
                if peeled < opt_depth {
                    return Ok(None);
                }
                target
            } else {
                target
            };
            let Val::List(list) = target else {
                return Err(ExecError::Integrity {
                    message: "nested insert target did not evaluate to a list".to_string(),
                });
            };
            let Some(CollectionOrigin::Ref { field, parent }) = list.origin else {
                return Err(ExecError::Integrity {
                    message: "nested insert target has no ref collection origin".to_string(),
                });
            };
            if field != field_id {
                return Err(ExecError::Integrity {
                    message: "nested insert origin field mismatch (internal error)".to_string(),
                });
            }
            let Some(Field::Ref {
                target: struct_id,
                is_list: true,
                owning: true,
                ..
            }) = ctx.schema.field(field)
            else {
                return Err(ExecError::Unsupported {
                    message: "insert through unsupported reference (internal error)".to_string(),
                });
            };
            let relation =
                ctx.schema
                    .relation_of_field(field)
                    .ok_or_else(|| ExecError::Integrity {
                        message: "insert target Ref has no relation (internal error)".to_string(),
                    })?;
            let parent_is_first = ctx.schema.relations[relation.index()].endpoints()[0] == field;
            Ok(Some(InsertTarget::Ref {
                struct_id: *struct_id,
                relation,
                parent: parent.key,
                parent_is_first,
            }))
        }
    }
}

/// Insert base records, then recursively insert owned child refs
fn insert_group(
    schema: &ValidatedSchema,
    target: InsertTarget,
    records: Vec<RecordVal>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    if records.is_empty() {
        return Ok(());
    }
    let struct_id = target.struct_id();
    let mut values = Vec::with_capacity(records.len());
    let mut owned = Vec::with_capacity(records.len());
    for record in records {
        let (base, children) = record_parts(schema, struct_id, record)?;
        values.push(base);
        owned.push(children);
    }

    let keys = match &target {
        InsertTarget::Root { .. } => write.insert_rows(struct_id, &values)?,
        InsertTarget::Ref {
            relation,
            parent,
            parent_is_first,
            ..
        } => write.insert_rows_with_parent(
            struct_id,
            &values,
            *relation,
            *parent,
            *parent_is_first,
        )?,
    };
    if keys.len() != values.len() {
        return Err(ExecError::Integrity {
            message: "insert returned an unexpected number of row keys".to_string(),
        });
    }

    for (children, key) in owned.into_iter().zip(keys) {
        for (field_id, value) in children {
            insert_owned_field(schema, field_id, value, key, write)?;
        }
    }
    Ok(())
}

/// Normalize an insert argument value into a list of records
fn collect_insert_records(value: Val) -> Result<Vec<RecordVal>, ExecError> {
    match value {
        Val::Record(record) => Ok(vec![record]),
        Val::List(list) => list.into_iter().map(record_from_value).collect(),
        other => Err(ExecError::Integrity {
            message: format!(
                "insert argument is not a record or list of records: {other} (internal error)"
            ),
        }),
    }
}

fn record_from_value(value: Val) -> Result<RecordVal, ExecError> {
    match value {
        Val::Record(record) => Ok(record),
        other => Err(ExecError::Integrity {
            message: format!("expected record value, got {other} (internal error)"),
        }),
    }
}

/// Split a record into base column values and owned ref values
fn record_parts(
    schema: &ValidatedSchema,
    struct_id: StructId,
    mut record: RecordVal,
) -> Result<RecordParts, ExecError> {
    let struct_def = schema
        .structs
        .get(struct_id.index())
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown struct id {} (internal error)", struct_id.index()),
        })?;
    let mut values = Vec::new();
    let mut owned = Vec::new();
    for (local, field) in struct_def.fields.iter().enumerate() {
        let field_id = FieldId::new(struct_id, local);
        let index = record
            .iter()
            .position(|(field_name, _)| field_name.as_ref() == field.name());
        match field {
            Field::Ref {
                owning: true,
                optional,
                ..
            } => {
                if let Some(index) = index {
                    let value = record.fields.swap_remove(index).1;
                    if !matches!(value, Val::None) {
                        owned.push((field_id, value));
                    }
                } else if !*optional {
                    return Err(ExecError::Integrity {
                        message: format!(
                            "missing insert field `{}` (internal error)",
                            field.name()
                        ),
                    });
                }
            }
            Field::Ref { owning: false, .. } => {
                if index.is_some() {
                    return Err(ExecError::Integrity {
                        message: format!(
                            "non-owning ref `{}` cannot be inserted (internal error)",
                            field.name()
                        ),
                    });
                }
            }
            Field::Value { ty, .. } | Field::Array { element: ty, .. } => {
                if let Some(index) = index {
                    values.push((field_id, record.fields.swap_remove(index).1));
                } else if matches!(ty, ValueType::Optional(_)) {
                    values.push((field_id, Val::None));
                } else {
                    return Err(ExecError::Integrity {
                        message: format!(
                            "missing insert field `{}` (internal error)",
                            field.name()
                        ),
                    });
                }
            }
        }
    }
    Ok((values, owned))
}

/// Insert one owning ref field's records beneath a parent row
fn insert_owned_field(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
    parent: Key,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(Field::Ref {
        target,
        optional,
        is_list,
        owning,
        ..
    }) = schema.field(field_id)
    else {
        return Err(ExecError::Integrity {
            message: "owned insert field is not a ref (internal error)".to_string(),
        });
    };
    if !*owning {
        return Err(ExecError::Integrity {
            message: "attempted recursive insert through non-owning ref (internal error)"
                .to_string(),
        });
    }
    let records = if *is_list {
        let Val::List(list) = value else {
            return Err(ExecError::Integrity {
                message: "list ref field did not contain a list (internal error)".to_string(),
            });
        };
        list.into_iter()
            .map(record_from_value)
            .collect::<Result<Vec<_>, _>>()?
    } else if *optional {
        match value {
            Val::None => Vec::new(),
            Val::Some(value) => vec![record_from_value(*value)?],
            other => {
                return Err(ExecError::Integrity {
                    message: format!("optional ref field contained {other} (internal error)"),
                });
            }
        }
    } else {
        vec![record_from_value(value)?]
    };
    if records.is_empty() {
        return Ok(());
    }
    let relation = schema
        .relation_of_field(field_id)
        .ok_or_else(|| ExecError::Integrity {
            message: "owned insert field has no relation (internal error)".to_string(),
        })?;
    let parent_is_first = schema.relations[relation.index()].endpoints()[0] == field_id;
    insert_group(
        schema,
        InsertTarget::Ref {
            struct_id: *target,
            relation,
            parent,
            parent_is_first,
        },
        records,
        write,
    )
}

fn exec_update(
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

struct UpdateParts {
    /// ids and new values of modified base and array fields
    base: FieldValues,
    /// ids of modified array fields (values in `base`)
    arrays: Vec<FieldId>,
}

/// Apply a prepared update after all argument values have been computed
fn apply_update(
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
    origin: RowOrigin,
    parts: UpdateParts,
) -> Result<(), ExecError> {
    for field_id in &parts.arrays {
        write.delete_array_field(*field_id, &[origin.key])?;
    }
    if !parts.base.is_empty() {
        write.update_row(origin.struct_id, origin.key, &parts.base)?;
    }
    Ok(())
}

/// Extract unique target row origins paired with their current record values
fn collect_update_target_records(value: Val) -> Result<Vec<(RowOrigin, Val)>, ExecError> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    match value {
        Val::Record(record) => {
            let origin = record.origin.ok_or_else(|| ExecError::Integrity {
                message: "update target record has no row origin (internal error)".to_string(),
            })?;
            if seen.insert(origin) {
                targets.push((origin, Val::Record(record)));
            }
        }
        Val::List(list) => {
            for item in list {
                let Val::Record(record) = item else {
                    return Err(ExecError::Integrity {
                        message: "update target list contains non-record value (internal error)"
                            .to_string(),
                    });
                };
                let origin = record.origin.ok_or_else(|| ExecError::Integrity {
                    message: "update target record has no row origin (internal error)".to_string(),
                })?;
                if seen.insert(origin) {
                    targets.push((origin, Val::Record(record)));
                }
            }
        }
        other => {
            return Err(ExecError::Integrity {
                message: format!(
                    "update target is not a record or list of records: {other} (internal error)"
                ),
            });
        }
    }
    Ok(targets)
}

/// Convert a constructed update record into only the fields it supplies
fn resolve_update_parts(
    schema: &ValidatedSchema,
    struct_id: StructId,
    mut record: RecordVal,
) -> Result<UpdateParts, ExecError> {
    let struct_def = schema
        .structs
        .get(struct_id.index())
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown struct id {} (internal error)", struct_id.index()),
        })?;
    let mut base = Vec::new();
    let mut arrays = Vec::new();
    for (local, field) in struct_def.fields.iter().enumerate() {
        let field_id = FieldId::new(struct_id, local);
        let Some(index) = record
            .iter()
            .position(|(field_name, _)| field_name.as_ref() == field.name())
        else {
            continue;
        };
        let value = record.fields.swap_remove(index).1;
        match field {
            Field::Value { ty, .. } if ty.has_array() => {
                arrays.push(field_id);
                base.push((field_id, value));
            }
            Field::Value { .. } => base.push((field_id, value)),
            Field::Array { .. } => {
                arrays.push(field_id);
                base.push((field_id, value));
            }
            Field::Ref { .. } => {
                return Err(ExecError::Unsupported {
                    message: "updating owning ref fields is not yet implemented".to_string(),
                });
            }
        }
    }
    Ok(UpdateParts {
        base,
        arrays,
    })
}

fn exec_delete(
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
    let mut seen = HashSet::new();
    match value {
        Val::Record(record) => {
            seen.insert(record.origin.ok_or_else(|| ExecError::Integrity {
                message: "delete target record has no row origin (internal error)".to_string(),
            })?);
        }
        Val::List(list) => {
            for item in list {
                match item {
                    Val::Record(record) => {
                        seen.insert(record.origin.ok_or_else(|| {
                            ExecError::Integrity {
                                message: "delete target record has no row origin (internal error)"
                                    .to_string(),
                            }
                        })?);
                    }
                    other => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "delete target list contains non-record value {other} (internal error)"
                            ),
                        });
                    }
                }
            }
        }
        other => {
            return Err(ExecError::Integrity {
                message: format!(
                    "delete target is not a record or list of records: {other} (internal error)"
                ),
            });
        }
    }
    Ok(seen)
}

/// Delete a batch of rows and recursively cascade their owning refs
fn delete_owned_cascade(
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
            .ok_or_else(|| ExecError::Integrity {
                message: format!("unknown struct id {} (internal error)", struct_id.index()),
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
        let relation =
            ctx.schema
                .relation_of_field(field_id)
                .ok_or_else(|| ExecError::Integrity {
                    message: "owning delete field has no relation (internal error)".to_string(),
                })?;
        let fetch_from_first = ctx.schema.relations[relation.index()].endpoints()[0] == field_id;
        let groups = ctx.storage.fetch_related(
            relation,
            fetch_from_first,
            &keys,
            &[],
            &FetchConstraints::EMPTY,
        )?;
        let mut child_keys = Vec::new();
        for group in groups {
            for row in group {
                let origin = RowOrigin {
                    struct_id: target,
                    key: row.rowid,
                };
                if seen.insert(origin) {
                    child_keys.push(row.rowid);
                }
            }
        }
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
                if pk.table_id() == table && !relation_has_owning_endpoint(schema, relation) {
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

fn relation_has_owning_endpoint(schema: &ValidatedSchema, relation: &Relation) -> bool {
    relation
        .endpoints()
        .iter()
        .any(|field| matches!(schema.field(*field), Some(Field::Ref { owning: true, .. })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::*;
    use crate::eval::{ExecOptions, exec_file_with_options};
    use crate::pushdown::FetchConstraints;
    use crate::sqlite::SqliteBackend;
    use crate::storage::Storage;
    use crate::value::Scalar;
    use chrono::Utc;
    use grove_query::typecheck::types::TypedQueryFile;
    use rusqlite::Connection;

    fn test_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root items: Item;
                enum Kind { A => 1, B => 2 }
                struct Item {
                    id: Int,
                    name: String,
                    active: Bool,
                    note: ?String,
                    pos: (Float, Float)@(x, y),
                    kind: Kind,
                    maybe_kind: ?Kind,
                }
                "#,
            )
            .0
            .expect("invalid mutation test schema"),
        )
        .0
        .expect("invalid mutation test schema")
    }

    fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items(id INTEGER, name TEXT, active INTEGER, note TEXT,
             x REAL, y REAL, kind INTEGER, maybe_kind INTEGER)",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    fn typed(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query(source);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "{diags:?}");
        typed.unwrap()
    }

    fn run(source: &str, schema: &ValidatedSchema, backend: &SqliteBackend<'_>) -> Option<Val> {
        run_result(source, schema, backend).unwrap()
    }

    fn run_result(
        source: &str,
        schema: &ValidatedSchema,
        backend: &SqliteBackend<'_>,
    ) -> Result<Option<Val>, ExecError> {
        let file = typed(source, schema);
        exec_file_with_options(&file, backend, schema, Utc::now(), ExecOptions::default())
    }

    fn fields(schema: &ValidatedSchema) -> (StructId, Vec<FieldId>) {
        let item = schema.roots[0].struct_id;
        let fields = ["id", "name", "active", "note", "pos", "kind", "maybe_kind"]
            .iter()
            .map(|name| schema.struct_field(item, name).unwrap().0)
            .collect();
        (item, fields)
    }

    #[test]
    fn insert_root_record_scalar_enum_tuple_optional() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, fields) = fields(&schema);
        let kind = schema.enum_by_name("Kind").unwrap().0;

        run(
            r#"items.insert({
                id = 1,
                name = "alpha",
                active = true,
                note = some("hello"),
                pos = (1.5f, -2.5f),
                kind = Kind::B,
                maybe_kind = some(Kind::A)
            }); 0"#,
            &schema,
            &backend,
        );

        let rows = backend
            .fetch(item, &fields, &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                str("alpha"),
                bool(true),
                some(str("hello")),
                Val::Tuple(vec![float(1.5), float(-2.5),]),
                Val::Enum(kind, 1),
                some(Val::Enum(kind, 0)),
            ]
        );
    }

    #[test]
    fn insert_root_bulk_list() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, fields) = fields(&schema);

        run(
            r#"items.insert([
                { id = 1, name = "alpha", active = true, pos = (1.0f, 2.0f), kind = Kind::A },
                { id = 2, name = "beta", active = false, pos = (3.0f, 4.0f), kind = Kind::B },
            ]);"#,
            &schema,
            &backend,
        );

        let rows = backend
            .fetch(item, &fields, &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values[0], Val::Scalar(Scalar::Int(1)));
        assert_eq!(rows[1].values[0], Val::Scalar(Scalar::Int(2)));
        assert_eq!(rows[0].values[6], Val::None);
        assert_eq!(
            rows[1].values[1],
            Val::Scalar(Scalar::String("beta".to_string()))
        );
    }

    #[test]
    fn insert_filtered_root_and_same_turn_read() {
        let schema = test_schema();
        let backend = test_backend(&schema);

        let result = run(
            r#"items[id > 0].insert({
                id = 7,
                name = "gamma",
                active = false,
                pos = (0.0f, 0.0f),
                kind = Kind::A
            }); items.len()"#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(Val::Scalar(Scalar::Int(1))));
    }

    fn ref_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root users: User;
                struct User {
                    id: Int,
                    name: String,
                    orders: List<Order>,
                    profile: ?Profile,
                }
                struct Order {
                    id: Int,
                    total: Int,
                    user: &User,
                }
                struct Profile {
                    bio: String,
                    user: &User,
                }
                rel Order.user <<-> User.orders (orders.user_id -> users.id);
                rel Profile.user <-> User.profile (profiles.user_id -> users.id);
                "#,
            )
            .0
            .expect("invalid ref mutation schema"),
        )
        .0
        .expect("invalid ref mutation schema")
    }

    fn ref_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE profiles(bio TEXT, user_id INTEGER);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    #[test]
    fn insert_nested_owning_collection_target() {
        let schema = ref_schema();
        let backend = ref_backend(&schema);

        let result = run(
            r#"
            users.insert({ id = 1, name = "A", orders = [] });
            users.first().unwrap().orders.insert({ id = 10, total = 100 });
            users.first().unwrap().orders { id, total }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(Val::list(vec![rec(vec![
                ("id", int(10)),
                ("total", int(100)),
            ])]))
        );
    }

    #[test]
    fn insert_recursive_owning_children() {
        let schema = ref_schema();
        let backend = ref_backend(&schema);

        let result = run(
            r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [
                    { id = 10, total = 100 },
                    { id = 11, total = 110 },
                ],
            });
            users.first().unwrap().orders { id, total }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(Val::list(vec![
                rec(vec![("id", int(10)), ("total", int(100))]),
                rec(vec![("id", int(11)), ("total", int(110))]),
            ]))
        );
    }

    #[test]
    fn insert_optional_single_owning_child() {
        let schema = ref_schema();
        let backend = ref_backend(&schema);

        let result = run(
            r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [],
                profile = some({ bio = "bio" }),
            });
            users.first().unwrap().profile?.bio
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(some(str("bio"))));
    }

    #[test]
    fn insert_bulk_recursive_children_per_parent() {
        let schema = ref_schema();
        let backend = ref_backend(&schema);

        let result = run(
            r#"
            users.insert([
                { id = 1, name = "A", orders = [{ id = 10, total = 1 }] },
                {
                    id = 2,
                    name = "B",
                    orders = [
                        { id = 20, total = 2 },
                        { id = 21, total = 3 },
                    ],
                },
                { id = 3, name = "C", orders = [] },
            ]);
            users.map($.orders.len())
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(Val::list(vec![int(1), int(2), int(0)])));
    }

    fn array_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root rows: Row;
                struct Row {
                    id: Int,
                    tags: List<String>@tags_ref via row_tags[row_key, tag],
                    nums: List<Int>@nums_ref via row_nums[row_key, num],
                    matrix: List<List<Int> via inner_nums[inner_key, val]>@matrix_ref
                        via outer_nums[outer_key, inner_key],
                    extra: (Int, List<String> via extra_tags[ekey, eval])@(eid, elink),
                }
                "#,
            )
            .0
            .expect("invalid array mutation schema"),
        )
        .0
        .expect("invalid array mutation schema")
    }

    fn array_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE rows(id INTEGER, tags_ref INTEGER, nums_ref INTEGER,
                 matrix_ref INTEGER, eid INTEGER, elink INTEGER);
             CREATE TABLE row_tags(row_key INTEGER, tag TEXT);
             CREATE TABLE row_nums(row_key INTEGER, num INTEGER);
             CREATE TABLE outer_nums(outer_key INTEGER, inner_key INTEGER);
             CREATE TABLE inner_nums(inner_key INTEGER, val INTEGER);
             CREATE TABLE extra_tags(ekey INTEGER, eval TEXT);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    #[test]
    fn insert_array_fields_and_nested_value_arrays() {
        let schema = array_schema();
        let backend = array_backend(&schema);

        let result = run(
            r#"
            rows.insert({
                id = 1,
                tags = ["a", "b"],
                nums = [1, 2, 3],
                matrix = [[1, 2], [3]],
                extra = (7, ["x", "y"]),
            });
            rows.first().unwrap() { tags, nums, matrix, extra }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(rec(vec![
                ("tags", Val::list(vec![str("a"), str("b")]),),
                ("nums", Val::list(vec![int(1), int(2), int(3)])),
                (
                    "matrix",
                    Val::list(vec![
                        Val::list(vec![int(1), int(2)]),
                        Val::list(vec![int(3)]),
                    ]),
                ),
                (
                    "extra",
                    Val::Tuple(vec![int(7), Val::list(vec![str("x"), str("y")])]),
                ),
            ]))
        );
    }

    #[test]
    fn insert_empty_and_multiple_array_rows() {
        let schema = array_schema();
        let backend = array_backend(&schema);

        let result = run(
            r#"
            rows.insert([
                { id = 1, tags = [], nums = [1], matrix = [], extra = (1, []) },
                { id = 2, tags = ["z"], nums = [2, 3], matrix = [[4]], extra = (2, ["q"]) },
            ]);
            rows { id, tags, nums, matrix, extra }
            "#,
            &schema,
            &backend,
        );

        let Some(Val::List(rows)) = result else {
            panic!("expected rows list");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            rec(vec![
                ("id", int(1)),
                ("tags", Val::list(vec![])),
                ("nums", Val::list(vec![int(1)])),
                ("matrix", Val::list(vec![])),
                ("extra", Val::Tuple(vec![int(1), Val::list(vec![])])),
            ])
        );
        assert_eq!(
            rows[1],
            rec(vec![
                ("id", int(2)),
                ("tags", Val::list(vec![str("z")])),
                ("nums", Val::list(vec![int(2), int(3)])),
                ("matrix", Val::list(vec![Val::list(vec![int(4)])])),
                ("extra", Val::Tuple(vec![int(2), Val::list(vec![str("q")])]),),
            ])
        );
    }

    #[test]
    fn insert_optional_none_target_noop() {
        let schema = ref_schema();
        let backend = ref_backend(&schema);

        let result = run(
            r#"
            users.first()?.orders?.insert({ id = 1, total = 1 });
            users.len()
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(int(0)));
    }

    #[test]
    fn insert_nested_target_missing_parent_key_error() {
        let schema = ref_schema();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE profiles(bio TEXT, user_id INTEGER);
             INSERT INTO users(id, name) VALUES (NULL, 'A');",
        )
        .unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();

        let err = run_result(
            "users.first()?.orders?.insert({ id = 1, total = 1 }); 0",
            &schema,
            &backend,
        )
        .unwrap_err();
        let ExecError::Integrity { message } = err else {
            panic!("expected integrity error, got {err:?}");
        };
        assert!(message.contains("no join key"), "{message}");
    }

    fn cascade_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root users: User;
                struct User {
                    id: Int,
                    name: String,
                    orders: List<Order>,
                }
                struct Order {
                    id: Int,
                    total: Int,
                    user: &User,
                    items: List<Item>,
                }
                struct Item {
                    id: Int,
                    order: &Order,
                }
                rel Order.user <<-> User.orders (orders.user_id -> users.id);
                rel Item.order <<-> Order.items (items.order_id -> orders.id);
                "#,
            )
            .0
            .expect("invalid cascade schema"),
        )
        .0
        .expect("invalid cascade schema")
    }

    fn cascade_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE orders(id INTEGER, total INTEGER, user_id INTEGER);
             CREATE TABLE items(id INTEGER, order_id INTEGER);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

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
    fn delete_record_target() {
        let schema = cascade_schema();
        let backend = cascade_backend(&schema);
        let user = schema.roots[0].struct_id;
        let order = schema.struct_by_name("Order").unwrap().0;

        let result = run(
            r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [{ id = 10, total = 1, items = [] }],
            });
            users.find(name == "A").unwrap().delete();
            users.len()
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(int(0)));
        assert!(
            backend
                .fetch(user, &[], &FetchConstraints::EMPTY)
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .fetch(order, &[], &FetchConstraints::EMPTY)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delete_duplicate_targets_deduped() {
        let schema = cascade_schema();
        let backend = cascade_backend(&schema);
        let user = schema.roots[0].struct_id;

        let result = run(
            r#"
            users.insert({
                id = 1,
                name = "A",
                orders = [{ id = 10, total = 1, items = [{ id = 100 }] }],
            });
            users.extend(users).delete();
            users.len()
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(int(0)));
        assert!(
            backend
                .fetch(user, &[], &FetchConstraints::EMPTY)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delete_optional_none_target_noop() {
        let schema = cascade_schema();
        let backend = cascade_backend(&schema);

        let result = run(
            r#"
            users.first()?.delete();
            users.len()
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(int(0)));
    }

    fn one_to_many_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root parents: Parent;
                root children: Child;
                struct Parent {
                    id: Int,
                    name: String,
                    children: &List<Child>,
                }
                struct Child {
                    id: Int,
                    name: String,
                    parent: &?Parent,
                }
                rel Parent.children <->> Child.parent (children.parent_id -> parents.id);
                "#,
            )
            .0
            .expect("invalid one-to-many schema"),
        )
        .0
        .expect("invalid one-to-many schema")
    }

    fn one_to_many_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE parents(id INTEGER, name TEXT);
             CREATE TABLE children(id INTEGER, name TEXT, parent_id INTEGER);
             INSERT INTO parents(id, name) VALUES (1, 'parent');
             INSERT INTO children(id, name, parent_id) VALUES (10, 'child', 1);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
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

    fn self_ref_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root users: User;
                struct User {
                    id: Int,
                    name: String,
                    manager: &?User,
                    subordinates: &List<User>,
                }
                rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
                "#,
            )
            .0
            .expect("invalid self-ref schema"),
        )
        .0
        .expect("invalid self-ref schema")
    }

    fn self_ref_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT, manager_id INTEGER);
             INSERT INTO users(id, name, manager_id) VALUES
                 (1, 'manager', NULL),
                 (2, 'report', 1);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
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

    fn many_to_many_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root users: User;
                root roles: Role;
                struct User {
                    id: Int,
                    name: String,
                    roles: &List<Role>,
                }
                struct Role {
                    id: Int,
                    name: String,
                    users: &List<User>,
                }
                rel Role.users <<->> User.roles
                    via user_roles[role_id -> roles.id, user_id -> users.id];
                "#,
            )
            .0
            .expect("invalid many-to-many schema"),
        )
        .0
        .expect("invalid many-to-many schema")
    }

    fn many_to_many_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER, name TEXT);
             CREATE TABLE roles(id INTEGER, name TEXT);
             CREATE TABLE user_roles(role_id INTEGER, user_id INTEGER);
             INSERT INTO users(id, name) VALUES (1, 'u1'), (2, 'u2');
             INSERT INTO roles(id, name) VALUES (1, 'r1'), (2, 'r2');
             INSERT INTO user_roles(role_id, user_id) VALUES (1, 1), (1, 2), (2, 1);",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
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

    #[test]
    fn update_record_fields_with_prev() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let kind = schema.enum_by_name("Kind").unwrap().0;

        let result = run(
            r#"
            items.insert({
                id = 1,
                name = "a",
                active = true,
                note = some("old"),
                pos = (1.0f, 2.0f),
                kind = Kind::A,
                maybe_kind = some(Kind::B),
            });
            items.first().unwrap().update({
                name = prev.name.concat("!"),
                note = none,
                pos = (3.0f, 4.0f),
                kind = Kind::B,
                maybe_kind = none,
            });
            items.first().unwrap() { id, name, active, note, pos, kind, maybe_kind }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(rec(vec![
                ("id", int(1)),
                ("name", str("a!")),
                ("active", bool(true)),
                ("note", Val::None),
                ("pos", Val::Tuple(vec![float(3.0), float(4.0)]),),
                ("kind", Val::Enum(kind, 1)),
                ("maybe_kind", Val::None),
            ]))
        );
    }

    #[test]
    fn update_argument_values_precomputed() {
        let schema = test_schema();
        let backend = test_backend(&schema);

        let result = run(
            r#"
            items.insert([
                { id = 1, name = "a", active = true, pos = (0.0f, 0.0f), kind = Kind::A },
                { id = 2, name = "b", active = true, pos = (0.0f, 0.0f), kind = Kind::A },
            ]);
            items.update({
                name = items.first().unwrap().name.concat("x"),
            });
            items { name }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(Val::list(vec![
                rec(vec![("name", str("ax"))]),
                rec(vec![("name", str("ax"))]),
            ]))
        );
    }

    #[test]
    fn update_optional_none_target_noop() {
        let schema = test_schema();
        let backend = test_backend(&schema);

        let result = run(
            r#"
            items.first()?.update({ name = "x" });
            items.len()
            "#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(int(0)));
    }

    #[test]
    fn update_replace_array_fields() {
        let schema = array_schema();
        let backend = array_backend(&schema);

        let result = run(
            r#"
            rows.insert({
                id = 1,
                tags = ["a", "b"],
                nums = [1, 2],
                matrix = [[1], [2, 3]],
                extra = (7, ["x"]),
            });
            rows.first().unwrap().update({
                tags = ["c"],
                nums = [9],
                matrix = [[5, 6]],
                extra = (8, ["z"]),
            });
            rows.first().unwrap() { tags, nums, matrix, extra }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(rec(vec![
                ("tags", Val::list(vec![str("c")])),
                ("nums", Val::list(vec![int(9)])),
                ("matrix", Val::list(vec![Val::list(vec![int(5), int(6)])]),),
                ("extra", Val::Tuple(vec![int(8), Val::list(vec![str("z")])]),),
            ]))
        );
    }

    #[test]
    fn update_empty_and_duplicate_targets() {
        let schema = test_schema();
        let backend = test_backend(&schema);

        let result = run(
            r#"
            items.update({ name = "x" });
            items.insert({ id = 1, name = "a", active = true, pos = (0.0f, 0.0f), kind = Kind::A });
            items.extend(items).update({ name = prev.name.concat("!") });
            items { name }
            "#,
            &schema,
            &backend,
        );

        assert_eq!(
            result,
            Some(Val::list(vec![rec(vec![("name", str("a!"))])]))
        );
    }
}
