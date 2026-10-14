#![allow(dead_code)]

use grove_schema::validated::{FieldId, RelationId, ScalarType, StructId, ValidatedSchema};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::codec;
use crate::error::ExecError;
use crate::storage::{Key, Row, Storage};
use crate::value::{Scalar, Val};

pub struct SqliteBackend {
    conn: Connection,
    schema: ValidatedSchema,
}

impl SqliteBackend {
    pub fn new(conn: Connection, schema: ValidatedSchema) -> Self {
        SqliteBackend { conn, schema }
    }

    pub fn open_in_memory(schema: ValidatedSchema) -> Result<Self, ExecError> {
        let conn = Connection::open_in_memory()
            .map_err(|err| Self::storage_error("open in-memory database", err))?;
        Ok(Self::new(conn, schema))
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

/// Decode one scalar column; `NULL` is `None` only when nullable
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
    fn fetch(&self, _struct_id: StructId, _fields: &[FieldId]) -> Result<Vec<Row>, ExecError> {
        todo!()
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
