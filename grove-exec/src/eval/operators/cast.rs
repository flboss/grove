use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use grove_query::casts::CastKind;
use grove_schema::validated::{EnumId, EnumUnderlying, EnumValue, ValidatedSchema};
use grove_types::Span;
use num_traits::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

use crate::error::ExecError;
use crate::value::{self, Scalar, Val};

pub(crate) fn eval_cast(
    kind: CastKind,
    value: Val,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<Val, ExecError> {
    if kind.is_fallible() {
        return eval_fallible_cast(kind, value, schema);
    }
    let scalar = match (kind, value) {
        (CastKind::IntToString, Val::Scalar(scalar @ Scalar::Int(_)))
        | (CastKind::FloatToString, Val::Scalar(scalar @ Scalar::Float(_)))
        | (CastKind::BoolToString, Val::Scalar(scalar @ Scalar::Bool(_)))
        | (CastKind::DecToString, Val::Scalar(scalar @ Scalar::Dec(_)))
        | (CastKind::InstantToString, Val::Scalar(scalar @ Scalar::Instant(_))) => {
            Scalar::String(scalar.to_string())
        }
        (CastKind::IntToFloat, Val::Scalar(Scalar::Int(n))) => Scalar::Float(n as f64),
        (CastKind::IntToDec, Val::Scalar(Scalar::Int(n))) => {
            Scalar::Dec(Decimal::from_i64(n).expect("Decimal fits every i64"))
        }
        // Rust `as` semantics: truncate toward zero, saturate, NaN to zero
        (CastKind::FloatToInt, Val::Scalar(Scalar::Float(x))) => Scalar::Int(x as i64),
        (CastKind::FloatToDec, Val::Scalar(Scalar::Float(x))) => match Decimal::from_f64(x) {
            Some(d) => Scalar::Dec(d.normalize()),
            None if x.is_nan() => {
                return Err(ExecError::Abort {
                    message: "cannot cast NaN to Dec".to_string(),
                    span,
                });
            }
            None => Scalar::Dec(if x.is_sign_positive() {
                Decimal::MAX
            } else {
                Decimal::MIN
            }),
        },
        (CastKind::DecToInt, Val::Scalar(Scalar::Dec(d))) => {
            Scalar::Int(d.trunc().to_i64().unwrap_or(if d.is_sign_positive() {
                i64::MAX
            } else {
                i64::MIN
            }))
        }
        (CastKind::DecToFloat, Val::Scalar(Scalar::Dec(d))) => {
            Scalar::Float(d.to_f64().expect("f64 has a wider range than Decimal"))
        }
        (CastKind::BoolToInt, Val::Scalar(Scalar::Bool(b))) => Scalar::Int(i64::from(b)),
        (CastKind::StringToBytes, Val::Scalar(Scalar::String(s))) => Scalar::Bytes(s.into_bytes()),
        (CastKind::EnumToInt(enum_id), Val::Enum(id, index)) if id == enum_id => {
            enum_cast_scalar(schema, id, index, EnumUnderlying::Int)?
        }
        (CastKind::EnumToString(enum_id), Val::Enum(id, index)) if id == enum_id => {
            enum_cast_scalar(schema, id, index, EnumUnderlying::String)?
        }
        (kind, value) => {
            return Err(ExecError::Internal {
                message: format!("mismatched infallible cast operands: {value} via {kind:?}"),
            });
        }
    };
    Ok(Val::Scalar(scalar))
}

fn eval_fallible_cast(
    kind: CastKind,
    value: Val,
    schema: &ValidatedSchema,
) -> Result<Val, ExecError> {
    let converted = match (kind, value) {
        (CastKind::StringToInt, Val::Scalar(Scalar::String(s))) => {
            s.parse::<i64>().ok().map(Scalar::Int).map(Val::Scalar)
        }
        (CastKind::StringToFloat, Val::Scalar(Scalar::String(s))) => s
            .parse::<f64>()
            .ok()
            .filter(|value| !schema.config.float_checks || value.is_finite())
            .map(Scalar::Float)
            .map(Val::Scalar),
        (CastKind::StringToDec, Val::Scalar(Scalar::String(s))) => {
            s.parse::<Decimal>().ok().map(Scalar::Dec).map(Val::Scalar)
        }
        (CastKind::StringToBool, Val::Scalar(Scalar::String(s))) => {
            if s.eq_ignore_ascii_case("true") {
                Some(Val::Scalar(Scalar::Bool(true)))
            } else if s.eq_ignore_ascii_case("false") {
                Some(Val::Scalar(Scalar::Bool(false)))
            } else {
                None
            }
        }
        (CastKind::StringToInstant, Val::Scalar(Scalar::String(s))) => {
            parse_instant(&s).map(Scalar::Instant).map(Val::Scalar)
        }
        (CastKind::IntToBool, Val::Scalar(Scalar::Int(0))) => {
            Some(Val::Scalar(Scalar::Bool(false)))
        }
        (CastKind::IntToBool, Val::Scalar(Scalar::Int(1))) => Some(Val::Scalar(Scalar::Bool(true))),
        (CastKind::IntToBool, Val::Scalar(Scalar::Int(_))) => None,
        (CastKind::BytesToString, Val::Scalar(Scalar::Bytes(bytes))) => String::from_utf8(bytes)
            .ok()
            .map(Scalar::String)
            .map(Val::Scalar),
        (CastKind::FloatToIntStrict, Val::Scalar(Scalar::Float(x))) => {
            strict_float_to_int(x).map(Val::Scalar)
        }
        (CastKind::DecToIntStrict, Val::Scalar(Scalar::Dec(d))) => {
            if d.fract().is_zero() {
                d.to_i64().map(Scalar::Int).map(Val::Scalar)
            } else {
                None
            }
        }
        (CastKind::FloatToDecStrict, Val::Scalar(Scalar::Float(x))) => {
            if x.is_finite() {
                Decimal::from_f64(x)
                    .map(|d| Scalar::Dec(d.normalize()))
                    .map(Val::Scalar)
            } else {
                None
            }
        }
        (CastKind::StringToEnum(enum_id), Val::Scalar(Scalar::String(s))) => {
            enum_variant_index_by_value(schema, enum_id, EnumValue::String(s))?
                .map(|index| Val::Enum(enum_id, index))
        }
        (CastKind::IntToEnum(enum_id), Val::Scalar(Scalar::Int(n))) => {
            enum_variant_index_by_value(schema, enum_id, EnumValue::Int(n))?
                .map(|index| Val::Enum(enum_id, index))
        }
        (kind, value) => {
            return Err(ExecError::Internal {
                message: format!("mismatched fallible cast operands: {value} via {kind:?}"),
            });
        }
    };
    Ok(converted.map_or(Val::None, |value| Val::Some(Box::new(value))))
}

fn parse_instant(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(datetime) = DateTime::parse_from_rfc3339(value) {
        return Some(datetime.with_timezone(&Utc));
    }
    if let Some((date, time)) = value.split_once(' ') {
        let normalized = format!("{date}T{time}");
        if let Ok(datetime) = DateTime::parse_from_rfc3339(&normalized) {
            return Some(datetime.with_timezone(&Utc));
        }
        for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
            if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
                return Some(DateTime::from_naive_utc_and_offset(naive, Utc));
            }
        }
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    Some(DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

fn enum_variant_index_by_value(
    schema: &ValidatedSchema,
    id: EnumId,
    value: EnumValue,
) -> Result<Option<usize>, ExecError> {
    let enum_def = schema
        .enums
        .get(id.index())
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown enum id {}", id.index()),
        })?;
    if enum_def.underlying != value.enum_underlying() {
        return Err(ExecError::Internal {
            message: format!("enum cast source does not match enum `{}`", enum_def.name),
        });
    }
    Ok(enum_def.variant_index_by_value(&value))
}

fn strict_float_to_int(x: f64) -> Option<Scalar> {
    const MIN: f64 = i64::MIN as f64;
    const MAX: f64 = i64::MAX as f64;
    if !x.is_finite() || x.fract() != 0.0 || !(MIN..=MAX).contains(&x) {
        return None;
    }
    Some(Scalar::Int(x as i64))
}

fn enum_cast_scalar(
    schema: &ValidatedSchema,
    id: EnumId,
    index: usize,
    expected: EnumUnderlying,
) -> Result<Scalar, ExecError> {
    let enum_def = schema
        .enums
        .get(id.index())
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown enum id {}", id.index()),
        })?;
    if enum_def.underlying != expected {
        return Err(ExecError::Internal {
            message: format!("enum cast target does not match enum `{}`", enum_def.name),
        });
    }
    value::enum_variant_scalar(enum_def, index).ok_or_else(|| ExecError::Internal {
        message: format!("unknown variant {index} for enum `{}`", enum_def.name),
    })
}
