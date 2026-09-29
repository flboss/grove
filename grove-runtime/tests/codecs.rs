
use chrono::{DateTime, TimeDelta, Utc};
use grove_query::codegen::model::{ParamValue, ScalarType};
use grove_runtime::decode::{
    decode_value, encode_dec, encode_duration, encode_instant, encode_param,
};
use grove_runtime::{
    RuntimeConfig, Value, assemble_dec, decode_dec, decode_duration, decode_instant,
};
use rusqlite::types::ValueRef;
use rust_decimal::Decimal;
use std::str::FromStr;


fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[test]
fn dec_round_trip_normalized() {
    let cases = [
        ("0", "0"),
        ("0.00", "0"),
        ("-0.00", "0"),
        ("1", "1"),
        ("-1", "-1"),
        ("5.00", "5"),
        ("2.50", "2.5"),
        ("-5.00", "-5"),
        ("10", "10"),
        ("100", "100"),
        ("1000", "1000"),
        ("250", "250"),
        ("2500", "2500"),
        ("250.00", "250"),
        ("0.10", "0.1"),
        ("123.456", "123.456"),
        ("-123.456", "-123.456"),
        ("123.450", "123.45"),
        (
            "79228162514264337593543950335",
            "79228162514264337593543950335",
        ),
        (
            "-79228162514264337593543950335",
            "-79228162514264337593543950335",
        ),
        (
            "79228162514264337593543950330",
            "79228162514264337593543950330",
        ),
        (
            "10000000000000000000000000000",
            "10000000000000000000000000000",
        ),
        (
            "0.0000000000000000000000000001",
            "0.0000000000000000000000000001",
        ),
        (
            "-0.0000000000000000000000000001",
            "-0.0000000000000000000000000001",
        ),
        (
            "12345678901234567890123456789",
            "12345678901234567890123456789",
        ),
        (
            "3.1415926535897932384626433833",
            "3.1415926535897932384626433833",
        ),
        ("9.99", "9.99"),
        ("99.99", "99.99"),
        ("0.5", "0.5"),
        ("-0.5", "-0.5"),
    ];
    for (input, normalized) in cases {
        let d = dec(input);
        let bytes = encode_dec(&d);
        assert_eq!(bytes.len(), 13, "13-byte encoding of {input}");
        let rt = decode_dec(&bytes, "c").unwrap();
        assert_eq!(rt, d, "numeric round-trip of {input}");
        assert_eq!(rt.to_string(), normalized, "normalized display of {input}");
        assert_eq!(encode_dec(&rt), bytes, "encode stability of {input}");
    }
}

#[test]
fn dec_equal_values_share_bytes() {
    let groups = [
        ["0", "0.00", "-0.00", "0.000"],
        ["5", "5.0", "5.00", "5.000"],
        ["-2.5", "-2.50", "-2.5", "-2.5"],
        ["250", "250.0", "250.00", "250.000"],
        ["10", "10.0", "10.00", "10.000"],
    ];
    for group in groups {
        let first = encode_dec(&dec(group[0]));
        for other in group.iter().skip(1) {
            assert_eq!(encode_dec(&dec(other)), first, "byte identity in {group:?}");
        }
    }
    assert_ne!(
        encode_dec(&dec("1")),
        encode_dec(&dec("1.0000000000000000000000000001"))
    );
    assert_ne!(encode_dec(&dec("-1")), encode_dec(&dec("1")));
}

#[test]
fn dec_ordering_matches_numeric() {
    let mut values = vec![
        dec("-1000"),
        dec("-250"),
        dec("-99.99"),
        dec("-10"),
        dec("-2"),
        dec("-1.001"),
        dec("-1"),
        dec("-0.5"),
        dec("-0.0000000000000000000000000001"),
        dec("-0.00"),
        dec("0"),
        dec("0.00"),
        dec("0.0000000000000000000000000001"),
        dec("0.5"),
        dec("1"),
        dec("1.001"),
        dec("2"),
        dec("9.99"),
        dec("10"),
        dec("25"),
        dec("99.99"),
        dec("100"),
        dec("250"),
        dec("999.99"),
        dec("1000"),
        dec("79228162514264337593543950330"),
        dec("79228162514264337593543950335"),
    ];
    values.reverse();
    let encoded: Vec<_> = values.iter().map(encode_dec).collect();
    for bytes in &encoded {
        assert_eq!(bytes.len(), 13);
    }
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
fn dec_never_panics_on_untrusted_blobs() {
    let payloads: &[[u8; 12]] = &[
        [0; 12],
        [0xFF; 12],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        [
            0x45, 0x38, 0x6B, 0xCD, 0xE4, 0x6B, 0xD2, 0x8D, 0x99, 0xA0, 0x00, 0x00,
        ],
    ];
    for first in 0..=0xFFu8 {
        for payload in payloads {
            let mut blob = [0u8; 13];
            blob[0] = first;
            blob[1..13].copy_from_slice(payload);
            let _ = decode_dec(&blob, "c");
        }
    }
    for exp in [i8::MIN, -100, -29, -28, 0, 28, 29, 100, i8::MAX] {
        for field in [
            0u128,
            1,
            999,
            (1 << 96) - 1,
            1 << 96,
            (1 << 97) - 1,
            u128::MAX,
        ] {
            let _ = decode_dec(&assemble_dec(false, exp, field), "c");
            let _ = decode_dec(&assemble_dec(true, exp, field), "c");
        }
    }
}

#[test]
fn dec_malformed_is_decode_error() {
    let good = encode_dec(&dec("1.5"));
    for len in [0, 1, 12] {
        assert!(
            decode_dec(&good[..len], "c").is_err(),
            "length {len} rejected"
        );
    }
    let mut long = good.to_vec();
    long.push(0);
    assert!(decode_dec(&long, "c").is_err());
    assert!(decode_dec(&assemble_dec(false, 35, 5), "c").is_err());
    assert!(decode_dec(&assemble_dec(true, 35, 5), "c").is_err());
    assert!(decode_dec(&assemble_dec(false, 28, (1 << 97) - 1), "c").is_err());
    assert!(decode_dec(&assemble_dec(false, 28, 5), "c").is_err());
    assert!(decode_dec(&assemble_dec(false, -28, 123), "c").is_err());
    assert!(decode_dec(&assemble_dec(false, 28, 9 * 10u128.pow(28)), "c").is_err());
    assert!(decode_dec(&assemble_dec(true, -28, 123), "c").is_err());
}

#[test]
fn dec_assembled_edges_decode() {
    let cases: &[(bool, i8, u128, &str)] = &[
        (
            false,
            28,
            5 * 10u128.pow(28),
            "50000000000000000000000000000",
        ),
        (false, 0, 15 * 10u128.pow(27), "1.5"),
        (true, 0, 15 * 10u128.pow(27), "-1.5"),
        (false, -28, 1, "0.0000000000000000000000000001"),
        (true, -28, 1, "-0.0000000000000000000000000001"),
        (false, 2, 25 * 10u128.pow(26), "250"),
        (false, -28, 0, "0"),
        (true, 5, 0, "0"),
    ];
    for (negative, exp, field, expected) in cases {
        let rt = decode_dec(&assemble_dec(*negative, *exp, *field), "c").unwrap();
        assert_eq!(
            rt.to_string(),
            *expected,
            "assembled {negative}/{exp}/{field}"
        );
    }
    assert_eq!(
        assemble_dec(false, 0, 15 * 10u128.pow(27)),
        encode_dec(&dec("1.5"))
    );
    assert_eq!(
        assemble_dec(true, 0, 15 * 10u128.pow(27)),
        encode_dec(&dec("-1.5"))
    );
    assert_eq!(
        assemble_dec(false, 2, 250 * 10u128.pow(26)),
        encode_dec(&dec("250"))
    );
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
fn temporal_malformed_is_decode_error() {
    use grove_runtime::decode::{encode_duration, encode_instant};
    let good = encode_instant(&instant(0, 0));
    for len in [0, 11] {
        assert!(decode_instant(&good[..len], "c").is_err());
    }
    let mut long = good.to_vec();
    long.push(0);
    assert!(decode_instant(&long, "c").is_err());
    let good = encode_duration(&TimeDelta::zero());
    assert!(decode_duration(&good[..11], "c").is_err());
    let mut bad = encode_instant(&instant(1_700_000_000, 0));
    bad[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    assert!(decode_instant(&bad, "c").is_err());
    let mut bad = encode_duration(&TimeDelta::seconds(5));
    bad[8..12].copy_from_slice(&1_000_000_000u32.to_be_bytes());
    assert!(decode_duration(&bad, "c").is_err());
    let mut bad = encode_duration(&TimeDelta::seconds(5));
    let mut secs = i64::MAX.to_be_bytes();
    secs[0] ^= 0x80;
    bad[0..8].copy_from_slice(&secs);
    assert!(decode_duration(&bad, "c").is_err());
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
    assert_eq!(Value::Float(2.5).to_string(), "2.5");
    assert_eq!(
        Value::Instant(DateTime::from_timestamp(0, 0).unwrap()).to_string(),
        "1970-01-01T00:00:00Z"
    );
    assert_eq!(
        Value::Duration(TimeDelta::milliseconds(1500)).to_string(),
        "1.500000000s"
    );
    assert_eq!(Value::Duration(TimeDelta::seconds(90)).to_string(), "90s");
    assert_eq!(
        Value::List(vec![Value::Int(1), Value::Null]).to_string(),
        "[1, none]"
    );
    assert_eq!(
        Value::Tuple(vec![Value::Int(1), Value::Dec(dec("2"))]).to_string(),
        "(1, 2)"
    );
    assert_eq!(
        Value::Record(vec![("a".to_string(), Value::Int(1))]).to_string(),
        "{a: 1}"
    );
}
