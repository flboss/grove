
use chrono::{DateTime, TimeDelta, Utc};
use grove_runtime::decode::{
    decode_dec, decode_duration, decode_instant, encode_dec, encode_duration, encode_instant,
};
use grove_runtime::{DecArithmetic, IntArithmetic, Runtime, RuntimeConfig};
use rust_decimal::Decimal;
use std::str::FromStr;

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn blob_param(bytes: &[u8; 13]) -> Vec<u8> {
    bytes.to_vec()
}

fn blob12(bytes: &[u8; 12]) -> Vec<u8> {
    bytes.to_vec()
}

fn checked() -> Runtime {
    Runtime::open_in_memory().unwrap()
}

fn query_dec(rt: &Runtime, sql: &str) -> Decimal {
    let bytes: Vec<u8> = rt.conn().query_row(sql, [], |row| row.get(0)).unwrap();
    decode_dec(&bytes, "c").unwrap()
}

fn query_int(rt: &Runtime, sql: &str) -> i64 {
    rt.conn().query_row(sql, [], |row| row.get(0)).unwrap()
}

fn query_text(rt: &Runtime, sql: &str) -> String {
    rt.conn().query_row(sql, [], |row| row.get(0)).unwrap()
}


#[test]
fn int_arithmetic_basic() {
    let rt = checked();
    assert_eq!(query_int(&rt, r#"SELECT "$$add"(40, 2)"#), 42);
    assert_eq!(query_int(&rt, r#"SELECT "$$sub"(40, 2)"#), 38);
    assert_eq!(query_int(&rt, r#"SELECT "$$mul"(6, 7)"#), 42);
    assert_eq!(query_int(&rt, r#"SELECT "$$div"(7, 2)"#), 3);
    assert_eq!(query_int(&rt, r#"SELECT "$$rem"(-7, 3)"#), -1);
    assert_eq!(query_int(&rt, r#"SELECT "$$mod"(-7, 3)"#), 2);
    assert_eq!(query_int(&rt, r#"SELECT "$$rem"(7, -3)"#), 1);
    assert_eq!(query_int(&rt, r#"SELECT "$$mod"(7, -3)"#), 1);
    assert_eq!(query_int(&rt, r#"SELECT "$$neg"(5)"#), -5);
}

#[test]
fn int_overflow_modes() {
    let max = i64::MAX;
    let rt = checked();
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$add"(?, 1)"#, [max], |row| row.get::<_, i64>(0))
        .unwrap_err();
    assert!(err.to_string().contains("overflow"), "unexpected: {err}");
    let rt = Runtime::open_in_memory_with_config(RuntimeConfig {
        int_arithmetic: IntArithmetic::Saturating,
        ..Default::default()
    })
    .unwrap();
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$add"(?, 1)"#, [max], |row| row.get(0))
        .unwrap();
    assert_eq!(n, max);
    let rt = Runtime::open_in_memory_with_config(RuntimeConfig {
        int_arithmetic: IntArithmetic::Wrapping,
        ..Default::default()
    })
    .unwrap();
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$add"(?, 1)"#, [max], |row| row.get(0))
        .unwrap();
    assert_eq!(n, i64::MIN);
}

#[test]
fn int_division_by_zero_aborts_in_every_mode() {
    for mode in [
        IntArithmetic::Checked,
        IntArithmetic::Saturating,
        IntArithmetic::Wrapping,
    ] {
        let rt = Runtime::open_in_memory_with_config(RuntimeConfig {
            int_arithmetic: mode,
            ..Default::default()
        })
        .unwrap();
        for op in ["$$div", "$$rem", "$$mod"] {
            let err = rt
                .conn()
                .query_row(&format!(r#"SELECT "{op}"(1, 0)"#), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap_err();
            assert!(
                err.to_string().contains("division by zero"),
                "{op}/{mode:?}: {err}"
            );
        }
    }
}

#[test]
fn int_neg_modes() {
    let rt = checked();
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$neg"(?)"#, [i64::MIN], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_err();
    assert!(err.to_string().contains("overflow"), "unexpected: {err}");
    let rt = Runtime::open_in_memory_with_config(RuntimeConfig {
        int_arithmetic: IntArithmetic::Saturating,
        ..Default::default()
    })
    .unwrap();
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$neg"(?)"#, [i64::MIN], |row| row.get(0))
        .unwrap();
    assert_eq!(n, i64::MAX);
}


#[test]
fn float_arithmetic_and_checks() {
    let rt = checked();
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$add"(1.5, 2.5)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, 4.0);
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$mul"(1e308, 10.0)"#, [], |row| {
            row.get::<_, f64>(0)
        })
        .unwrap_err();
    assert!(err.to_string().contains("non-finite"), "unexpected: {err}");
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$div"(0.0, 0.0)"#, [], |row| {
            row.get::<_, f64>(0)
        })
        .unwrap_err();
    assert!(err.to_string().contains("non-finite"), "unexpected: {err}");
    let rt = Runtime::open_in_memory_with_config(RuntimeConfig {
        float_checks: false,
        ..Default::default()
    })
    .unwrap();
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$mul"(1e308, 10.0)"#, [], |row| row.get(0))
        .unwrap();
    assert!(f.is_infinite());
    let f: Option<f64> = rt
        .conn()
        .query_row(r#"SELECT "$$div"(0.0, 0.0)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, None);
}


#[test]
fn dec_arithmetic_basic() {
    let rt = checked();
    let a = blob_param(&encode_dec(&dec("1.10")));
    let b = blob_param(&encode_dec(&dec("2.20")));
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$add"(?, ?)"#, [&a[..], &b[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap().to_string(), "3.3");
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$mul"(?, ?)"#, [&a[..], &b[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap().to_string(), "2.42");
    let c = blob_param(&encode_dec(&dec("-7.5")));
    let e = blob_param(&encode_dec(&dec("2")));
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$mod"(?, ?)"#, [&c[..], &e[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap().to_string(), "0.5");
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$rem"(?, ?)"#, [&c[..], &e[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap().to_string(), "-1.5");
}

#[test]
fn dec_overflow_modes_and_div_zero() {
    let rt = checked();
    let max = blob_param(&encode_dec(&Decimal::MAX));
    let one = blob_param(&encode_dec(&Decimal::ONE));
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$add"(?, ?)"#, [&max[..], &one[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap_err();
    assert!(err.to_string().contains("overflow"), "unexpected: {err}");
    let rt = Runtime::open_in_memory_with_config(RuntimeConfig {
        dec_arithmetic: DecArithmetic::Saturating,
        ..Default::default()
    })
    .unwrap();
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$add"(?, ?)"#, [&max[..], &one[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap(), Decimal::MAX);
    let zero = blob_param(&encode_dec(&Decimal::ZERO));
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$div"(?, ?)"#, [&one[..], &zero[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("division by zero"),
        "unexpected: {err}"
    );
}


#[test]
fn cast_kinds() {
    let rt = checked();
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(?, ?)"#, (i64::MAX, "float"), |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(f, 9.223372036854776e18);
    assert_eq!(
        query_dec(&rt, r#"SELECT "$$cast"(42, 'dec')"#).to_string(),
        "42"
    );
    assert_eq!(query_int(&rt, r#"SELECT "$$cast"(3.99, 'int')"#), 3);
    assert_eq!(query_int(&rt, r#"SELECT "$$cast"(-3.99, 'int')"#), -3);
    assert_eq!(query_int(&rt, r#"SELECT "$$cast"(1e30, 'int')"#), i64::MAX);
    assert_eq!(query_int(&rt, r#"SELECT "$$cast"(-1e30, 'int')"#), i64::MIN);
    let nan: f64 = f64::NAN;
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(?, 'int')"#, [nan], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(NULL, 'int')"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(NULL, 'dec')"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap(), Decimal::ZERO);
    let f: Option<f64> = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(NULL, 'float')"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, None);
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(0.1, 'dec')"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap().to_string(), "0.1");
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(?, 'dec')"#, [f64::INFINITY], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap(), Decimal::MAX);
    let big = blob_param(&encode_dec(&dec("3.99")));
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(?, 'int')"#, [&big[..]], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 3);
    let huge = blob_param(&encode_dec(&Decimal::MAX));
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(?, 'int')"#, [&huge[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(n, i64::MAX);
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$cast"(?, 'float')"#, [&big[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(f, 3.99);
}


#[test]
fn string_case_is_unicode_aware() {
    let rt = checked();
    assert_eq!(query_text(&rt, r#"SELECT "$$upper"('straße')"#), "STRASSE");
    assert_eq!(query_text(&rt, r#"SELECT "$$lower"('STRASSE')"#), "strasse");
}

#[test]
fn float_rounding() {
    let rt = checked();
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$float_round"(2.5)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, 3.0);
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$float_round"(-2.5)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, -3.0);
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$float_floor"(2.9)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, 2.0);
    let f: f64 = rt
        .conn()
        .query_row(r#"SELECT "$$float_ceil"(2.1)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(f, 3.0);
}

#[test]
fn dec_round_dp_is_bankers() {
    let rt = checked();
    let x = blob_param(&encode_dec(&dec("3.14159")));
    let d: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$dec_round_dp"(?, 2)"#, [&x[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_dec(&d, "c").unwrap().to_string(), "3.14");
    for (input, expected) in [("2.5", "2"), ("3.5", "4")] {
        let x = blob_param(&encode_dec(&dec(input)));
        let d: Vec<u8> = rt
            .conn()
            .query_row(r#"SELECT "$$dec_round_dp"(?, 0)"#, [&x[..]], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(decode_dec(&d, "c").unwrap().to_string(), expected);
    }
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$dec_round_dp"(?, -1)"#, [&x[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("non-negative"),
        "unexpected: {err}"
    );
}

#[test]
fn dec_to_string_renders() {
    let rt = checked();
    let x = blob_param(&encode_dec(&dec("2.50")));
    let s: String = rt
        .conn()
        .query_row(r#"SELECT "$$dec_to_string"(?)"#, [&x[..]], |row| row.get(0))
        .unwrap();
    assert_eq!(s, "2.5");
}

fn leap_instant() -> DateTime<Utc> {
    chrono::NaiveDate::from_ymd_opt(2024, 2, 29)
        .unwrap()
        .and_hms_opt(15, 4, 5)
        .unwrap()
        .and_utc()
}

#[test]
fn instant_extractors() {
    let rt = checked();
    let t = blob12(&encode_instant(&leap_instant()));
    for (name, expected) in [
        ("$$instant_year", 2024),
        ("$$instant_month", 2),
        ("$$instant_day", 29),
        ("$$instant_hour", 15),
        ("$$instant_minute", 4),
        ("$$instant_second", 5),
        ("$$instant_weekday", 4),
    ] {
        let n: i64 = rt
            .conn()
            .query_row(&format!(r#"SELECT "{name}"(?)"#), [&t[..]], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(n, expected, "{name}");
    }
    let s: String = rt
        .conn()
        .query_row(r#"SELECT "$$instant_to_rfc3339"(?)"#, [&t[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(s, "2024-02-29T15:04:05Z");
    let epoch: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$instant_epoch"(?)"#, [&t[..]], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_dec(&epoch, "c").unwrap().to_string(), "1709219045");
}

#[test]
fn duration_extractors() {
    let rt = checked();
    let d = blob12(&encode_duration(&TimeDelta::seconds(5400)));
    for (name, expected) in [
        ("$$duration_as_seconds", "5400"),
        ("$$duration_as_minutes", "90"),
        ("$$duration_as_hours", "1.5"),
        ("$$duration_as_days", "0.0625"),
    ] {
        let raw: Vec<u8> = rt
            .conn()
            .query_row(&format!(r#"SELECT "{name}"(?)"#), [&d[..]], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            decode_dec(&raw, "c").unwrap().to_string(),
            expected,
            "{name}"
        );
    }
}


fn instant_blob(secs: i64, nanos: u32) -> Vec<u8> {
    blob12(&encode_instant(
        &DateTime::from_timestamp(secs, nanos).unwrap(),
    ))
}

fn duration_blob(total_secs: i64) -> Vec<u8> {
    blob12(&encode_duration(&TimeDelta::seconds(total_secs)))
}

#[test]
fn temporal_add_sub() {
    let rt = checked();
    let t = instant_blob(1_704_067_200, 0);
    let d = duration_blob(86_400);
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$add"(?, ?, 'instant', 'duration')"#,
            [&t[..], &d[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_instant(&out, "c").unwrap(),
        DateTime::from_timestamp(1_704_153_600, 0).unwrap()
    );
    let u = instant_blob(1_704_153_600, 0);
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$sub"(?, ?, 'instant', 'instant')"#,
            [&u[..], &t[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_duration(&out, "c").unwrap(),
        TimeDelta::seconds(86_400)
    );
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$add"(?, ?, 'duration', 'duration')"#,
            [&d[..], &d[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_duration(&out, "c").unwrap(),
        TimeDelta::seconds(172_800)
    );
    let err = rt
        .conn()
        .query_row(
            r#"SELECT "$$add"(?, ?, 'instant', 'instant')"#,
            [&t[..], &t[..]],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap_err();
    assert!(err.to_string().contains("unsupported"), "unexpected: {err}");
}

#[test]
fn duration_scaling() {
    let rt = checked();
    let d = duration_blob(90);
    for sql in [
        r#"SELECT "$$mul"(?, 2, 'duration', NULL)"#,
        r#"SELECT "$$mul"(2, ?, NULL, 'duration')"#,
    ] {
        let out: Vec<u8> = rt
            .conn()
            .query_row(sql, [&d[..]], |row| row.get(0))
            .unwrap();
        assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(180));
    }
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$div"(?, 2, 'duration', NULL)"#,
            [&d[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(45));
    let f = blob_param(&encode_dec(&dec("1.5")));
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$mul"(?, ?, 'duration', NULL)"#,
            [&d[..], &f[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(135));
    for op in ["$$rem", "$$mod"] {
        let err = rt
            .conn()
            .query_row(
                &format!(r#"SELECT "{op}"(?, 7, 'duration', NULL)"#),
                [&d[..]],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap_err();
        assert!(err.to_string().contains("unsupported"), "{op}: {err}");
    }
    let err = rt
        .conn()
        .query_row(
            r#"SELECT "$$mul"(?, ?, 'duration', 'duration')"#,
            [&d[..], &d[..]],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap_err();
    assert!(err.to_string().contains("unsupported"), "unexpected: {err}");
}

#[test]
fn duration_div_rem_mod_duration() {
    let rt = checked();
    let a = duration_blob(90);
    let b = duration_blob(7);
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$div"(?, ?, 'duration', 'duration')"#,
            [&a[..], &b[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        decode_dec(&out, "c").unwrap().to_string(),
        "12.857142857142857142857142857"
    );
    let c = duration_blob(30);
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$div"(?, ?, 'duration', 'duration')"#,
            [&a[..], &c[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode_dec(&out, "c").unwrap().to_string(), "3");
    let z = duration_blob(0);
    let err = rt
        .conn()
        .query_row(
            r#"SELECT "$$div"(?, ?, 'duration', 'duration')"#,
            [&a[..], &z[..]],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("division by zero"),
        "unexpected: {err}"
    );
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$rem"(?, ?, 'duration', 'duration')"#,
            [&a[..], &b[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(6));
    let neg = duration_blob(-90);
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$rem"(?, ?, 'duration', 'duration')"#,
            [&neg[..], &b[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(-6));
    let out: Vec<u8> = rt
        .conn()
        .query_row(
            r#"SELECT "$$mod"(?, ?, 'duration', 'duration')"#,
            [&neg[..], &b[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(1));
}

#[test]
fn neg_duration() {
    let rt = checked();
    let d = duration_blob(90);
    let out: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$neg"(?, 'duration')"#, [&d[..]], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decode_duration(&out, "c").unwrap(), TimeDelta::seconds(-90));
}


#[test]
fn panic_aborts_with_marker() {
    let rt = checked();
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$panic"('boom')"#, [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("grove-abort: boom"),
        "unexpected: {err}"
    );
    let n: i64 = rt
        .conn()
        .query_row(r#"SELECT COALESCE(1, "$$panic"('boom'))"#, [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn now_and_today() {
    let rt = checked();
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$now"()"#, [], |row| row.get(0))
        .unwrap();
    let t = decode_instant(&raw, "c").unwrap();
    assert!((t - Utc::now()).num_seconds().abs() < 60);
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$today"(NULL)"#, [], |row| row.get(0))
        .unwrap();
    let midnight = decode_instant(&raw, "c").unwrap();
    assert_eq!(
        midnight.time(),
        chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
    );
    assert_eq!(midnight.date_naive(), Utc::now().date_naive());
    let off = blob12(&encode_duration(&TimeDelta::seconds(3600)));
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$today"(?)"#, [&off[..]], |row| row.get(0))
        .unwrap();
    assert_eq!(
        decode_instant(&raw, "c").unwrap(),
        midnight + TimeDelta::seconds(3600)
    );
    let bad = blob12(&encode_duration(&TimeDelta::seconds(25 * 3600)));
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$today"(?)"#, [&bad[..]], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap_err();
    assert!(err.to_string().contains("[0, 24h)"), "unexpected: {err}");
}

#[test]
fn nulls_propagate_and_mismatches_fail() {
    let rt = checked();
    let n: Option<i64> = rt
        .conn()
        .query_row(r#"SELECT "$$add"(NULL, 1)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(n, None);
    let s: Option<String> = rt
        .conn()
        .query_row(r#"SELECT "$$upper"(NULL)"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(s, None);
    let err = rt
        .conn()
        .query_row(r#"SELECT "$$add"('a', 'b')"#, [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_err();
    assert!(err.to_string().contains("unsupported"), "unexpected: {err}");
}


fn agg_table(rt: &Runtime, values: &[Vec<u8>]) {
    rt.conn()
        .execute("CREATE TABLE IF NOT EXISTS m(v BLOB)", [])
        .unwrap();
    rt.conn().execute("DELETE FROM m", []).unwrap();
    for v in values {
        rt.conn()
            .execute("INSERT INTO m(v) VALUES (?)", [&v[..]])
            .unwrap();
    }
}

fn decs(strs: &[&str]) -> Vec<Vec<u8>> {
    strs.iter()
        .map(|s| blob_param(&encode_dec(&dec(s))))
        .collect()
}

#[test]
fn custom_aggregates() {
    let rt = checked();
    agg_table(&rt, &decs(&["1.10", "2.20", "3.70"]));
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$sum_dec"(v) FROM m"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_dec(&raw, "c").unwrap().to_string(), "7");
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$avg_dec"(v) FROM m"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        decode_dec(&raw, "c").unwrap().to_string(),
        "2.3333333333333333333333333333"
    );
}

#[test]
fn aggregate_empty_sets_yield_null() {
    let rt = checked();
    rt.conn().execute("CREATE TABLE e(v BLOB)", []).unwrap();
    for name in [
        "$$sum_dec",
        "$$avg_dec",
        "$$sum_dur",
        "$$avg_dur",
        "$$avg_instant",
    ] {
        let v: Option<Vec<u8>> = rt
            .conn()
            .query_row(&format!(r#"SELECT "{name}"(v) FROM e"#), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(v, None, "{name} over empty set");
    }
}

#[test]
fn temporal_aggregates() {
    let rt = checked();
    let durs: Vec<Vec<u8>> = [60, 120, 180]
        .iter()
        .map(|s| blob12(&encode_duration(&TimeDelta::seconds(*s))))
        .collect();
    agg_table(&rt, &durs);
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$sum_dur"(v) FROM m"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_duration(&raw, "c").unwrap(), TimeDelta::seconds(360));
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$avg_dur"(v) FROM m"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode_duration(&raw, "c").unwrap(), TimeDelta::seconds(120));
    let instants: Vec<Vec<u8>> = [0, 10]
        .iter()
        .map(|s| blob12(&encode_instant(&DateTime::from_timestamp(*s, 0).unwrap())))
        .collect();
    agg_table(&rt, &instants);
    let raw: Vec<u8> = rt
        .conn()
        .query_row(r#"SELECT "$$avg_instant"(v) FROM m"#, [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        decode_instant(&raw, "c").unwrap(),
        DateTime::from_timestamp(5, 0).unwrap()
    );
}
