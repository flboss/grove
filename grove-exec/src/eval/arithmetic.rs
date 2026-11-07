use std::fmt;

use chrono::TimeDelta;
use grove_query::ast::BinaryOp;
use grove_schema::validated::{DecArithmetic, IntArithmetic};
use grove_types::Span;
use num_traits::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::error::ExecError;
use crate::value::{NANOS_PER_SEC, Scalar, total_nanos};

#[derive(Clone, Copy)]
pub(super) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Mod,
}

impl ArithOp {
    pub(super) fn is_rhs_zero_undefined(&self) -> bool {
        matches!(self, ArithOp::Div | ArithOp::Rem | ArithOp::Mod)
    }

    pub(super) fn full_name(&self) -> &str {
        match self {
            ArithOp::Add => "addition",
            ArithOp::Sub => "subtraction",
            ArithOp::Mul => "multiplication",
            ArithOp::Div => "division",
            ArithOp::Rem => "C-style remainder",
            ArithOp::Mod => "euclidean remainder",
        }
    }
}

impl fmt::Display for ArithOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArithOp::Add => write!(f, "+"),
            ArithOp::Sub => write!(f, "-"),
            ArithOp::Mul => write!(f, "*"),
            ArithOp::Div => write!(f, "/"),
            ArithOp::Rem => write!(f, "%"),
            ArithOp::Mod => write!(f, "mod"),
        }
    }
}

impl TryFrom<BinaryOp> for ArithOp {
    type Error = ();

    fn try_from(value: BinaryOp) -> Result<Self, Self::Error> {
        match value {
            BinaryOp::Add => Ok(ArithOp::Add),
            BinaryOp::Sub => Ok(ArithOp::Sub),
            BinaryOp::Mul => Ok(ArithOp::Mul),
            BinaryOp::Div => Ok(ArithOp::Div),
            BinaryOp::Rem => Ok(ArithOp::Rem),
            BinaryOp::Mod => Ok(ArithOp::Mod),
            _ => Err(()),
        }
    }
}

pub(super) fn int_arith(
    a: i64,
    op: ArithOp,
    b: i64,
    mode: IntArithmetic,
    span: Span,
) -> Result<Scalar, ExecError> {
    if b == 0 && op.is_rhs_zero_undefined() {
        return Err(ExecError::Abort {
            message: format!("{} by zero: {a} {op} {b}", op.full_name()),
            span,
        });
    }
    let checked = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => a.checked_div(b),
        ArithOp::Rem => a.checked_rem(b),
        ArithOp::Mod => a.checked_rem_euclid(b),
    };
    let n = match checked {
        Some(n) => n,
        None => match mode {
            IntArithmetic::Checked => {
                return Err(ExecError::Abort {
                    message: format!("integer overflow: {a} {op} {b}"),
                    span,
                });
            }
            IntArithmetic::Saturating => match op {
                ArithOp::Add => a.saturating_add(b),
                ArithOp::Sub => a.saturating_sub(b),
                ArithOp::Mul => a.saturating_mul(b),
                ArithOp::Div => a.saturating_div(b),
                // i64::MIN % -1 is the only failure case
                ArithOp::Rem | ArithOp::Mod => 0,
            },
            IntArithmetic::Wrapping => match op {
                ArithOp::Add => a.wrapping_add(b),
                ArithOp::Sub => a.wrapping_sub(b),
                ArithOp::Mul => a.wrapping_mul(b),
                ArithOp::Div => a.wrapping_div(b),
                ArithOp::Rem | ArithOp::Mod => 0,
            },
        },
    };
    Ok(Scalar::Int(n))
}

pub(super) fn float_arith(
    a: f64,
    op: ArithOp,
    b: f64,
    float_checks: bool,
    span: Span,
) -> Result<Scalar, ExecError> {
    let f = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
        ArithOp::Mod => {
            let r = a % b;
            if r < 0.0 { r + b.abs() } else { r }
        }
    };
    if float_checks && !f.is_finite() {
        return Err(ExecError::Abort {
            message: format!("non-finite float result: {a} {op} {b}"),
            span,
        });
    }
    Ok(Scalar::Float(f))
}

pub(super) fn dec_arith(
    a: Decimal,
    op: ArithOp,
    b: Decimal,
    mode: DecArithmetic,
    span: Span,
) -> Result<Scalar, ExecError> {
    if b.is_zero() && op.is_rhs_zero_undefined() {
        return Err(ExecError::Abort {
            message: format!("{} by zero: {a} {op} {b}", op.full_name()),
            span,
        });
    }
    let checked = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => a.checked_div(b),
        ArithOp::Rem => a.checked_rem(b),
        ArithOp::Mod => a.checked_rem(b).and_then(|r| {
            if r.is_sign_negative() {
                r.checked_add(b.abs())
            } else {
                Some(r)
            }
        }),
    };
    let d = match checked {
        Some(d) => d,
        None => match mode {
            DecArithmetic::Checked => {
                return Err(ExecError::Abort {
                    message: format!("decimal overflow: {a} {op} {b}"),
                    span,
                });
            }
            DecArithmetic::Saturating => match op {
                ArithOp::Add | ArithOp::Sub => {
                    if a.is_sign_positive() {
                        Decimal::MAX
                    } else {
                        Decimal::MIN
                    }
                }
                ArithOp::Mul | ArithOp::Div => {
                    if a.is_sign_negative() == b.is_sign_negative() {
                        Decimal::MAX
                    } else {
                        Decimal::MIN
                    }
                }
                // Scale-up overflow occurred
                ArithOp::Rem | ArithOp::Mod => {
                    return Err(ExecError::Abort {
                        message: format!("decimal overflow: {a} {op} {b}"),
                        span,
                    });
                }
            },
        },
    };
    Ok(Scalar::Dec(d.normalize()))
}

/// Build a Duration from whole nanoseconds; out-of-range aborts
pub(super) fn duration_from_nanos(nanos: i128, span: Span) -> Result<TimeDelta, ExecError> {
    let secs = nanos.div_euclid(NANOS_PER_SEC as i128);
    let sub = nanos.rem_euclid(NANOS_PER_SEC as i128) as u32;
    i64::try_from(secs)
        .ok()
        .and_then(|secs| TimeDelta::new(secs, sub))
        .ok_or_else(|| ExecError::Abort {
            message: format!("duration overflow: {secs}.{sub:09}s"),
            span,
        })
}

/// Scale a duration by a decimal factor (banker's rounding to ns)
pub(super) fn scale_duration(
    duration: TimeDelta,
    factor: Decimal,
    is_div: bool,
    span: Span,
) -> Result<Scalar, ExecError> {
    if is_div && factor.is_zero() {
        return Err(ExecError::Abort {
            message: format!(
                "division by zero: {} / {factor}",
                Scalar::Duration(duration)
            ),
            span,
        });
    }
    let base = Decimal::from_i128_with_scale(total_nanos(&duration), 0);
    let scaled = if is_div {
        base.checked_div(factor)
    } else {
        base.checked_mul(factor)
    };
    let nanos = scaled
        .and_then(|scaled| {
            let rounded = scaled.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
            rounded.to_i128()
        })
        .ok_or_else(|| ExecError::Abort {
            message: format!(
                "duration overflow: {factor} {} {}",
                if is_div { "/" } else { "*" },
                Scalar::Duration(duration)
            ),
            span,
        })?;
    duration_from_nanos(nanos, span).map(Scalar::Duration)
}

#[cfg(test)]
mod tests {
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};
    use chrono::{DateTime, TimeDelta};
    use rust_decimal::{Decimal, dec};

    #[test]
    fn eval_int_arithmetic() {
        assert_eq!(run_src("1 + 2"), Ok(int(3)));
        assert_eq!(run_src("7 - 10"), Ok(int(-3)));
        assert_eq!(run_src("6 * 7"), Ok(int(42)));
        assert_eq!(run_src("7 / 2"), Ok(int(3)));
        assert_eq!(run_src("-7 / 2"), Ok(int(-3)));
        assert_eq!(run_src("-7 % 3"), Ok(int(-1)));
        assert_eq!(run_src("7 % -3"), Ok(int(1)));
        assert_eq!(run_src("-7 mod 3"), Ok(int(2)));
        assert_eq!(run_src("7 mod -3"), Ok(int(1)));
        assert_eq!(run_src("-5"), Ok(int(-5)));
        assert_eq!(run_src("--5"), Ok(int(5)));
    }

    #[test]
    fn eval_int_overflow_checked() {
        for src in [
            "Int::MAX + 1",
            "Int::MAX * 2",
            "Int::MIN - 1",
            "Int::MIN / -1",
            "-Int::MIN",
        ] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("integer overflow"), "{src}: {err}");
        }
        for src in ["1 / 0", "1 % 0", "1 mod 0"] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("zero"), "{src}: {err}");
        }
    }

    #[test]
    fn eval_int_saturating() {
        let sat = r#"config { int_arithmetic = "saturating" }"#;
        assert_eq!(run_src_config("Int::MAX + 1", sat), Ok(int(i64::MAX)));
        assert_eq!(run_src_config("Int::MIN - 1", sat), Ok(int(i64::MIN)));
        assert_eq!(run_src_config("Int::MIN / -1", sat), Ok(int(i64::MAX)));
        assert_eq!(run_src_config("-Int::MIN", sat), Ok(int(i64::MAX)));
        assert!(
            run_src_config("1 / 0", sat)
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_int_wrapping() {
        let wrap = r#"config { int_arithmetic = "wrapping" }"#;
        assert_eq!(run_src_config("Int::MAX + 1", wrap), Ok(int(i64::MIN)));
        assert_eq!(run_src_config("Int::MIN / -1", wrap), Ok(int(i64::MIN)));
        assert!(
            run_src_config("1 % 0", wrap)
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_float_arithmetic() {
        assert_eq!(run_src("1.5f + 2.25f"), Ok(float(3.75)));
        assert_eq!(run_src("10.0f / 4.0f"), Ok(float(2.5)));
        assert_eq!(run_src("7.5f % 2.0f"), Ok(float(1.5)));
        assert_eq!(run_src("5.5f mod 2.0f"), Ok(float(1.5)));
        assert_eq!(run_src("-7.5f mod 2.0f"), Ok(float(0.5)));
        assert_eq!(run_src("-0.0f"), Ok(float(0.0)));
    }

    #[test]
    fn eval_float_checks() {
        for src in ["1.0f / 0.0f", "0.0f / 0.0f", "1e308f * 10.0f"] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("non-finite"), "{src}: {err}");
        }
        let off = r#"config { float_checks = false }"#;
        assert_eq!(run_src_config("1.0f / 0.0f", off), Ok(float(f64::INFINITY)));
        match run_src_config("0.0f / 0.0f", off) {
            Ok(Val::Scalar(Scalar::Float(x))) => assert!(x.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
    }

    #[test]
    fn eval_dec_arithmetic() {
        assert_eq!(run_src("1.5 + 2.5"), Ok(dec(dec!(4.0))));
        assert_eq!(run_src("5.00 / 2.0"), Ok(dec(dec!(2.5))));
        assert_eq!(
            run_src("1.0 / 3.0"),
            Ok(dec(dec!(0.3333333333333333333333333333)))
        );
        assert_eq!(run_src("1.10 + 2.20"), Ok(dec(dec!(3.3))));
        assert_eq!(run_src("-7.0 % 3.0"), Ok(dec(dec!(-1))));
        assert_eq!(run_src("-7.0 mod 3.0"), Ok(dec(dec!(2))));
        assert_eq!(run_src("7.0 mod -3.0"), Ok(dec(dec!(1))));
        assert_eq!(run_src("-7.0 mod -3.0"), Ok(dec(dec!(2))));
    }

    #[test]
    fn eval_dec_overflow_checked() {
        for src in [
            "Dec::MAX + 1.0",
            "Dec::MAX * 10.0",
            "Dec::MIN - 1.0",
            "1.0 / 0.0",
            "1.0 % 0.0",
        ] {
            let err = run_src(src).unwrap_err();
            assert!(
                err.to_string().contains("overflow") || err.to_string().contains("zero"),
                "{src}: {err}"
            );
        }
    }

    #[test]
    fn eval_dec_saturating() {
        let sat = r#"config { dec_arithmetic = "saturating" }"#;
        assert_eq!(run_src_config("Dec::MAX + 1.0", sat), Ok(dec(Decimal::MAX)));
        assert_eq!(run_src_config("Dec::MIN - 1.0", sat), Ok(dec(Decimal::MIN)));
        assert_eq!(
            run_src_config("Dec::MAX * 10.0", sat),
            Ok(dec(Decimal::MAX))
        );
        assert!(
            run_src_config("1.0 / 0.0", sat)
                .unwrap_err()
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn eval_temporal_arithmetic() {
        let td_30d = TimeDelta::days(30);
        let plus_30d = FIXED_NOW + td_30d;
        assert_eq!(
            run_src("@2026-10-20 + #30d"),
            Ok(Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(1792454400, 0).unwrap() + td_30d
            )))
        );
        assert_eq!(
            run_src("@now + #30d"),
            Ok(Val::Scalar(Scalar::Instant(plus_30d)))
        );
        assert_eq!(
            run_src("@now - @2026-10-20"),
            Ok(Val::Scalar(Scalar::Duration(
                FIXED_NOW.signed_duration_since(DateTime::from_timestamp(1792454400, 0).unwrap())
            )))
        );
        assert_eq!(run_src("#75s / #30s"), Ok(dec(dec!(2.5))));
        assert_eq!(
            run_src("-#90s % #7s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(-6))))
        );
        assert_eq!(
            run_src("-#90s mod #7s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(1))))
        );
        assert_eq!(
            run_src("#30d * 3"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(90))))
        );
        assert_eq!(
            run_src("3 * #30d"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(90))))
        );
        assert_eq!(
            run_src("#30d * 1.5"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(45))))
        );
        assert_eq!(
            run_src("2.0f * #30s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(60))))
        );
        assert_eq!(
            run_src("#30d / 2"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(15))))
        );
        assert_eq!(
            run_src("-#30d"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::days(-30))))
        );
        assert_eq!(
            run_src("#5s * 0"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::zero())))
        );
    }

    #[test]
    fn eval_temporal_overflow() {
        let err = run_src("@262142-12-31 + #1d").unwrap_err();
        assert!(err.to_string().contains("instant overflow"), "{err}");
        let err = run_src("@262142-12-31_23:59:59.999999999 + #0.000000001s").unwrap_err();
        assert!(err.to_string().contains("instant overflow"), "{err}");
        let err = run_src("#100000000000d + #100000000000d").unwrap_err();
        assert!(err.to_string().contains("duration overflow"), "{err}");
        let err = run_src("#100000000000d * 100000000000").unwrap_err();
        assert!(err.to_string().contains("duration overflow"), "{err}");
        for src in ["#90s / #0s", "#5s % #0s", "#5s mod #0s", "#5s / 0"] {
            let err = run_src(src).unwrap_err();
            assert!(err.to_string().contains("zero"), "{src}: {err}");
        }
    }
}
