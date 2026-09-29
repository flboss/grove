
use chrono::{DateTime, TimeDelta, Utc};
use grove_query::codegen::model::{ParamValue, ScalarType};
use grove_runtime::decode::{
    decode_value, encode_dec, encode_duration, encode_instant, encode_param,
};
use grove_runtime::{RuntimeConfig, Value, decode_dec, decode_duration, decode_instant};
use rusqlite::types::ValueRef;
use rust_decimal::Decimal;
use std::str::FromStr;


fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[test]
fn dec_round_trip_exact() {
    let cases = [
        "0",
        "0.00",
        "-0.00",
        "1",
        "-1",
        "5.0",
        "2.50",
        "-5.00",
        "123.456",
        "-123.456",
        "79228162514264337593543950335",
        "-79228162514264337593543950335",
        "0.0000000000000000000000000001",
        "-0.0000000000000000000000000001",
        "12345678901234567890123456789",
        "3.1415926535897932384626433833",
    ];
    for s in cases {
        let d = dec(s);
        let rt = decode_dec(&encode_dec(&d), "c").unwrap();
        assert_eq!(rt, d, "round-trip of {s}");
        if s != "-0.00" {
            assert_eq!(rt.to_string(), s, "display fidelity of {s}");
        }
    }
    let mut neg_zero = dec("0.00");
    neg_zero.set_sign_negative(true);
    let rt = decode_dec(&encode_dec(&neg_zero), "c").unwrap();
    assert!(rt.is_sign_negative());
    assert_eq!(rt.to_string(), "-0.00");
}

#[test]
fn dec_ordering_matches_numeric() {
    let mut values = vec![
        dec("-1000"),
        dec("-99.99"),
        dec("-2"),
        dec("-1.001"),
        dec("-1"),
        dec("-0.5"),
        dec("-0.00"),
        dec("0"),
        dec("0.00"),
        dec("0.5"),
        dec("1"),
        dec("1.0"),
        dec("1.001"),
        dec("2"),
        dec("99.99"),
        dec("1000"),
        dec("79228162514264337593543950335"),
        dec("0.0000000000000000000000000001"),
    ];
    values.reverse();
    let encoded: Vec<_> = values.iter().map(encode_dec).collect();
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by_key(|&i| encoded[i]);
    for pair in order.windows(2) {
        let (a, b) = (values[pair[0]], values[pair[1]]);
        assert!(a <= b, "order inversion: {a} before {b}");
    }
    let mut distinct = values.clone();
    distinct.sort();
    distinct.dedup();
    let encoded: Vec<_> = distinct.iter().map(encode_dec).collect();
    let mut sorted = encoded.clone();
    sorted.sort();
    assert_eq!(sorted, encoded);
}

#[test]
fn dec_malformed_is_decode_error() {
    let good = encode_dec(&dec("1.5"));
    let mut bad = good;
    bad[0] = 0x07;
    assert!(decode_dec(&bad, "c").is_err());
    assert!(decode_dec(&good[..31], "c").is_err());
    let mut bad = good;
    bad[5] = 0x0A;
    assert!(decode_dec(&bad, "c").is_err());
    let mut bad = good;
    bad[2] = 30;
    assert!(decode_dec(&bad, "c").is_err());
}


fn instant(secs: i64, nanos: u32) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, nanos).unwrap()
}

#[test]
fn instant_round_trip_and_order() {
    let cases = [
        instant(0, 0),
        instant(0, 1),
        instant(1, 0),
        instant(-1, 0),
        instant(-1, 999_999_999),
        instant(1_700_000_000, 123_456_789),
        instant(8_000_000_000_000, 0),
        instant(-8_000_000_000_000, 0),
        instant(253_402_300_799, 999_999_999),
        instant(-62_167_219_200, 0),
    ];
    for t in cases {
        assert_eq!(decode_instant(&encode_instant(&t), "c").unwrap(), t);
    }
    let mut encoded: Vec<_> = cases.iter().map(encode_instant).collect();
    encoded.sort();
    let mut ordered = cases;
    ordered.sort();
    let reordered: Vec<_> = encoded
        .iter()
        .map(|b| decode_instant(b, "c").unwrap())
        .collect();
    assert_eq!(reordered, ordered);
}


#[test]
fn duration_round_trip_exact() {
    let cases = [
        TimeDelta::zero(),
        TimeDelta::seconds(1),
        TimeDelta::seconds(-1),
        TimeDelta::milliseconds(1500),
        TimeDelta::milliseconds(-1500),
        TimeDelta::nanoseconds(1),
        TimeDelta::nanoseconds(-1),
        TimeDelta::nanoseconds(999_999_999),
        TimeDelta::seconds(90_000_000_000),
        TimeDelta::seconds(-90_000_000_000),
        TimeDelta::MIN,
        TimeDelta::MAX,
    ];
    for d in cases {
        let rt = decode_duration(&encode_duration(&d), "c").unwrap();
        assert_eq!(rt, d, "round-trip of {d:?}");
    }
}

#[test]
fn duration_ordering_matches_chrono() {
    let mut values = vec![
        TimeDelta::MIN,
        TimeDelta::seconds(-1_000_000),
        TimeDelta::milliseconds(-1500),
        TimeDelta::nanoseconds(-2),
        TimeDelta::nanoseconds(-1),
        TimeDelta::zero(),
        TimeDelta::nanoseconds(1),
        TimeDelta::nanoseconds(2),
        TimeDelta::milliseconds(1500),
        TimeDelta::seconds(1_000_000),
        TimeDelta::MAX,
    ];
    values.reverse();
    let encoded: Vec<_> = values.iter().map(encode_duration).collect();
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by_key(|&i| encoded[i]);
    for pair in order.windows(2) {
        assert!(values[pair[0]] <= values[pair[1]]);
    }
}


#[test]
fn decode_value_table() {
    use ScalarType as T;
    assert_eq!(
        decode_value(ValueRef::Integer(7), "k", None).unwrap(),
        Value::Int(7)
    );
    assert_eq!(
        decode_value(ValueRef::Text(b"hi"), "k", None).unwrap(),
        Value::String("hi".to_string())
    );
    assert_eq!(
        decode_value(ValueRef::Null, "k", None).unwrap(),
        Value::Null
    );
    assert_eq!(
        decode_value(ValueRef::Integer(1), "b", Some(T::Bool)).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        decode_value(ValueRef::Integer(0), "b", Some(T::Bool)).unwrap(),
        Value::Bool(false)
    );
    assert!(decode_value(ValueRef::Integer(2), "b", Some(T::Bool)).is_err());
    assert_eq!(
        decode_value(ValueRef::Integer(42), "f", Some(T::Float)).unwrap(),
        Value::Float(42.0)
    );
    assert!(decode_value(ValueRef::Integer(i64::MAX), "f", Some(T::Float)).is_err());
    assert_eq!(
        decode_value(ValueRef::Real(2.5), "f", Some(T::Float)).unwrap(),
        Value::Float(2.5)
    );
    assert!(decode_value(ValueRef::Real(2.5), "i", Some(T::Int)).is_err());
    let d = dec("9.99");
    assert_eq!(
        decode_value(ValueRef::Blob(&encode_dec(&d)), "d", Some(T::Dec)).unwrap(),
        Value::Dec(d)
    );
    assert!(decode_value(ValueRef::Blob(&encode_dec(&d)), "x", Some(T::Int)).is_err());
    assert!(decode_value(ValueRef::Blob(&encode_dec(&d)), "x", None).is_err());
    assert!(decode_value(ValueRef::Text(&[0xFF]), "s", Some(T::String)).is_err());
}

#[test]
fn param_round_trip() {
    use rusqlite::types::Value as SqlValue;
    let t = instant(1_700_000_000, 42);
    let d = TimeDelta::milliseconds(-1500);
    let cases = [
        (ParamValue::Int(-9), ScalarType::Int),
        (ParamValue::Float(2.5), ScalarType::Float),
        (ParamValue::Dec(dec("-0.50")), ScalarType::Dec),
        (ParamValue::String("hi".to_string()), ScalarType::String),
        (ParamValue::Bool(true), ScalarType::Bool),
        (ParamValue::Instant(t), ScalarType::Instant),
        (ParamValue::Duration(d), ScalarType::Duration),
        (ParamValue::Null, ScalarType::Int),
    ];
    for (param, ty) in cases {
        let sql = encode_param(&param);
        let value = match &sql {
            SqlValue::Null => decode_value(ValueRef::Null, "c", Some(ty)).unwrap(),
            SqlValue::Integer(n) => decode_value(ValueRef::Integer(*n), "c", Some(ty)).unwrap(),
            SqlValue::Real(f) => decode_value(ValueRef::Real(*f), "c", Some(ty)).unwrap(),
            SqlValue::Text(s) => decode_value(ValueRef::Text(s.as_bytes()), "c", Some(ty)).unwrap(),
            SqlValue::Blob(b) => decode_value(ValueRef::Blob(b), "c", Some(ty)).unwrap(),
        };
        let expected = match param {
            ParamValue::Null => Value::Null,
            ParamValue::Int(n) => Value::Int(n),
            ParamValue::Float(f) => Value::Float(f),
            ParamValue::Dec(x) => Value::Dec(x),
            ParamValue::String(s) => Value::String(s),
            ParamValue::Bool(b) => Value::Bool(b),
            ParamValue::Instant(x) => Value::Instant(x),
            ParamValue::Duration(x) => Value::Duration(x),
        };
        assert_eq!(value, expected);
    }
}


#[test]
fn runtime_config_defaults_match_schemaless() {
    let cfg = RuntimeConfig::default();
    assert!(cfg.float_checks);
    assert_eq!(
        cfg,
        RuntimeConfig {
            int_arithmetic: grove_runtime::IntArithmetic::Checked,
            float_checks: true,
            dec_arithmetic: grove_runtime::DecArithmetic::Checked,
        }
    );
}

#[test]
fn value_display_smoke() {
    assert_eq!(Value::Null.to_string(), "none");
    assert_eq!(Value::Int(-3).to_string(), "-3");
    assert_eq!(Value::Bool(true).to_string(), "true");
    assert_eq!(Value::Dec(dec("5.0")).to_string(), "5.0");
    assert_eq!(
        Value::List(vec![Value::Int(1), Value::Null]).to_string(),
        "[1, none]"
    );
    assert_eq!(
        Value::Record(vec![("a".to_string(), Value::Int(1))]).to_string(),
        "{a: 1}"
    );
}
