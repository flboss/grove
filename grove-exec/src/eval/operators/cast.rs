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

#[cfg(test)]
mod tests {
    use crate::eval::harness::{
        bool, bytes, dec, float, int, run_seeded, run_src, run_src_config, some, str,
    };
    use crate::testdata;
    use crate::value::{Scalar, Val};
    use num_traits::ToPrimitive;
    use rust_decimal::{Decimal, dec};

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
    fn eval_cast_nan_to_dec_aborts() {
        let off = r#"config { float_checks = false }"#;
        let err = run_src_config("(0.0f / 0.0f) as Dec", off).unwrap_err();
        assert!(err.to_string().contains("NaN"), "{err}");
    }
}
