use grove_schema::validated::{ScalarType, ValidatedSchema, ValueType};
use rusqlite::types::{Value, ValueRef};

use crate::codec;
use crate::error::ExecError;
use crate::value::{Scalar, Val, enum_variant_scalar};

pub(super) fn stored_class(value: &ValueRef<'_>) -> &'static str {
    match value {
        ValueRef::Null => "NULL",
        ValueRef::Integer(_) => "INTEGER",
        ValueRef::Real(_) => "REAL",
        ValueRef::Text(_) => "TEXT",
        ValueRef::Blob(_) => "BLOB",
    }
}

pub(super) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Encode a scalar literal as a SQLite value
pub(super) fn scalar_value(value: &Scalar) -> Value {
    match value {
        Scalar::Int(n) => Value::Integer(*n),
        Scalar::Float(x) => Value::Real(*x),
        Scalar::Dec(d) => Value::Blob(Vec::from(codec::encode_dec(*d))),
        Scalar::String(s) => Value::Text(s.clone()),
        Scalar::Bytes(bytes) => Value::Blob(bytes.clone()),
        Scalar::Bool(b) => Value::Integer(if *b { 1 } else { 0 }),
        Scalar::Instant(t) => Value::Blob(Vec::from(codec::encode_instant(*t))),
        Scalar::Duration(d) => Value::Blob(Vec::from(codec::encode_duration(*d))),
    }
}

pub(super) fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// An owned link-key value for grouping array elements by parent row
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum LinkKey {
    Null,
    Int(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl LinkKey {
    pub(super) fn from_value(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => LinkKey::Null,
            ValueRef::Integer(n) => LinkKey::Int(n),
            ValueRef::Real(x) => LinkKey::Real(x.to_bits()),
            ValueRef::Text(bytes) => LinkKey::Text(String::from_utf8_lossy(bytes).into_owned()),
            ValueRef::Blob(bytes) => LinkKey::Blob(bytes.to_vec()),
        }
    }

    pub(super) fn to_value(&self) -> Value {
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
fn type_width(ty: &ValueType, array_width: usize) -> usize {
    match ty {
        ValueType::Scalar(_) | ValueType::Enum(_) => 1,
        ValueType::Tuple(elements) => elements
            .iter()
            .map(|element| type_width(element, array_width))
            .sum(),
        ValueType::Optional(inner) => type_width(inner, array_width),
        ValueType::Array { .. } => array_width,
    }
}

pub(super) fn leaf_count(ty: &ValueType) -> usize {
    type_width(ty, 0)
}

pub(super) fn decode_scalar(
    ty: ScalarType,
    value: ValueRef<'_>,
    nullable: bool,
    float_checks: bool,
) -> Result<Val, ExecError> {
    let mismatch = |found: &str| ExecError::Integrity {
        message: format!("expected {ty:?}, found {found}"),
    };
    match (ty, value) {
        (_, ValueRef::Null) if nullable => Ok(Val::None),
        // SQLite stores NaN as NULL; decoding prefers absence over NaN if nullable
        (ScalarType::Float, ValueRef::Null) => {
            if float_checks {
                Err(ExecError::Integrity {
                    message: "found non-finite float: NaN".to_string(),
                })
            } else {
                Ok(Val::Scalar(Scalar::Float(f64::NAN)))
            }
        }
        (ScalarType::Int, ValueRef::Integer(n)) => Ok(Val::Scalar(Scalar::Int(n))),
        (ScalarType::Float, ValueRef::Real(x)) => Ok(Val::Scalar(Scalar::Float(x))),
        (ScalarType::String, ValueRef::Text(bytes)) => String::from_utf8(bytes.to_vec())
            .map(|s| Val::Scalar(Scalar::String(s)))
            .map_err(|_| mismatch("non-UTF8 TEXT")),
        (ScalarType::Bytes, ValueRef::Blob(blob)) => Ok(Val::Scalar(Scalar::Bytes(blob.to_vec()))),
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

pub(super) fn encode_value(
    ty: &ValueType,
    value: &Val,
    schema: &ValidatedSchema,
    out: &mut Vec<Value>,
) -> Result<(), ExecError> {
    match ty {
        ValueType::Scalar(scalar_ty) => {
            let Val::Scalar(scalar) = value else {
                return Err(ExecError::Internal {
                    message: format!("expected scalar value, got {value}"),
                });
            };
            let encoded = match (scalar_ty, scalar) {
                (ScalarType::Int, Scalar::Int(n)) => Value::Integer(*n),
                (ScalarType::Float, Scalar::Float(x)) => Value::Real(*x),
                (ScalarType::Dec, Scalar::Dec(d)) => scalar_value(&Scalar::Dec(*d)),
                (ScalarType::String, Scalar::String(s)) => Value::Text(s.clone()),
                (ScalarType::Bytes, Scalar::Bytes(bytes)) => Value::Blob(bytes.clone()),
                (ScalarType::Bool, Scalar::Bool(b)) => Value::Integer(i64::from(*b)),
                (ScalarType::Instant, Scalar::Instant(t)) => scalar_value(&Scalar::Instant(*t)),
                (ScalarType::Duration, Scalar::Duration(d)) => scalar_value(&Scalar::Duration(*d)),
                _ => {
                    return Err(ExecError::Internal {
                        message: format!("value {value} does not match column type {scalar_ty:?}"),
                    });
                }
            };
            out.push(encoded);
        }
        ValueType::Enum(id) => {
            let Val::Enum(found, variant) = value else {
                return Err(ExecError::Internal {
                    message: format!("expected enum value, got {value}"),
                });
            };
            if found != id {
                return Err(ExecError::Internal {
                    message: format!(
                        "enum id {} does not match target {}",
                        found.index(),
                        id.index()
                    ),
                });
            }
            let enum_def = schema
                .enums
                .get(id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown enum id {}", id.index()),
                })?;
            let scalar =
                enum_variant_scalar(enum_def, *variant).ok_or_else(|| ExecError::Internal {
                    message: format!("enum `{}` has no variant index {variant}", enum_def.name),
                })?;
            out.push(scalar_value(&scalar));
        }
        _ => {
            return Err(ExecError::Internal {
                message: format!("unexpected encode type {ty:?}"),
            });
        }
    }
    Ok(())
}

pub(super) fn value_write_width(ty: &ValueType) -> usize {
    type_width(ty, 1)
}
