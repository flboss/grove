use std::collections::HashSet;

use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema, ValueType};

use super::links::link_targets;
use crate::error::ExecError;
use crate::value::{RecordVal, RowOrigin, Val};

pub(super) fn collect_insert_records(value: Val) -> Result<Vec<RecordVal>, ExecError> {
    records_from_value(value, "insert argument")
}

pub(super) fn records_from_value(value: Val, context: &str) -> Result<Vec<RecordVal>, ExecError> {
    match value {
        Val::Record(record) => Ok(vec![record]),
        Val::List(list) => list.into_iter().map(record_from_value).collect(),
        other => Err(ExecError::Internal {
            message: format!("{context} is not a record or list of records: {other}"),
        }),
    }
}

pub(super) fn record_from_value(value: Val) -> Result<RecordVal, ExecError> {
    match value {
        Val::Record(record) => Ok(record),
        other => Err(ExecError::Internal {
            message: format!("expected record value, got {other}"),
        }),
    }
}

pub(super) struct RecordParts {
    pub(super) base: Vec<(FieldId, Val)>,
    pub(super) owned: Vec<(FieldId, Val)>,
    pub(super) links: Vec<(FieldId, Vec<RowOrigin>)>,
}

pub(super) enum SplitMode {
    Insert,
    Update,
}

pub(super) struct SplitParts {
    pub(super) base: Vec<(FieldId, Val)>,
    pub(super) arrays: Vec<FieldId>,
    pub(super) owned: Vec<(FieldId, Val)>,
    pub(super) links: Vec<(FieldId, Vec<RowOrigin>)>,
}

/// Split a record into base values, array fields, owned ref values, and non-owning links
pub(super) fn split_record_fields(
    schema: &ValidatedSchema,
    struct_id: StructId,
    mut record: RecordVal,
    mode: SplitMode,
) -> Result<SplitParts, ExecError> {
    let require_fields = matches!(mode, SplitMode::Insert);
    let struct_def = schema
        .structs
        .get(struct_id.index())
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown struct id {}", struct_id.index()),
        })?;
    let mut base = Vec::new();
    let mut arrays = Vec::new();
    let mut owned = Vec::new();
    let mut links = Vec::new();
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
                    if !require_fields || !matches!(value, Val::None) {
                        owned.push((field_id, value));
                    }
                } else if require_fields && !*optional {
                    return Err(ExecError::Internal {
                        message: format!("missing insert field `{}`", field.name()),
                    });
                }
            }
            Field::Ref { owning: false, .. } => {
                if let Some(index) = index {
                    let value = record.fields.swap_remove(index).1;
                    links.push((field_id, link_targets(schema, field_id, value)?));
                }
            }
            Field::Value { ty, .. } | Field::Array { element: ty, .. } => {
                let is_array = matches!(field, Field::Array { .. }) || ty.has_array();
                if let Some(index) = index {
                    let value = record.fields.swap_remove(index).1;
                    if is_array {
                        arrays.push(field_id);
                    }
                    base.push((field_id, value));
                } else if require_fields {
                    if matches!(ty, ValueType::Optional(_)) {
                        base.push((field_id, Val::None));
                    } else {
                        return Err(ExecError::Internal {
                            message: format!("missing insert field `{}`", field.name()),
                        });
                    }
                }
            }
        }
    }
    Ok(SplitParts {
        base,
        arrays,
        owned,
        links,
    })
}

pub(super) fn record_parts(
    schema: &ValidatedSchema,
    struct_id: StructId,
    record: RecordVal,
) -> Result<RecordParts, ExecError> {
    let parts = split_record_fields(schema, struct_id, record, SplitMode::Insert)?;
    Ok(RecordParts {
        base: parts.base,
        owned: parts.owned,
        links: parts.links,
    })
}
pub(super) fn collect_update_target_records(
    value: Val,
) -> Result<Vec<(RowOrigin, Val)>, ExecError> {
    let records = records_from_value(value, "update target")?;
    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(records.len());
    for record in records {
        let origin = record.origin.ok_or_else(|| ExecError::Internal {
            message: "update target record has no row origin".to_string(),
        })?;
        if seen.insert(origin) {
            targets.push((origin, Val::Record(record)));
        }
    }
    Ok(targets)
}
