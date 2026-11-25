use crate::eval::harness::*;
use crate::testdata;
use crate::value::{Scalar, Val};
use grove_types::Span;
use num_traits::ToPrimitive;
use rust_decimal::{Decimal, dec};

#[test]
fn eval_ordered_comparisons() {
    for (src, expected) in [
        ("1 < 2", true),
        ("2 <= 2", true),
        ("3 > 4", false),
        ("2 >= 3", false),
        ("1 < 1", false),
        ("1.5f < 2.0f", true),
        ("2.0f <= 2.0f", true),
        ("1.5 < 2.5", true),
        ("30.00 < 30.01", true),
        ("\"a\" < \"b\"", true),
        ("\"b\" <= \"b\"", true),
        ("\"abc\" > \"abd\"", false),
        (r#"hex"00" < hex"0100""#, true),
        (r#"hex"00" <= hex"00""#, true),
        (r#"hex"0000" > hex"00""#, true),
        (r#"hex"ff" > hex"0100""#, true),
        ("false < true", true),
        ("true <= true", true),
        ("@now < @3000-01-02", true),
        ("#30s > #1s", true),
        ("#30s >= #30s", true),
    ] {
        assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
    }
}

#[test]
fn eval_ordered_tuples() {
    use std::cmp::Ordering;
    let span = Span::new(0, 0);
    assert_eq!(
        Val::Tuple(vec![int(1), str("a")]).cmp(&Val::Tuple(vec![int(1), str("b")]), span),
        Ok(Ordering::Less)
    );
    assert_eq!(
        Val::Tuple(vec![int(1), str("a")]).cmp(&Val::Tuple(vec![int(1), str("a")]), span),
        Ok(Ordering::Equal)
    );
    assert_eq!(
        Val::Tuple(vec![int(2), str("a")]).cmp(&Val::Tuple(vec![int(1), str("z")]), span),
        Ok(Ordering::Greater)
    );
    assert!(
        Val::Tuple(vec![int(1)])
            .cmp(&Val::Tuple(vec![int(1), str("a")]), span)
            .is_err()
    );
}

#[test]
fn eval_ordered_nan_aborts() {
    let off = r#"config { float_checks = false }"#;
    let err = run_src_config("(0.0f / 0.0f) < 1.0f", off).unwrap_err();
    assert!(err.to_string().contains("NaN"), "{err}");
}

#[test]
fn eval_equality() {
    for (src, expected) in [
        ("1 == 1", true),
        ("1 != 2", true),
        ("1 == 2", false),
        ("1.5f == 1.5f", true),
        ("\"a\" == \"a\"", true),
        ("\"a\" != \"b\"", true),
        ("true == true", true),
        ("true != false", true),
        ("30.00 == 30.0", true),
        ("@now == @now", true),
        ("#30s == #30s", true),
        (r#"hex"00" == hex"00""#, true),
        (r#"hex"00" != hex"01""#, true),
        (r#"hex"0000" == hex"00""#, false),
        ("some(1) == some(1)", true),
        ("some(1) != some(2)", true),
        ("none == some(2)", false),
        ("some(some(\"a\")) != some(none)", true),
    ] {
        assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
    }
}

#[test]
fn eval_equality_non_owning_refs_ignored() {
    assert_eq!(run_seeded("users == users"), Ok(bool(true)));
    assert_eq!(run_seeded("users != users"), Ok(bool(false)));
    assert_eq!(run_seeded("users in (users,)"), Ok(bool(true)));
}

#[test]
fn eval_equality_nan() {
    let off = r#"config { float_checks = false }"#;
    assert_eq!(
        run_src_config("(0.0f / 0.0f) == (0.0f / 0.0f)", off),
        Ok(bool(false))
    );
    assert_eq!(
        run_src_config("(0.0f / 0.0f) != (0.0f / 0.0f)", off),
        Ok(bool(true))
    );
}

#[test]
fn eval_equality_structural() {
    assert_eq!(Val::None.eq(&Val::None), Some(true));
    assert_eq!(
        Val::list(vec![int(1), Val::None]).eq(&Val::list(vec![int(1), Val::None])),
        Some(true)
    );
    assert_eq!(
        Val::list(vec![int(1)]).eq(&Val::list(vec![int(1), int(2)])),
        Some(false)
    );
    assert_eq!(
        Val::Scalar(Scalar::Int(1)).eq(&Val::Scalar(Scalar::Bool(true))),
        None
    );
}

#[test]
fn eval_in() {
    for (src, expected) in [
        ("1 in (1, 2, false)", true),
        ("9 in (1, 2, 3)", false),
        ("\"a\" in ([true], 1.0, \"a\")", true),
        ("\"z\" in (\"a\", 0.0f)", false),
        (r#"hex"00" in (hex"00", hex"01")"#, true),
        (r#"hex"02" in (hex"00", hex"01")"#, false),
        ("2 in (1, \"a\")", false),
        ("\"b\" in (1, \"a\")", false),
        ("some(1) in (some(1), 2)", true),
        ("some(3) in (some(2), 3)", false),
        ("{ b = 2, a = 1 } in ({ a = 1, b = 2 }, { a = 0 })", true),
        ("{ b = 2, a = 9 } in ({ a = 1, b = 2 }, { a = 0 })", false),
    ] {
        assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
    }
}

#[test]
fn eval_in_composite() {
    let names = |src: &str| match run_seeded(src) {
        Ok(Val::List(rows)) => rows
            .into_iter()
            .map(|row| match row {
                Val::Record(fields) => fields
                    .into_iter()
                    .find_map(|(name, value)| (name.as_ref() == "name").then_some(value))
                    .expect("expected `name` field"),
                other => panic!("expected record, got {other}"),
            })
            .collect::<Vec<_>>(),
        got => panic!("expected rows for {src}, got {got:?}"),
    };
    assert_eq!(names("users[id in (1, 3)]"), [str("Ada"), str("Cyd")]);
    assert_eq!(
        names("users[name in (\"Ada\", \"Eli\")]"),
        [str("Ada"), str("Eli")]
    );
    assert_eq!(names("users[id in (2, \"x\")]"), [str("Bob")]);
    assert_eq!(
        names("users[if nickname.is_some() {nickname.unwrap() in (\"A\", \"C\")} else {false}]"),
        [str("Ada"), str("Cyd")]
    );
    assert_eq!(
        names("users[nickname in (none, some(\"A\"))]"),
        [str("Ada"), str("Bob"), str("Dan")]
    );
}

#[test]
fn eval_logic() {
    for (src, expected) in [
        ("true && true", true),
        ("true && false", false),
        ("false && true", false),
        ("true || false", true),
        ("false || false", false),
        ("true || false && false", true),
        ("(true || false) && false", false),
        ("!false", true),
        ("!!true", true),
        ("!(1 != 2)", false),
    ] {
        assert_eq!(run_src(src), Ok(bool(expected)), "{src}");
    }
}

#[test]
fn eval_logic_short_circuit() {
    assert_eq!(run_src("false && (1 / 0 == 0)"), Ok(bool(false)));
    assert_eq!(run_src("true || (1 / 0 == 0)"), Ok(bool(true)));
    assert!(
        run_src("true && (1 / 0 == 0)")
            .unwrap_err()
            .to_string()
            .contains("zero")
    );
}

#[test]
fn eval_infallible_casts() {
    for (src, expected) in [
        ("5 as Float", float(5.0)),
        ("5 as Dec", dec(dec!(5))),
        ("5 as Int", int(5)),
        ("1.9f as Int", int(1)),
        ("-1.9f as Int", int(-1)),
        ("1e300f as Int", int(i64::MAX)),
        ("(-1e300f) as Int", int(i64::MIN)),
        ("1.5f as Dec", dec(dec!(1.5))),
        ("1e300f as Dec", dec(Decimal::MAX)),
        ("5.9 as Int", int(5)),
        ("-5.9 as Int", int(-5)),
        ("30.00 as Int", int(30)),
        ("Dec::MAX as Int", int(i64::MAX)),
        ("(-Dec::MAX) as Int", int(i64::MIN)),
        ("-Dec::MAX as Int", int(i64::MIN + 1)),
        ("Dec::MIN as Float", float(Decimal::MIN.to_f64().unwrap())),
        ("5.5 as Float", float(5.5)),
        ("true as Int", int(1)),
        ("false as Int", int(0)),
        ("42 as String", str("42")),
        ("3.14f as String", str("3.14f")),
        ("1.0f as String", str("1.0f")),
        ("true as String", str("true")),
        ("30.00 as String", str("30.0")),
        ("2.5 as String", str("2.5")),
        ("(123456789 as Dec) as String", str("123456789.0")),
        (
            "@2026-10-20_14:30:45 as String",
            str("2026-10-20T14:30:45Z"),
        ),
        (r#""hello" as Bytes"#, bytes(b"hello")),
        ("\"é\" as Bytes", bytes(&[0xc3, 0xa9])),
    ] {
        assert_eq!(run_src(src), Ok(expected), "{src}");
    }
}

#[test]
fn eval_fallible_casts() {
    for (src, expected) in [
        (r#""42" as? Int"#, some(int(42))),
        (r#""-7" as? Int"#, some(int(-7))),
        (r#""9223372036854775808" as? Int"#, Val::None),
        (r#""oops" as? Int"#, Val::None),
        (r#""1.5" as? Float"#, some(float(1.5))),
        (r#""1.23e-45" as? Float"#, some(float(1.23e-45))),
        (r#""NaN" as? Float"#, Val::None),
        (r#""oops" as? Float"#, Val::None),
        (r#""6.4" as? Dec"#, some(dec(dec!(6.4)))),
        (r#""-5.43e21" as? Dec"#, some(dec(dec!(-5.43e21)))),
        (r#""oops" as? Dec"#, Val::None),
        (r#""TrUe" as? Bool"#, some(bool(true))),
        (r#""FALSE" as? Bool"#, some(bool(false))),
        (r#""1" as? Bool"#, Val::None),
        (r#"" true" as? Bool"#, Val::None),
        ("0 as? Bool", some(bool(false))),
        ("1 as? Bool", some(bool(true))),
        ("2 as? Bool", Val::None),
        (r#"hex"6869" as? String"#, some(str("hi"))),
        (r#"hex"ff" as? String"#, Val::None),
        ("1.5f as? Int", Val::None),
        ("(-1.0f) as? Int", some(int(-1))),
        ("1e300f as? Int", Val::None),
        ("3.5 as? Int", Val::None),
        ("2.0 as? Int", some(int(2))),
        ("Dec::MAX as? Int", Val::None),
        ("1.54321f as? Dec", some(dec(dec!(1.54321)))),
        ("1.2e-15f as? Dec", some(dec(dec!(1.2e-15)))),
        ("1e300f as? Dec", Val::None),
    ] {
        assert_eq!(run_src(src), Ok(expected), "{src}");
    }

    let off = r#"config { float_checks = false }"#;
    match run_src_config(r#""NaN" as? Float"#, off) {
        Ok(Val::Some(inner)) => match inner.as_ref() {
            Val::Scalar(Scalar::Float(value)) => assert!(value.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        },
        other => panic!("expected some(NaN), got {other:?}"),
    }
    assert_eq!(
        run_src_config(r#""inf" as? Float"#, off),
        Ok(some(float(f64::INFINITY)))
    );
}

#[test]
fn eval_fallible_instant_casts() {
    use chrono::{DateTime, Utc};

    let instant = |rfc3339: &str| {
        some(Val::Scalar(Scalar::Instant(
            DateTime::parse_from_rfc3339(rfc3339)
                .unwrap()
                .with_timezone(&Utc),
        )))
    };
    for (src, expected) in [
        ("2024-01-15T12:34:56Z", None),
        ("2024-01-15T12:34:56.123Z", None),
        ("2024-01-15T12:34:56+02:00", None),
        ("2024-01-15T12:34:56.123+02:00", None),
        ("2024-01-15 12:34:56", Some("2024-01-15T12:34:56Z")),
        ("2024-01-15 12:34:56.123", Some("2024-01-15T12:34:56.123Z")),
        ("2024-01-15 12:34:56Z", Some("2024-01-15T12:34:56Z")),
        (
            "2024-01-15 12:34:56.123+02:00",
            Some("2024-01-15T12:34:56.123+02:00"),
        ),
        ("2024-01-15", Some("2024-01-15T00:00:00Z")),
    ] {
        assert_eq!(
            run_src(&format!("\"{src}\" as? Instant")),
            Ok(instant(expected.unwrap_or(src))),
            "{src}"
        );
    }
    for src in [
        r#""01/02/2024" as? Instant"#,
        r#""2024-01-15 12:34" as? Instant"#,
        r#""2024-01-15T12:34:56" as? Instant"#,
        r#""yesterday" as? Instant"#,
    ] {
        assert_eq!(run_src(src), Ok(Val::None), "{src}");
    }
}

#[test]
fn eval_fallible_enum_casts() {
    let schema = testdata::schema();
    let priority = testdata::enum_id(&schema, "Priority");
    let level = testdata::enum_id(&schema, "Level");
    assert_eq!(
        run_seeded(r#""low" as? Priority"#),
        Ok(some(Val::Enum(priority, 0)))
    );
    assert_eq!(
        run_seeded(r#""high" as? Priority"#),
        Ok(some(Val::Enum(priority, 2)))
    );
    assert_eq!(run_seeded(r#""High" as? Priority"#), Ok(Val::None));
    assert_eq!(run_seeded("2 as? Level"), Ok(some(Val::Enum(level, 2))));
    assert_eq!(run_seeded("3 as? Level"), Ok(Val::None));
}

#[test]
fn eval_type_annotations() {
    assert_eq!(
        run_src("([] as List<Int>).extend([1, 2, 3])"),
        Ok(Val::list(vec![int(1), int(2), int(3)]))
    );
    assert_eq!(run_src("none as ?String"), Ok(Val::None));
    assert_eq!(
        run_src("some(none) as ??Int"),
        Ok(Val::Some(Box::new(Val::None)))
    );
    assert_eq!(
        run_src("[] as List<(?Int, List<Duration>, ??String, {a: Instant, d: Dec})>"),
        Ok(Val::list(Vec::new()))
    );
}

#[test]
fn eval_bytes_sort_filter_extrema() {
    assert_eq!(
        run_src(r#"[hex"02", hex"01"].sort()"#),
        Ok(Val::list(vec![bytes(&[0x01]), bytes(&[0x02])]))
    );
    assert_eq!(
        run_src(r#"[hex"02", hex"01"].sort_desc()"#),
        Ok(Val::list(vec![bytes(&[0x02]), bytes(&[0x01])]))
    );
    assert_eq!(
        run_src(r#"[hex"02", hex"01"].min()"#),
        Ok(some(bytes(&[0x01])))
    );
    assert_eq!(
        run_src(r#"[hex"02", hex"01"].max()"#),
        Ok(some(bytes(&[0x02])))
    );
    assert_eq!(
        run_src(r#"[hex"00", hex"01"][$ >= hex"01"]"#),
        Ok(Val::list(vec![bytes(&[0x01])]))
    );
}

#[test]
fn eval_cast_nan_to_dec_aborts() {
    let off = r#"config { float_checks = false }"#;
    let err = run_src_config("(0.0f / 0.0f) as Dec", off).unwrap_err();
    assert!(err.to_string().contains("NaN"), "{err}");
}
