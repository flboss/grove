use std::fmt::Write;

use chrono::TimeDelta;

pub(crate) fn total_nanos(td: &TimeDelta) -> i128 {
    td.num_seconds() as i128 * NANOS_PER_SEC as i128 + td.subsec_nanos() as i128
}

pub(crate) const NANOS_PER_SEC: u128 = 1_000_000_000;
pub(crate) const NANOS_PER_MIN: u128 = 60 * NANOS_PER_SEC;
pub(crate) const NANOS_PER_HOUR: u128 = 60 * NANOS_PER_MIN;
pub(crate) const NANOS_PER_DAY: u128 = 24 * NANOS_PER_HOUR;

pub(super) fn fmt_duration(td: &TimeDelta) -> String {
    let total_nanos = total_nanos(td);
    if total_nanos == 0 {
        return "0s".to_string();
    }
    let mut nanos = total_nanos.unsigned_abs();

    let days = nanos / NANOS_PER_DAY;
    nanos %= NANOS_PER_DAY;
    let hours = nanos / NANOS_PER_HOUR;
    nanos %= NANOS_PER_HOUR;
    let mins = nanos / NANOS_PER_MIN;
    nanos %= NANOS_PER_MIN;
    let secs = nanos / NANOS_PER_SEC;
    nanos %= NANOS_PER_SEC;

    let mut out = String::new();
    if total_nanos < 0 {
        out.push('-');
    }
    if days != 0 {
        write!(out, "{days}d").unwrap();
    }
    if hours != 0 {
        write!(out, "{hours}h").unwrap();
    }
    if mins != 0 {
        write!(out, "{mins}m").unwrap();
    }
    if secs != 0 || nanos != 0 {
        if nanos == 0 {
            write!(out, "{secs}s").unwrap();
        } else {
            let frac = format!("{nanos:09}");
            write!(out, "{secs}.{}s", frac.trim_end_matches('0')).unwrap();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use crate::value::Scalar;

    use super::*;

    #[test]
    fn duration_display() {
        let s = |secs: i64| Scalar::Duration(TimeDelta::seconds(secs)).to_string();
        let ms = |millis: i64| Scalar::Duration(TimeDelta::milliseconds(millis)).to_string();
        let ns = |nanos: i64| Scalar::Duration(TimeDelta::nanoseconds(nanos)).to_string();
        assert_eq!(s(0), "0s");
        assert_eq!(s(90), "1m30s");
        assert_eq!(s(60 * 60), "1h");
        assert_eq!(s(60 * 61 + 1), "1h1m1s");
        assert_eq!(s(24 * 60 * 60), "1d");
        assert_eq!(s(25 * 60 * 60), "1d1h");
        assert_eq!(s(-365 * 24 * 60 * 60 - 60 * 60 - 2), "-365d1h2s");
        assert_eq!(s(-90), "-1m30s");
        assert_eq!(ms(500), "0.5s");
        assert_eq!(ms(1500), "1.5s");
        assert_eq!(ms(61_500), "1m1.5s");
        assert_eq!(ms(-1500), "-1.5s");
        assert_eq!(ns(-1), "-0.000000001s");
        assert_eq!(ns(1_234_567_890), "1.23456789s");
    }

    #[test]
    fn instant_display() {
        let at = |nanos: u32| {
            Scalar::Instant(DateTime::from_timestamp(1_700_000_000, nanos).unwrap()).to_string()
        };
        assert_eq!(at(0), "2023-11-14T22:13:20Z");
        assert_eq!(at(123_000_000), "2023-11-14T22:13:20.123Z");
        assert_eq!(at(123_456_000), "2023-11-14T22:13:20.123456Z");
        assert_eq!(at(123_456_789), "2023-11-14T22:13:20.123456789Z");
        assert_eq!(at(1), "2023-11-14T22:13:20.000000001Z");
    }
}
