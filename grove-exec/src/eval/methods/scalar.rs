use chrono::{DateTime, Datelike, SecondsFormat, Timelike, Utc};
use grove_types::Span;
use rust_decimal::{Decimal, RoundingStrategy};

use super::{method_int_arg, method_string_arg};
use crate::error::ExecError;
use crate::value::{
    NANOS_PER_DAY, NANOS_PER_HOUR, NANOS_PER_MIN, NANOS_PER_SEC, Scalar, Val, total_nanos,
};

fn second_dec(dt: &DateTime<Utc>) -> Decimal {
    Decimal::new(
        dt.second() as i64 * 1_000_000_000 + dt.nanosecond() as i64,
        9,
    )
    .normalize()
}

fn epoch_dec(dt: &DateTime<Utc>) -> Decimal {
    Decimal::from_i128_with_scale(
        dt.timestamp() as i128 * 1_000_000_000 + dt.nanosecond() as i128,
        9,
    )
    .normalize()
}

fn duration_as(total_nanos: Decimal, divisor: i64) -> Scalar {
    Scalar::Dec(
        total_nanos
            .checked_div(Decimal::from(divisor))
            .expect("as_* divides nanos by a nonzero constant")
            .normalize(),
    )
}

pub(super) fn eval_scalar_method(
    base: &Val,
    name: &str,
    args: &[Val],
    span: Span,
) -> Result<Scalar, ExecError> {
    let Val::Scalar(base) = base else {
        return Err(ExecError::Integrity {
            message: format!("method `{}` on unsupported value (internal error)", name),
        });
    };
    match (base, name) {
        (Scalar::String(s), "contains") => {
            Ok(Scalar::Bool(s.contains(method_string_arg(args, name)?)))
        }
        (Scalar::String(s), "starts_with") => {
            Ok(Scalar::Bool(s.starts_with(method_string_arg(args, name)?)))
        }
        (Scalar::String(s), "ends_with") => {
            Ok(Scalar::Bool(s.ends_with(method_string_arg(args, name)?)))
        }
        (Scalar::String(s), "to_upper") => Ok(Scalar::String(s.to_uppercase())),
        (Scalar::String(s), "to_lower") => Ok(Scalar::String(s.to_lowercase())),
        (Scalar::String(s), "len") => Ok(Scalar::Int(s.chars().count() as i64)),
        (Scalar::Bytes(bytes), "len") => Ok(Scalar::Int(bytes.len() as i64)),
        (Scalar::Instant(t), "year") => Ok(Scalar::Int(t.year() as i64)),
        (Scalar::Instant(t), "month") => Ok(Scalar::Int(t.month() as i64)),
        (Scalar::Instant(t), "day") => Ok(Scalar::Int(t.day() as i64)),
        (Scalar::Instant(t), "hour") => Ok(Scalar::Int(t.hour() as i64)),
        (Scalar::Instant(t), "minute") => Ok(Scalar::Int(t.minute() as i64)),
        (Scalar::Instant(t), "weekday") => {
            Ok(Scalar::Int(t.weekday().num_days_from_sunday() as i64))
        }
        (Scalar::Instant(t), "second") => Ok(Scalar::Dec(second_dec(t))),
        (Scalar::Instant(t), "epoch") => Ok(Scalar::Dec(epoch_dec(t))),
        (Scalar::Instant(t), "to_rfc3339") => Ok(Scalar::String(
            t.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )),
        (Scalar::Duration(d), "as_seconds") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_SEC as i64,
        )),
        (Scalar::Duration(d), "as_minutes") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_MIN as i64,
        )),
        (Scalar::Duration(d), "as_hours") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_HOUR as i64,
        )),
        (Scalar::Duration(d), "as_days") => Ok(duration_as(
            Decimal::from_i128_with_scale(total_nanos(d), 0),
            NANOS_PER_DAY as i64,
        )),
        (Scalar::Dec(d), "round_dp") => {
            let n = method_int_arg(args, name)?;
            if n < 0 {
                return Err(ExecError::Abort {
                    message: format!("round_dp needs non-negative digits, got {n}"),
                    span,
                });
            }
            if n > 28 {
                Ok(Scalar::Dec(*d))
            } else {
                Ok(Scalar::Dec(
                    d.round_dp_with_strategy(n as u32, RoundingStrategy::MidpointNearestEven)
                        .normalize(),
                ))
            }
        }
        (scalar, "to_string")
            if matches!(
                scalar,
                Scalar::Int(_) | Scalar::Float(_) | Scalar::Bool(_) | Scalar::Dec(_)
            ) =>
        {
            Ok(Scalar::String(scalar.to_string()))
        }
        (Scalar::Float(x), "round") => Ok(Scalar::Float(x.round())),
        (Scalar::Float(x), "floor") => Ok(Scalar::Float(x.floor())),
        (Scalar::Float(x), "ceil") => Ok(Scalar::Float(x.ceil())),
        _ => Err(ExecError::Integrity {
            message: format!(
                "unknown method `{name}` for {} (internal error)",
                base.kind_str(),
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};
    use rust_decimal::dec;

    #[test]
    fn eval_string_methods() {
        for (src, expected) in [
            (r#""hello".contains("ell")"#, bool(true)),
            (r#""hello".contains("z")"#, bool(false)),
            (r#""hello".contains("ELL")"#, bool(false)),
            (r#""hello".contains("")"#, bool(true)),
            (r#""".contains("")"#, bool(true)),
            (r#""hello".starts_with("he")"#, bool(true)),
            (r#""hello".starts_with("lo")"#, bool(false)),
            (r#""hello".ends_with("lo")"#, bool(true)),
            (r#""hello".ends_with("he")"#, bool(false)),
            (r#""HeLLo".to_upper()"#, str("HELLO")),
            (r#""HeLLo".to_lower()"#, str("hello")),
            (r#""ß".to_upper()"#, str("SS")),
            (r#""hello".len()"#, int(5)),
            (r#""".len()"#, int(0)),
            (r#""héllo".len()"#, int(5)),
            (r#""😀".len()"#, int(1)),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_bytes_methods() {
        for (src, expected) in [
            (r#"hex"deadbeef_0011 23 45".len()"#, int(8)),
            (r#"b64"3q2+7w==".len()"#, int(4)),
            (r#"hex"".len()"#, int(0)),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_instant_methods() {
        for (src, expected) in [
            ("@2026-10-20.year()", int(2026)),
            ("@2026-10-20.month()", int(10)),
            ("@2026-10-20.day()", int(20)),
            ("@2026-10-20.hour()", int(0)),
            ("@2026-10-20.minute()", int(0)),
            ("@2026-10-20.weekday()", int(2)),
            ("@2026-10-25.weekday()", int(0)),
            ("@2026-10-20_14:30:45.hour()", int(14)),
            ("@2026-10-20_14:30:45.minute()", int(30)),
            ("@2026-10-20_14:30:45.second()", dec(dec!(45))),
            (
                "@2026-10-20_14:30:45.123456789.second()",
                dec(dec!(45.123456789)),
            ),
            ("@1970-01-01.epoch()", dec(dec!(0))),
            ("@1970-01-02.epoch()", dec(dec!(86400))),
            ("@1970-01-01_00:00:01.5.epoch()", dec(dec!(1.5))),
            (
                "@2026-10-20_14:30:45.to_rfc3339()",
                str("2026-10-20T14:30:45Z"),
            ),
            (
                "@2026-10-20_14:30:45.5.to_rfc3339()",
                str("2026-10-20T14:30:45.500Z"),
            ),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_duration_methods() {
        for (src, expected) in [
            ("#90s.as_seconds()", dec(dec!(90))),
            ("#1.5s.as_seconds()", dec(dec!(1.5))),
            ("(-#90s).as_seconds()", dec(dec!(-90))),
            ("#3600s.as_minutes()", dec(dec!(60))),
            ("#3600s.as_hours()", dec(dec!(1))),
            ("#86400s.as_days()", dec(dec!(1))),
            ("#90s.as_hours()", dec(dec!(0.025))),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        match run_src("#1s.as_minutes()") {
            Ok(Val::Scalar(Scalar::Dec(d))) => {
                assert_eq!(d.scale(), 28);
                assert!(d.to_string().starts_with("0.016666"), "{d}");
            }
            got => panic!("expected Dec, got {got:?}"),
        }
    }

    #[test]
    fn eval_dec_methods() {
        for (src, expected) in [
            ("2.5.round_dp(0)", dec(dec!(2))),
            ("3.5.round_dp(0)", dec(dec!(4))),
            ("2.675.round_dp(2)", dec(dec!(2.68))),
            ("2.665.round_dp(2)", dec(dec!(2.66))),
            ("2.567.round_dp(2)", dec(dec!(2.57))),
            ("1.5.round_dp(100)", dec(dec!(1.5))),
            ("30.00.to_string()", str("30.0")),
            ("2.5.to_string()", str("2.5")),
            ("(123456789 as Dec).to_string()", str("123456789.0")),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
        let err = run_src("2.5.round_dp(-1)").unwrap_err();
        assert!(err.to_string().contains("round_dp"), "{err}");
    }

    #[test]
    fn eval_scalar_to_string() {
        for (src, expected) in [
            ("42.to_string()", str("42")),
            ("3.14f.to_string()", str("3.14")),
            ("1.0f.to_string()", str("1.0")),
            ("true.to_string()", str("true")),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }

    #[test]
    fn eval_float_rounding() {
        for (src, expected) in [
            ("2.5f.round()", float(3.0)),
            ("(-2.5f).round()", float(-3.0)),
            ("2.1f.round()", float(2.0)),
            ("2.9f.floor()", float(2.0)),
            ("(-2.1f).floor()", float(-3.0)),
            ("2.1f.ceil()", float(3.0)),
            ("(-2.9f).ceil()", float(-2.0)),
        ] {
            assert_eq!(run_src(src), Ok(expected), "{src}");
        }
    }
}
