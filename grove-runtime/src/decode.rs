
use chrono::{DateTime, TimeDelta, Utc};
use grove_query::codegen::model::ScalarType;
use rust_decimal::Decimal;

use super::error::RuntimeError;
use super::value::Value;

const DEC_BYTES: usize = 13;
const DEC_EXP_BIAS: i32 = 28;
const DEC_BIASED_MAX: u8 = 56;
const TEN_POW_29: u128 = 100_000_000_000_000_000_000_000_000_000;
const TWO_POW_96: u128 = 1 << 96;
const TEMPORAL_BYTES: usize = 12;
const NANOS_PER_SEC: i64 = 1_000_000_000;

pub fn assemble_dec(negative: bool, exp: i8, significand: u128) -> [u8; DEC_BYTES] {
    let biased = exp as i16 + DEC_EXP_BIAS as i16;
    let mut out = [0u8; DEC_BYTES];
    out[0] = (((biased as u16) << 1) as u8) | ((significand >> 96) & 1) as u8 | 0x80;
    let low = significand & (TWO_POW_96 - 1);
    out[1..13].copy_from_slice(&low.to_be_bytes()[4..16]);
    if negative {
        for byte in out.iter_mut() {
            *byte ^= 0xFF;
        }
    }
    out
}

pub fn encode_dec(d: &Decimal) -> [u8; DEC_BYTES] {
    let mut mantissa = d.mantissa().unsigned_abs();
    let mut scale = d.scale();
    while scale > 0 && mantissa.is_multiple_of(10) {
        mantissa /= 10;
        scale -= 1;
    }
    if mantissa == 0 {
        return [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    }
    let negative = d.is_sign_negative();
    let mut digits = 0u32;
    let mut probe = mantissa;
    while probe > 0 {
        digits += 1;
        probe /= 10;
    }
    let exp = digits as i32 - 1 - scale as i32;
    debug_assert!((1..=29).contains(&digits));
    debug_assert!((-DEC_EXP_BIAS..=DEC_EXP_BIAS).contains(&exp));
    let aligned = mantissa * 10u128.pow(29 - digits);
    debug_assert!(aligned < TEN_POW_29);
    assemble_dec(negative, exp as i8, aligned)
}

pub fn decode_dec(bytes: &[u8], column: &str) -> Result<Decimal, RuntimeError> {
    let decode_err = |found: String| RuntimeError::Decode {
        column: column.to_string(),
        expected: "Dec".to_string(),
        found,
    };
    if bytes.len() != DEC_BYTES {
        return Err(decode_err(format!("{}-byte blob", bytes.len())));
    }
    let negative = bytes[0] & 0x80 == 0;
    let mut raw = [0u8; DEC_BYTES];
    raw.copy_from_slice(bytes);
    if negative {
        for byte in raw.iter_mut() {
            *byte ^= 0xFF;
        }
    }
    let biased = (raw[0] >> 1) & 0x3F;
    if biased > DEC_BIASED_MAX {
        return Err(decode_err("exponent out of range".to_string()));
    }
    let exp = biased as i32 - DEC_EXP_BIAS;
    let mut field = [0u8; 16];
    field[3] = raw[0] & 1;
    field[4..16].copy_from_slice(&raw[1..13]);
    let aligned = u128::from_be_bytes(field);
    if aligned >= TEN_POW_29 {
        return Err(decode_err("significand out of range".to_string()));
    }
    if aligned == 0 {
        return Ok(Decimal::ZERO);
    }
    let mut trailing = 0u32;
    let mut probe = aligned;
    while probe.is_multiple_of(10) {
        probe /= 10;
        trailing += 1;
    }
    let stripped = probe;
    let mut stripped_digits = 0u32;
    let mut probe = stripped;
    while probe > 0 {
        stripped_digits += 1;
        probe /= 10;
    }
    let mut split = None;
    for k in 0..=trailing {
        let scale = stripped_digits as i32 + k as i32 - 1 - exp;
        if scale >= 0 {
            split = Some((stripped * 10u128.pow(k), scale as u32));
            break;
        }
    }
    let Some((mantissa, scale)) = split else {
        return Err(decode_err("unrepresentable scale".to_string()));
    };
    if scale > 28 {
        return Err(decode_err("scale out of range".to_string()));
    }
    if mantissa >= TWO_POW_96 {
        return Err(decode_err("mantissa out of range".to_string()));
    }
    let mut value = Decimal::try_from_i128_with_scale(mantissa as i128, scale)
        .map_err(|_| decode_err("mantissa out of range".to_string()))?;
    value.set_sign_negative(negative);
    Ok(value)
}

pub fn encode_instant(t: &DateTime<Utc>) -> [u8; TEMPORAL_BYTES] {
    let mut out = [0u8; TEMPORAL_BYTES];
    out[..8].copy_from_slice(&t.timestamp().to_be_bytes());
    out[8..].copy_from_slice(&t.timestamp_subsec_nanos().to_be_bytes());
    out[0] ^= 0x80;
    out
}

pub fn decode_instant(bytes: &[u8], column: &str) -> Result<DateTime<Utc>, RuntimeError> {
    let decode_err = |found: String| RuntimeError::Decode {
        column: column.to_string(),
        expected: "Instant".to_string(),
        found,
    };
    if bytes.len() != TEMPORAL_BYTES {
        return Err(decode_err(format!("{}-byte blob", bytes.len())));
    }
    let mut secs = [0u8; 8];
    let mut nanos = [0u8; 4];
    secs.copy_from_slice(&bytes[..8]);
    nanos.copy_from_slice(&bytes[8..]);
    secs[0] ^= 0x80;
    DateTime::from_timestamp(i64::from_be_bytes(secs), u32::from_be_bytes(nanos))
        .ok_or_else(|| decode_err("timestamp out of range".to_string()))
}

pub fn encode_duration(d: &TimeDelta) -> [u8; TEMPORAL_BYTES] {
    let total = d.num_seconds() as i128 * NANOS_PER_SEC as i128 + d.subsec_nanos() as i128;
    let secs = total.div_euclid(NANOS_PER_SEC as i128);
    let nanos = total.rem_euclid(NANOS_PER_SEC as i128);
    debug_assert!(secs >= i64::MIN as i128 && secs <= i64::MAX as i128);
    debug_assert!(nanos >= 0 && nanos < NANOS_PER_SEC as i128);
    let mut out = [0u8; TEMPORAL_BYTES];
    out[..8].copy_from_slice(&(secs as i64).to_be_bytes());
    out[8..].copy_from_slice(&(nanos as u32).to_be_bytes());
    out[0] ^= 0x80;
    out
}

pub fn decode_duration(bytes: &[u8], column: &str) -> Result<TimeDelta, RuntimeError> {
    let decode_err = |found: String| RuntimeError::Decode {
        column: column.to_string(),
        expected: "Duration".to_string(),
        found,
    };
    if bytes.len() != TEMPORAL_BYTES {
        return Err(decode_err(format!("{}-byte blob", bytes.len())));
    }
    let mut secs = [0u8; 8];
    let mut nanos = [0u8; 4];
    secs.copy_from_slice(&bytes[..8]);
    nanos.copy_from_slice(&bytes[8..]);
    secs[0] ^= 0x80;
    let nanos = u32::from_be_bytes(nanos);
    if nanos >= NANOS_PER_SEC as u32 {
        return Err(decode_err("nanos out of range".to_string()));
    }
    TimeDelta::new(i64::from_be_bytes(secs), nanos)
        .ok_or_else(|| decode_err("duration out of range".to_string()))
}

pub fn decode_value(
    value: rusqlite::types::ValueRef<'_>,
    column: &str,
    ty: Option<ScalarType>,
) -> Result<Value, RuntimeError> {
    use rusqlite::types::ValueRef;
    let decode_err = |expected: &str, found: String| RuntimeError::Decode {
        column: column.to_string(),
        expected: expected.to_string(),
        found,
    };
    match value {
        ValueRef::Null => Ok(Value::Null),
        ValueRef::Integer(n) => match ty {
            None | Some(ScalarType::Int) => Ok(Value::Int(n)),
            Some(ScalarType::Bool) => match n {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => Err(decode_err("Bool", format!("INTEGER {n}"))),
            },
            Some(ScalarType::Float) => {
                if n.abs() <= (1i64 << 53) {
                    Ok(Value::Float(n as f64))
                } else {
                    Err(decode_err("Float", format!("inexact INTEGER {n}")))
                }
            }
            Some(other) => Err(decode_err(&format!("{other:?}"), format!("INTEGER {n}"))),
        },
        ValueRef::Real(f) => match ty {
            Some(ScalarType::Float) => Ok(Value::Float(f)),
            _ => Err(decode_err(&format!("{ty:?}"), format!("REAL {f}"))),
        },
        ValueRef::Text(t) => match ty {
            None | Some(ScalarType::String) => std::str::from_utf8(t)
                .map(|s| Value::String(s.to_string()))
                .map_err(|_| decode_err("String", "invalid UTF-8".to_string())),
            Some(other) => Err(decode_err(&format!("{other:?}"), "TEXT".to_string())),
        },
        ValueRef::Blob(b) => match ty {
            Some(ScalarType::Dec) => decode_dec(b, column).map(Value::Dec),
            Some(ScalarType::Instant) => decode_instant(b, column).map(Value::Instant),
            Some(ScalarType::Duration) => decode_duration(b, column).map(Value::Duration),
            _ => Err(decode_err(
                &format!("{ty:?}"),
                format!("{}-byte BLOB", b.len()),
            )),
        },
    }
}

pub fn encode_param(p: &grove_query::codegen::model::ParamValue) -> rusqlite::types::Value {
    use grove_query::codegen::model::ParamValue;
    use rusqlite::types::Value as SqlValue;
    match p {
        ParamValue::Int(n) => SqlValue::Integer(*n),
        ParamValue::Float(f) => SqlValue::Real(*f),
        ParamValue::Dec(d) => SqlValue::Blob(encode_dec(d).to_vec()),
        ParamValue::String(s) => SqlValue::Text(s.clone()),
        ParamValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
        ParamValue::Instant(t) => SqlValue::Blob(encode_instant(t).to_vec()),
        ParamValue::Duration(d) => SqlValue::Blob(encode_duration(d).to_vec()),
        ParamValue::Null => SqlValue::Null,
    }
}
