use std::collections::HashMap;

use grove_schema::validated::{
    Field, FieldId, RelationId, ScalarType, StructId, ValidatedSchema, ValueType,
};
use rusqlite::Connection;
use rusqlite::config::DbConfig;
use rusqlite::params_from_iter;
use rusqlite::types::{Value, ValueRef};

use crate::codec;
use crate::error::ExecError;
use crate::storage::{Key, Row, Storage};
use crate::value::{Scalar, Val};

pub struct SqliteBackend {
    conn: Connection,
    schema: ValidatedSchema,
}

impl SqliteBackend {
    pub fn new(conn: Connection, schema: ValidatedSchema) -> Result<Self, ExecError> {
        for option in [
            // Disable quoted identifiers degrading to string literals in some cases
            DbConfig::SQLITE_DBCONFIG_DQS_DML,
            DbConfig::SQLITE_DBCONFIG_DQS_DDL,
        ] {
            conn.set_db_config(option, false)
                .map_err(|err| Self::storage_error("disable double-quoted literals", err))?;
        }
        Ok(SqliteBackend { conn, schema })
    }

    pub fn open_in_memory(schema: ValidatedSchema) -> Result<Self, ExecError> {
        let conn = Connection::open_in_memory()
            .map_err(|err| Self::storage_error("open in-memory database", err))?;
        Self::new(conn, schema)
    }

    fn storage_error(context: &str, err: rusqlite::Error) -> ExecError {
        ExecError::Storage {
            message: format!("{context}: {err}"),
        }
    }
}

fn stored_class(value: &ValueRef<'_>) -> &'static str {
    match value {
        ValueRef::Null => "NULL",
        ValueRef::Integer(_) => "INTEGER",
        ValueRef::Real(_) => "REAL",
        ValueRef::Text(_) => "TEXT",
        ValueRef::Blob(_) => "BLOB",
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// An owned link-key value for grouping array elements by parent row
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LinkKey {
    Null,
    Int(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl LinkKey {
    fn from_value(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => LinkKey::Null,
            ValueRef::Integer(n) => LinkKey::Int(n),
            ValueRef::Real(x) => LinkKey::Real(x.to_bits()),
            ValueRef::Text(bytes) => LinkKey::Text(String::from_utf8_lossy(bytes).into_owned()),
            ValueRef::Blob(bytes) => LinkKey::Blob(bytes.to_vec()),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            LinkKey::Null => Value::Null,
            LinkKey::Int(n) => Value::Integer(*n),
            LinkKey::Real(bits) => Value::Real(f64::from_bits(*bits)),
            LinkKey::Text(s) => Value::Text(s.clone()),
            LinkKey::Blob(bytes) => Value::Blob(bytes.clone()),
        }
    }
}

/// Scalar leaves under a `ValueType`, depth-first left-to-right to match
/// the validated `columns` order
fn leaf_count(ty: &ValueType) -> usize {
    match ty {
        ValueType::Scalar(_) => 1,
        ValueType::Tuple(elements) => elements.iter().map(leaf_count).sum(),
        ValueType::Optional(inner) => leaf_count(inner),
        ValueType::Array { .. } => 0,
    }
}

/// Decode one node from the head of `vals`; returns value, columns consumed
fn decode_node(ty: &ValueType, vals: &[ValueRef<'_>]) -> Result<(Val, usize), ExecError> {
    match ty {
        ValueType::Scalar(scalar) => decode_scalar(*scalar, vals[0], false).map(|v| (v, 1)),
        ValueType::Optional(inner) => {
            let len = leaf_count(inner);
            if vals[..len].iter().all(|v| matches!(v, ValueRef::Null)) {
                Ok((Val::None, len))
            } else {
                decode_node(inner, vals).map(|(v, n)| (Val::Some(Box::new(v)), n))
            }
        }
        ValueType::Tuple(elements) => {
            let mut items = Vec::with_capacity(elements.len());
            let mut used = 0;
            for element in elements {
                let (v, n) = decode_node(element, &vals[used..])?;
                items.push(v);
                used += n;
            }
            Ok((Val::Tuple(items), used))
        }
        ValueType::Array { .. } => Err(ExecError::Storage {
            message: "nested via arrays are not yet implemented".to_string(),
        }),
    }
}

struct ValueSlot<'a> {
    label: String,
    ty: &'a ValueType,
    start: usize,
    len: usize,
}

struct ArraySlot<'a> {
    slot: usize,
    label: String,
    element: &'a ValueType,
    link_idx: usize,
    side_table: String,
    key_col: String,
    value_cols: Vec<String>,
    keys: Vec<Option<LinkKey>>,
}

enum Slot<'a> {
    Value(ValueSlot<'a>),
    Array(ArraySlot<'a>),
}

/// Decode one scalar cell; `NULL` is `None` only when nullable
fn decode_scalar(ty: ScalarType, value: ValueRef<'_>, nullable: bool) -> Result<Val, ExecError> {
    let mismatch = |found: &str| ExecError::Integrity {
        message: format!("expected {ty:?}, found {found}"),
    };
    match (ty, value) {
        (_, ValueRef::Null) if nullable => Ok(Val::None),
        (ScalarType::Int, ValueRef::Integer(n)) => Ok(Val::Scalar(Scalar::Int(n))),
        (ScalarType::Float, ValueRef::Real(x)) => Ok(Val::Scalar(Scalar::Float(x))),
        (ScalarType::String, ValueRef::Text(bytes)) => String::from_utf8(bytes.to_vec())
            .map(|s| Val::Scalar(Scalar::String(s)))
            .map_err(|_| mismatch("non-UTF8 TEXT")),
        (ScalarType::Bool, ValueRef::Integer(0)) => Ok(Val::Scalar(Scalar::Bool(false))),
        (ScalarType::Bool, ValueRef::Integer(1)) => Ok(Val::Scalar(Scalar::Bool(true))),
        (ScalarType::Dec, ValueRef::Blob(blob)) => {
            codec::decode_dec(blob).map(|d| Val::Scalar(Scalar::Dec(d)))
        }
        (ScalarType::Instant, ValueRef::Blob(blob)) => {
            codec::decode_instant(blob).map(|t| Val::Scalar(Scalar::Instant(t)))
        }
        (ScalarType::Duration, ValueRef::Blob(blob)) => {
            codec::decode_duration(blob).map(|d| Val::Scalar(Scalar::Duration(d)))
        }
        (_, other) => Err(mismatch(stored_class(&other))),
    }
}

impl Storage for SqliteBackend {
    fn fetch(&self, struct_id: StructId, fields: &[FieldId]) -> Result<Vec<Row>, ExecError> {
        let schema = &self.schema;
        let def = schema
            .structs
            .get(struct_id.index())
            .ok_or_else(|| ExecError::Storage {
                message: format!("unknown struct id {}", struct_id.index()),
            })?;
        let struct_name = &def.name;
        let table = quote(&schema.tables[def.table.index()].name);
        let mut select = vec!["rowid".to_string()];
        let mut slots = Vec::with_capacity(fields.len());
        for field_id in fields {
            if field_id.struct_id() != struct_id {
                return Err(ExecError::Integrity {
                    message: format!("fetch field {field_id:?} belongs to another struct"),
                });
            }
            let field = def
                .fields
                .get(field_id.local())
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("unknown field {field_id:?} on struct `{struct_name}`"),
                })?;
            match field {
                Field::Value { name, ty, columns } => {
                    let start = select.len();
                    for col in columns {
                        select.push(quote(schema.column_name(*col)));
                    }
                    slots.push(Slot::Value(ValueSlot {
                        label: format!("{struct_name}.{name}"),
                        ty,
                        start,
                        len: columns.len(),
                    }));
                }
                Field::Array {
                    name,
                    element,
                    link,
                    storage,
                } => {
                    let link_idx = select.len();
                    select.push(quote(schema.column_name(*link)));
                    let side_table = quote(&schema.tables[storage.table.index()].name);
                    let key_col =
                        format!("{side_table}.{}", quote(schema.column_name(storage.key)));
                    slots.push(Slot::Array(ArraySlot {
                        slot: slots.len(),
                        label: format!("{struct_name}.{name}"),
                        element,
                        link_idx,
                        side_table,
                        key_col,
                        value_cols: storage
                            .value
                            .iter()
                            .map(|col| quote(schema.column_name(*col)))
                            .collect(),
                        keys: Vec::new(),
                    }));
                }
                Field::Ref { name, .. } => {
                    return Err(ExecError::Integrity {
                        message: format!(
                            "cannot fetch Ref field `{struct_name}.{name}`: navigation uses fetch_related"
                        ),
                    });
                }
            }
        }
        let ctx = format!("fetch {struct_name}");
        let sql = format!("SELECT {} FROM {table} ORDER BY rowid", select.join(", "));
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|err| Self::storage_error(&ctx, err))? {
            let rowid: i64 = row.get(0).map_err(|err| Self::storage_error(&ctx, err))?;
            let mut values = Vec::with_capacity(slots.len());
            for slot in &mut slots {
                match slot {
                    Slot::Value(slot) => {
                        let mut cells = Vec::with_capacity(slot.len);
                        for i in 0..slot.len {
                            cells.push(
                                row.get_ref(slot.start + i)
                                    .map_err(|err| Self::storage_error(&ctx, err))?,
                            );
                        }
                        let (val, _) =
                            decode_node(slot.ty, &cells).map_err(|err| err.context(&slot.label))?;
                        values.push(val);
                    }
                    Slot::Array(slot) => {
                        let link = row
                            .get_ref(slot.link_idx)
                            .map_err(|err| Self::storage_error(&ctx, err))?;
                        slot.keys.push(if matches!(link, ValueRef::Null) {
                            None
                        } else {
                            Some(LinkKey::from_value(link))
                        });
                        values.push(Val::List(Vec::new()));
                    }
                }
            }
            out.push(Row {
                rowid: Key(rowid),
                values,
            });
        }
        for slot in &slots {
            let Slot::Array(job) = slot else { continue };
            let mut wanted = Vec::new();
            let mut seen = HashMap::new();
            for key in job.keys.iter().flatten() {
                if seen.insert(key, ()).is_none() {
                    wanted.push(key.to_value());
                }
            }
            if wanted.is_empty() {
                continue;
            }
            let placeholders = vec!["?"; wanted.len()].join(", ");
            let sql = format!(
                "SELECT {}, {} FROM {} WHERE {} IN ({placeholders}) ORDER BY rowid",
                job.key_col,
                job.value_cols.join(", "),
                job.side_table,
                job.key_col,
            );
            let mut side = self
                .conn
                .prepare(&sql)
                .map_err(|err| Self::storage_error(&ctx, err))?;
            let mut srows = side
                .query(params_from_iter(wanted))
                .map_err(|err| Self::storage_error(&ctx, err))?;
            let mut groups: HashMap<LinkKey, Vec<Val>> = HashMap::new();
            while let Some(srow) = srows.next().map_err(|err| Self::storage_error(&ctx, err))? {
                let key = LinkKey::from_value(
                    srow.get_ref(0)
                        .map_err(|err| Self::storage_error(&ctx, err))?,
                );
                let mut cells = Vec::with_capacity(job.value_cols.len());
                for i in 0..job.value_cols.len() {
                    cells.push(
                        srow.get_ref(1 + i)
                            .map_err(|err| Self::storage_error(&ctx, err))?,
                    );
                }
                let (v, _) =
                    decode_node(job.element, &cells).map_err(|err| err.context(&job.label))?;
                groups.entry(key).or_default().push(v);
            }
            for (i, link) in job.keys.iter().enumerate() {
                if let Some(key) = link
                    && let Some(items) = groups.get(key)
                {
                    out[i].values[job.slot] = Val::List(items.clone());
                }
            }
        }
        Ok(out)
    }

    fn fetch_related(
        &self,
        _rel: RelationId,
        _fetch_from_first: bool,
        _parents: &[Key],
        _fields: &[FieldId],
    ) -> Result<Vec<Vec<Row>>, ExecError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta};

    #[test]
    fn open_in_memory_constructor() {
        assert!(SqliteBackend::open_in_memory(ValidatedSchema::default()).is_ok());
    }

    #[test]
    fn decode_int_float_string() {
        assert_eq!(
            decode_scalar(ScalarType::Int, ValueRef::Integer(-7), false),
            Ok(Val::Scalar(Scalar::Int(-7)))
        );
        assert_eq!(
            decode_scalar(ScalarType::Float, ValueRef::Real(1.5), false),
            Ok(Val::Scalar(Scalar::Float(1.5)))
        );
        assert_eq!(
            decode_scalar(ScalarType::String, ValueRef::Text(b"hi"), false),
            Ok(Val::Scalar(Scalar::String("hi".to_string())))
        );
        assert!(decode_scalar(ScalarType::String, ValueRef::Text(b"\xFF"), false).is_err());
    }

    #[test]
    fn decode_bool_strict() {
        assert_eq!(
            decode_scalar(ScalarType::Bool, ValueRef::Integer(0), false),
            Ok(Val::Scalar(Scalar::Bool(false)))
        );
        assert_eq!(
            decode_scalar(ScalarType::Bool, ValueRef::Integer(1), false),
            Ok(Val::Scalar(Scalar::Bool(true)))
        );
        assert!(decode_scalar(ScalarType::Bool, ValueRef::Integer(2), false).is_err());
        assert!(decode_scalar(ScalarType::Bool, ValueRef::Real(1.0), false).is_err());
    }

    #[test]
    fn decode_null() {
        assert_eq!(
            decode_scalar(ScalarType::Int, ValueRef::Null, true),
            Ok(Val::None)
        );
        assert!(decode_scalar(ScalarType::Int, ValueRef::Null, false).is_err());
    }

    #[test]
    fn decode_storage_class_mismatch() {
        assert!(decode_scalar(ScalarType::String, ValueRef::Integer(1), false).is_err());
        assert!(decode_scalar(ScalarType::Int, ValueRef::Real(1.0), false).is_err());
        assert!(decode_scalar(ScalarType::Float, ValueRef::Integer(1), false).is_err());
        assert!(decode_scalar(ScalarType::Dec, ValueRef::Text(b"1.5"), false).is_err());
        assert!(decode_scalar(ScalarType::Instant, ValueRef::Integer(0), false).is_err());
    }

    #[test]
    fn decode_blobs_round_trip() {
        let d: rust_decimal::Decimal = "30.00".parse().unwrap();
        assert_eq!(
            decode_scalar(
                ScalarType::Dec,
                ValueRef::Blob(&codec::encode_dec(d)),
                false
            ),
            Ok(Val::Scalar(Scalar::Dec(d.normalize())))
        );
        let t = DateTime::from_timestamp(1_700_000_000, 500).unwrap();
        assert_eq!(
            decode_scalar(
                ScalarType::Instant,
                ValueRef::Blob(&codec::encode_instant(t)),
                false
            ),
            Ok(Val::Scalar(Scalar::Instant(t)))
        );
        let span = TimeDelta::milliseconds(-1500);
        match decode_scalar(
            ScalarType::Duration,
            ValueRef::Blob(&codec::encode_duration(span)),
            false,
        ) {
            Ok(Val::Scalar(Scalar::Duration(back))) => {
                assert_eq!(back.num_seconds(), span.num_seconds());
                assert_eq!(back.subsec_nanos(), span.subsec_nanos());
            }
            other => panic!("unexpected decode result: {other:?}"),
        }
    }
}
