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
