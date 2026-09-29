
use chrono::{DateTime, TimeDelta, Utc};
use grove_query::codegen::model::ScalarType;
use rust_decimal::Decimal;

use super::error::RuntimeError;
use super::value::Value;

const DEC_BYTES: usize = 32;
const DEC_DIGITS: usize = 29;
const DEC_EXP_BIAS: i32 = 28;
const TEMPORAL_BYTES: usize = 12;
const NANOS_PER_SEC: i64 = 1_000_000_000;

pub fn encode_dec(d: &Decimal) -> [u8; DEC_BYTES] {
    let scale = d.scale() as i32;
    let mut magnitude = d.mantissa().unsigned_abs();
    let negative = d.is_sign_negative();

    let mut digits = [0u8; DEC_DIGITS];
    let mut count = 0usize;
    let exp = if magnitude == 0 {
        count = scale as usize + 1;
        -DEC_EXP_BIAS
    } else {
        let mut i = DEC_DIGITS;
        while magnitude > 0 {
            i -= 1;
            digits[i] = (magnitude % 10) as u8;
            magnitude /= 10;
            count += 1;
        }
        digits.copy_within(i.., 0);
        count as i32 - 1 - scale
    };
    debug_assert!((1..=DEC_DIGITS).contains(&count));
    debug_assert!((-DEC_EXP_BIAS..=DEC_EXP_BIAS).contains(&exp));

    let mut out = [0u8; DEC_BYTES];
    out[0] = if negative { 0x00 } else { 0x02 };
    out[1] = (exp + DEC_EXP_BIAS) as u8;
    out[2..31].copy_from_slice(&digits);
    out[31] = count as u8;
    if negative {
        out[1] ^= 0xFF;
        out[31] ^= 0xFF;
        for byte in out.iter_mut().skip(2).take(DEC_DIGITS) {
            *byte = 9 - *byte;
        }
    }
    out
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
    let negative = match bytes[0] {
        0x00 => true,
        0x02 => false,
        class => return Err(decode_err(format!("bad class byte {class:#04x}"))),
    };
    let (exp_byte, count_byte) = if negative {
        (bytes[1] ^ 0xFF, bytes[31] ^ 0xFF)
    } else {
        (bytes[1], bytes[31])
    };
    let exp = exp_byte as i32 - DEC_EXP_BIAS;
    let count = count_byte as usize;
    if !(-DEC_EXP_BIAS..=DEC_EXP_BIAS).contains(&exp) {
        return Err(decode_err("exponent out of range".to_string()));
    }
    if !(1..=DEC_DIGITS).contains(&count) {
        return Err(decode_err("digit count out of range".to_string()));
    }
    let mut digits = [0u8; DEC_DIGITS];
    digits.copy_from_slice(&bytes[2..31]);
    for byte in digits.iter_mut() {
        if *byte > 9 {
            return Err(decode_err("bad digit byte".to_string()));
        }
        if negative {
            *byte = 9 - *byte;
        }
    }
    let mut magnitude: i128 = 0;
    let mut all_zero = true;
    for &digit in &digits[..count] {
        magnitude = magnitude * 10 + digit as i128;
        all_zero &= digit == 0;
    }
    let scale = if all_zero {
        count as i32 - 1
    } else {
        let scale = count as i32 - 1 - exp;
        if !(0..=28).contains(&scale) {
            return Err(decode_err("scale out of range".to_string()));
        }
        scale
    };
    let mut value = Decimal::try_from_i128_with_scale(magnitude, scale as u32)
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
