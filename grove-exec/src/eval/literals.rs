use chrono::{DateTime, NaiveTime, Utc};
use grove_query::ast::{ConstantName, Literal, TypeName};
use grove_types::Spanned;

use rust_decimal::Decimal;

use crate::value::{Scalar, Val};

pub(crate) const MIDNIGHT: NaiveTime =
    NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time");

pub(crate) fn decode_literal(lit: &Spanned<Literal>, now: DateTime<Utc>) -> Val {
    match &lit.value {
        Literal::Int(n) => Val::Scalar(Scalar::Int(*n)),
        Literal::Float(x) => Val::Scalar(Scalar::Float(*x)),
        Literal::Dec(d) => Val::Scalar(Scalar::Dec(*d)),
        Literal::String(s) => Val::Scalar(Scalar::String(s.clone())),
        Literal::Bool(b) => Val::Scalar(Scalar::Bool(*b)),
        Literal::Instant(t) => Val::Scalar(Scalar::Instant(*t)),
        Literal::Duration(d) => Val::Scalar(Scalar::Duration(*d)),
        Literal::None => Val::None,
        Literal::Now => Val::Scalar(Scalar::Instant(now)),
        Literal::Today(time) => Val::Scalar(Scalar::Instant(
            now.date_naive()
                .and_time(time.unwrap_or(MIDNIGHT))
                .and_utc(),
        )),
    }
}

pub(crate) fn eval_type_constant(ty: TypeName, name: ConstantName) -> Val {
    let scalar = match (ty, name) {
        (TypeName::Int, ConstantName::Min) => Scalar::Int(i64::MIN),
        (TypeName::Int, ConstantName::Max) => Scalar::Int(i64::MAX),
        (TypeName::Float, ConstantName::Min) => Scalar::Float(f64::MIN),
        (TypeName::Float, ConstantName::Max) => Scalar::Float(f64::MAX),
        (TypeName::Dec, ConstantName::Min) => Scalar::Dec(Decimal::MIN),
        (TypeName::Dec, ConstantName::Max) => Scalar::Dec(Decimal::MAX),
        (TypeName::String, _) => unreachable!("String has no type constants"),
    };
    Val::Scalar(scalar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::*;
    use crate::value::{Scalar, Val};
    use chrono::{DateTime, TimeDelta};
    use rust_decimal::{Decimal, dec};

    #[test]
    fn eval_literals() {
        assert_eq!(run_src("42"), Ok(int(42)));
        assert_eq!(run_src("1.5f"), Ok(float(1.5)));
        assert_eq!(run_src("30.00"), Ok(dec(dec!(30.00))));
        assert_eq!(run_src("\"hi\""), Ok(str("hi")));
        assert_eq!(run_src("true"), Ok(bool(true)));
        assert_eq!(run_src("some(1)"), Ok(Val::Some(Box::new(int(1)))),);
    }

    #[test]
    fn eval_temporal_literals() {
        assert_eq!(run_src("@now"), Ok(Val::Scalar(Scalar::Instant(FIXED_NOW))));
        let midnight = FIXED_NOW.date_naive().and_time(MIDNIGHT).and_utc();
        assert_eq!(
            run_src("@today"),
            Ok(Val::Scalar(Scalar::Instant(midnight)))
        );
        let noon = FIXED_NOW
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(
            run_src("@today_12:00"),
            Ok(Val::Scalar(Scalar::Instant(noon)))
        );
        assert_eq!(
            run_src("@2026-10-20"),
            Ok(Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(1792454400, 0).unwrap()
            )))
        );
        assert_eq!(
            run_src("#90s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(90))))
        );
    }

    #[test]
    fn eval_type_constants() {
        assert_eq!(run_src("Int::MIN"), Ok(int(i64::MIN)));
        assert_eq!(run_src("Float::MAX"), Ok(float(f64::MAX)));
        assert_eq!(run_src("Dec::MIN"), Ok(dec(Decimal::MIN)));
        assert_eq!(run_src("Dec::MAX"), Ok(dec(Decimal::MAX)));
    }
}
