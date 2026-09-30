
use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use num_traits::{FromPrimitive, ToPrimitive};
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Error};
use rust_decimal::Decimal;

use super::decode::{
    decode_dec, decode_duration, decode_instant, encode_dec, encode_duration, encode_instant,
};
use super::{DecArithmetic, IntArithmetic, RuntimeConfig};

enum Arg {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn arg(ctx: &Context<'_>, i: usize) -> Result<Arg, Error> {
    match ctx.get::<Option<i64>>(i) {
        Ok(None) => return Ok(Arg::Null),
        Ok(Some(n)) => return Ok(Arg::Int(n)),
        Err(_) => {}
    }
    if let Ok(f) = ctx.get::<f64>(i) {
        return Ok(Arg::Float(f));
    }
    if let Ok(s) = ctx.get::<String>(i) {
        return Ok(Arg::Text(s));
    }
    if let Ok(b) = ctx.get::<Vec<u8>>(i) {
        return Ok(Arg::Blob(b));
    }
    Err(Error::InvalidFunctionParameterType(
        i,
        rusqlite::types::Type::Null,
    ))
}

fn fail(op: &str, msg: impl Into<String>) -> Error {
    Error::UserFunctionError(format!("{}: {}", op, msg.into()).into())
}

fn overflow(op: &str) -> Error {
    fail(op, "integer overflow")
}

fn div_by_zero(op: &str) -> Error {
    fail(op, "division by zero")
}

fn descriptor_is(arg: &Arg, name: &str) -> bool {
    matches!(arg, Arg::Text(s) if s == name)
}

fn nanos_to_dec(total_nanos: i128) -> Decimal {
    let secs = total_nanos.div_euclid(1_000_000_000);
    let nanos = total_nanos.rem_euclid(1_000_000_000);
    debug_assert!(secs >= i64::MIN as i128 && secs <= i64::MAX as i128);
    Decimal::new(secs as i64, 0) + Decimal::new(nanos as i64, 9)
}


fn int_op(op: &str, a: i64, b: i64, mode: IntArithmetic) -> Result<SqlValue, Error> {
    let checked = match op {
        "$$add" => a.checked_add(b),
        "$$sub" => a.checked_sub(b),
        "$$mul" => a.checked_mul(b),
        "$$div" => {
            if b == 0 {
                return Err(div_by_zero(op));
            }
            a.checked_div(b)
        }
        "$$rem" | "$$mod" => {
            if b == 0 {
                return Err(div_by_zero(op));
            }
            Some(if op == "$$mod" {
                a.rem_euclid(b)
            } else {
                a % b
            })
        }
        _ => return Err(fail(op, "unknown integer operator")),
    };
    match (checked, mode) {
        (Some(v), _) => Ok(SqlValue::Integer(v)),
        (None, IntArithmetic::Checked) => Err(overflow(op)),
        (None, IntArithmetic::Saturating) => Ok(SqlValue::Integer(match op {
            "$$add" => a.saturating_add(b),
            "$$sub" => a.saturating_sub(b),
            "$$mul" => a.saturating_mul(b),
            _ => a.saturating_div(b),
        })),
        (None, IntArithmetic::Wrapping) => Ok(SqlValue::Integer(match op {
            "$$add" => a.wrapping_add(b),
            "$$sub" => a.wrapping_sub(b),
            "$$mul" => a.wrapping_mul(b),
            _ => a.wrapping_div(b),
        })),
    }
}

fn float_op(op: &str, a: f64, b: f64, checks: bool) -> Result<SqlValue, Error> {
    let result = match op {
        "$$add" => a + b,
        "$$sub" => a - b,
        "$$mul" => a * b,
        "$$div" => a / b,
        "$$rem" => a % b,
        "$$mod" => a.rem_euclid(b),
        _ => return Err(fail(op, "unknown float operator")),
    };
    if checks && !result.is_finite() {
        return Err(fail(
            op,
            format!("non-finite float result from {a} and {b}"),
        ));
    }
    Ok(SqlValue::Real(result))
}

fn dec_saturate(a: &Decimal, b: &Decimal) -> Decimal {
    if a.is_sign_negative() != b.is_sign_negative() {
        return if a.is_sign_negative() {
            Decimal::MIN
        } else {
            Decimal::MAX
        };
    }
    if a.is_sign_negative() {
        Decimal::MIN
    } else {
        Decimal::MAX
    }
}

pub(crate) fn add_dec(
    op: &str,
    a: Decimal,
    b: Decimal,
    mode: DecArithmetic,
) -> Result<Decimal, Error> {
    match (a.checked_add(b), mode) {
        (Some(v), _) => Ok(v),
        (None, DecArithmetic::Checked) => Err(fail(op, "decimal overflow")),
        (None, DecArithmetic::Saturating) => Ok(dec_saturate(&a, &b)),
    }
}

pub(crate) fn div_dec(
    op: &str,
    a: Decimal,
    b: Decimal,
    mode: DecArithmetic,
) -> Result<Decimal, Error> {
    if b.is_zero() {
        return Err(div_by_zero(op));
    }
    match (a.checked_div(b), mode) {
        (Some(v), _) => Ok(v),
        (None, DecArithmetic::Checked) => Err(fail(op, "decimal overflow")),
        (None, DecArithmetic::Saturating) => Ok(dec_saturate(&a, &b)),
    }
}

fn dec_op(op: &str, a: Decimal, b: Decimal, mode: DecArithmetic) -> Result<SqlValue, Error> {
    if (op == "$$div" || op == "$$rem" || op == "$$mod") && b.is_zero() {
        return Err(div_by_zero(op));
    }
    if op == "$$add" {
        return add_dec(op, a, b, mode).map(|v| SqlValue::Blob(encode_dec(&v).to_vec()));
    }
    if op == "$$div" {
        return div_dec(op, a, b, mode).map(|v| SqlValue::Blob(encode_dec(&v).to_vec()));
    }
    let checked = match op {
        "$$sub" => a.checked_sub(b),
        "$$mul" => a.checked_mul(b),
        "$$rem" => a.checked_rem(b),
        "$$mod" => {
            a.checked_rem(b).map(|r| {
                if r.is_sign_negative() {
                    r.checked_add(b.abs()).unwrap_or(b.abs())
                } else {
                    r
                }
            })
        }
        _ => return Err(fail(op, "unknown decimal operator")),
    };
    match (checked, mode) {
        (Some(v), _) => Ok(SqlValue::Blob(encode_dec(&v).to_vec())),
        (None, DecArithmetic::Checked) => Err(fail(op, "decimal overflow")),
        (None, DecArithmetic::Saturating) => {
            Ok(SqlValue::Blob(encode_dec(&dec_saturate(&a, &b)).to_vec()))
        }
    }
}


pub(crate) fn duration_nanos(d: &TimeDelta) -> i128 {
    d.num_seconds() as i128 * 1_000_000_000 + d.subsec_nanos() as i128
}

pub(crate) fn duration_from_nanos(op: &str, total: i128) -> Result<TimeDelta, Error> {
    let secs = total.div_euclid(1_000_000_000);
    let nanos = total.rem_euclid(1_000_000_000);
    let secs = i64::try_from(secs).map_err(|_| fail(op, "duration overflow"))?;
    TimeDelta::new(secs, nanos as u32).ok_or_else(|| fail(op, "duration overflow"))
}

fn factor_to_decimal(op: &str, arg: &Arg) -> Result<Decimal, Error> {
    match arg {
        Arg::Int(n) => Ok(Decimal::from(*n)),
        Arg::Float(f) => {
            Decimal::from_f64(*f).ok_or_else(|| fail(op, format!("non-finite scalar factor {f}")))
        }
        Arg::Blob(b) => decode_dec(b, op).map_err(|e| fail(op, e.to_string())),
        _ => Err(fail(op, "duration scaling needs a numeric scalar")),
    }
}

fn scale_duration(
    op: &str,
    span: TimeDelta,
    factor: Decimal,
    divide: bool,
) -> Result<SqlValue, Error> {
    let total = Decimal::from(span.num_seconds()) * Decimal::new(1_000_000_000, 0)
        + Decimal::from(span.subsec_nanos() as i64);
    let scaled = if divide {
        if factor.is_zero() {
            return Err(div_by_zero(op));
        }
        total.checked_div(factor)
    } else {
        total.checked_mul(factor)
    };
    let scaled = scaled.ok_or_else(|| fail(op, "duration overflow"))?;
    let whole = scaled.trunc();
    let frac = (scaled - whole).abs();
    let whole = if frac >= Decimal::new(5, 1) {
        whole + Decimal::from(if scaled.is_sign_negative() { -1 } else { 1 })
    } else {
        whole
    };
    let nanos = whole
        .to_i128()
        .ok_or_else(|| fail(op, "duration overflow"))?;
    Ok(SqlValue::Blob(
        encode_duration(&duration_from_nanos(op, nanos)?).to_vec(),
    ))
}

fn temporal_op(
    op: &str,
    a: &Arg,
    b: &Arg,
    ta: &Arg,
    tb: &Arg,
    cfg: RuntimeConfig,
) -> Result<SqlValue, Error> {
    if matches!(a, Arg::Null) || matches!(b, Arg::Null) {
        return Ok(SqlValue::Null);
    }
    match op {
        "$$add" | "$$sub" => {
            let dir: i8 = if op == "$$add" { 1 } else { -1 };
            temporal_add_sub(op, dir, a, b, ta, tb)
        }
        "$$mul" | "$$div" | "$$rem" | "$$mod" => temporal_scale(op, a, b, ta, tb, cfg),
        _ => Err(fail(op, "unknown temporal operator")),
    }
}

fn temporal_add_sub(
    op: &str,
    dir: i8,
    a: &Arg,
    b: &Arg,
    ta: &Arg,
    tb: &Arg,
) -> Result<SqlValue, Error> {
    let instant = |arg: &Arg, desc: &Arg| -> Result<Option<DateTime<Utc>>, Error> {
        match (arg, descriptor_is(desc, "instant")) {
            (Arg::Blob(blob), true) => decode_instant(blob, op)
                .map(Some)
                .map_err(|e| fail(op, e.to_string())),
            _ => Ok(None),
        }
    };
    let span = |arg: &Arg, desc: &Arg| -> Result<Option<TimeDelta>, Error> {
        match (arg, descriptor_is(desc, "duration")) {
            (Arg::Blob(blob), true) => decode_duration(blob, op)
                .map(Some)
                .map_err(|e| fail(op, e.to_string())),
            _ => Ok(None),
        }
    };
    let (ai, bi) = (instant(a, ta)?, instant(b, tb)?);
    let (ad, bd) = (span(a, ta)?, span(b, tb)?);
    match (ai, bi, ad, bd) {
        (Some(t), None, None, Some(d)) => {
            let out = if dir > 0 {
                t.checked_add_signed(d)
            } else {
                t.checked_sub_signed(d)
            };
            out.map(|t| SqlValue::Blob(encode_instant(&t).to_vec()))
                .ok_or_else(|| fail(op, "instant overflow"))
        }
        (Some(t), Some(u), None, None) if dir < 0 => Ok(SqlValue::Blob(
            encode_duration(&t.signed_duration_since(u)).to_vec(),
        )),
        (None, None, Some(x), Some(y)) => {
            let out = if dir > 0 {
                x.checked_add(&y)
            } else {
                x.checked_sub(&y)
            };
            out.map(|d| SqlValue::Blob(encode_duration(&d).to_vec()))
                .ok_or_else(|| fail(op, "duration overflow"))
        }
        _ => Err(fail(op, "unsupported instant/duration combination")),
    }
}

fn temporal_scale(
    op: &str,
    a: &Arg,
    b: &Arg,
    ta: &Arg,
    tb: &Arg,
    cfg: RuntimeConfig,
) -> Result<SqlValue, Error> {
    let span = |arg: &Arg, desc: &Arg| -> Result<Option<TimeDelta>, Error> {
        match (arg, descriptor_is(desc, "duration")) {
            (Arg::Blob(blob), true) => decode_duration(blob, op)
                .map(Some)
                .map_err(|e| fail(op, e.to_string())),
            _ => Ok(None),
        }
    };
    match (span(a, ta)?, span(b, tb)?) {
        (Some(d), None) => {
            let factor = factor_to_decimal(op, b)?;
            match op {
                "$$mul" => scale_duration(op, d, factor, false),
                "$$div" => scale_duration(op, d, factor, true),
                _ => Err(fail(op, "unsupported duration/scalar combination")),
            }
        }
        (None, Some(d)) if op == "$$mul" => {
            let factor = factor_to_decimal(op, a)?;
            scale_duration(op, d, factor, false)
        }
        (Some(x), Some(y)) => {
            let a_total = duration_nanos(&x);
            let b_total = duration_nanos(&y);
            if b_total == 0 {
                return Err(div_by_zero(op));
            }
            match op {
                "$$div" => {
                    let ratio = div_dec(
                        op,
                        nanos_to_dec(a_total),
                        nanos_to_dec(b_total),
                        cfg.dec_arithmetic,
                    )?;
                    Ok(SqlValue::Blob(encode_dec(&ratio).to_vec()))
                }
                "$$rem" => duration_from_nanos(op, a_total % b_total)
                    .map(|d| SqlValue::Blob(encode_duration(&d).to_vec())),
                "$$mod" => {
                    let mut r = a_total % b_total;
                    if r < 0 {
                        r += b_total.abs();
                    }
                    duration_from_nanos(op, r).map(|d| SqlValue::Blob(encode_duration(&d).to_vec()))
                }
                _ => Err(fail(op, "unsupported duration/duration combination")),
            }
        }
        _ => Err(fail(op, "unsupported duration/scalar combination")),
    }
}


fn apply_arith2(op: &str, ctx: &Context<'_>, cfg: RuntimeConfig) -> Result<SqlValue, Error> {
    let (a, b) = (arg(ctx, 0)?, arg(ctx, 1)?);
    if matches!(a, Arg::Null) || matches!(b, Arg::Null) {
        return Ok(SqlValue::Null);
    }
    match (&a, &b) {
        (Arg::Int(x), Arg::Int(y)) => int_op(op, *x, *y, cfg.int_arithmetic),
        (Arg::Float(x), Arg::Float(y)) => float_op(op, *x, *y, cfg.float_checks),
        (Arg::Blob(x), Arg::Blob(y)) => {
            let (dx, dy) = (
                decode_dec(x, op).map_err(|e| fail(op, e.to_string()))?,
                decode_dec(y, op).map_err(|e| fail(op, e.to_string()))?,
            );
            dec_op(op, dx, dy, cfg.dec_arithmetic)
        }
        _ => Err(fail(op, "unsupported operand combination")),
    }
}

fn apply_arith4(op: &str, ctx: &Context<'_>, cfg: RuntimeConfig) -> Result<SqlValue, Error> {
    let (a, b, ta, tb) = (arg(ctx, 0)?, arg(ctx, 1)?, arg(ctx, 2)?, arg(ctx, 3)?);
    if matches!(a, Arg::Null) || matches!(b, Arg::Null) {
        return Ok(SqlValue::Null);
    }
    let temporal_side = |arg: &Arg, desc: &Arg| {
        matches!(arg, Arg::Blob(_))
            && (descriptor_is(desc, "instant") || descriptor_is(desc, "duration"))
    };
    if temporal_side(&a, &ta) || temporal_side(&b, &tb) {
        return temporal_op(op, &a, &b, &ta, &tb, cfg);
    }
    match (&a, &b) {
        (Arg::Int(x), Arg::Int(y)) => int_op(op, *x, *y, cfg.int_arithmetic),
        (Arg::Float(x), Arg::Float(y)) => float_op(op, *x, *y, cfg.float_checks),
        (Arg::Blob(x), Arg::Blob(y)) => {
            let (dx, dy) = (
                decode_dec(x, op).map_err(|e| fail(op, e.to_string()))?,
                decode_dec(y, op).map_err(|e| fail(op, e.to_string()))?,
            );
            dec_op(op, dx, dy, cfg.dec_arithmetic)
        }
        _ => Err(fail(op, "unsupported operand combination")),
    }
}

fn apply_neg(ctx: &Context<'_>, cfg: RuntimeConfig) -> Result<SqlValue, Error> {
    let op = "$$neg";
    let value = arg(ctx, 0)?;
    if matches!(value, Arg::Null) {
        return Ok(SqlValue::Null);
    }
    if ctx.len() > 1 {
        if let Arg::Blob(blob) = &value {
            let d = decode_duration(blob, op).map_err(|e| fail(op, e.to_string()))?;
            let out = TimeDelta::default()
                .checked_sub(&d)
                .ok_or_else(|| fail(op, "duration overflow"))?;
            return Ok(SqlValue::Blob(encode_duration(&out).to_vec()));
        }
        return Err(fail(op, "descriptor form needs a duration blob"));
    }
    match &value {
        Arg::Int(n) => match cfg.int_arithmetic {
            IntArithmetic::Checked => n
                .checked_neg()
                .map(SqlValue::Integer)
                .ok_or_else(|| overflow(op)),
            IntArithmetic::Saturating => Ok(SqlValue::Integer(n.saturating_neg())),
            IntArithmetic::Wrapping => Ok(SqlValue::Integer(n.wrapping_neg())),
        },
        Arg::Float(f) => Ok(SqlValue::Real(-f)),
        Arg::Blob(blob) => {
            let d = decode_dec(blob, op).map_err(|e| fail(op, e.to_string()))?;
            Ok(SqlValue::Blob(encode_dec(&(-d)).to_vec()))
        }
        _ => Err(fail(op, "unsupported operand")),
    }
}


fn apply_cast(ctx: &Context<'_>) -> Result<SqlValue, Error> {
    let op = "$$cast";
    let value = arg(ctx, 0)?;
    let target = match arg(ctx, 1)? {
        Arg::Text(t) => t,
        _ => return Err(fail(op, "target must be 'int', 'float' or 'dec'")),
    };
    if matches!(value, Arg::Null) {
        return match target.as_str() {
            "int" => Ok(SqlValue::Integer(0)),
            "dec" => Ok(SqlValue::Blob(encode_dec(&Decimal::ZERO).to_vec())),
            "float" => Ok(SqlValue::Null),
            _ => Err(fail(op, "target must be 'int', 'float' or 'dec'")),
        };
    }
    match target.as_str() {
        "int" => match value {
            Arg::Int(n) => Ok(SqlValue::Integer(n)),
            Arg::Float(f) => Ok(SqlValue::Integer(f as i64)),
            Arg::Blob(blob) => {
                let d = decode_dec(&blob, op).map_err(|e| fail(op, e.to_string()))?;
                let t = d.trunc();
                if t > Decimal::from(i64::MAX) {
                    Ok(SqlValue::Integer(i64::MAX))
                } else if t < Decimal::from(i64::MIN) {
                    Ok(SqlValue::Integer(i64::MIN))
                } else {
                    Ok(SqlValue::Integer(t.mantissa() as i64))
                }
            }
            _ => Err(fail(op, "cannot cast to int")),
        },
        "float" => match value {
            Arg::Int(n) => Ok(SqlValue::Real(n as f64)),
            Arg::Float(f) => Ok(SqlValue::Real(f)),
            Arg::Blob(blob) => {
                let d = decode_dec(&blob, op).map_err(|e| fail(op, e.to_string()))?;
                d.to_f64()
                    .map(SqlValue::Real)
                    .ok_or_else(|| fail(op, "decimal out of float range"))
            }
            _ => Err(fail(op, "cannot cast to float")),
        },
        "dec" => match value {
            Arg::Int(n) => Ok(SqlValue::Blob(encode_dec(&Decimal::from(n)).to_vec())),
            Arg::Float(f) => {
                if f.is_nan() {
                    Ok(SqlValue::Blob(encode_dec(&Decimal::ZERO).to_vec()))
                } else {
                    match Decimal::from_f64(f) {
                        Some(d) => Ok(SqlValue::Blob(encode_dec(&d).to_vec())),
                        None => {
                            let bound = if f.is_sign_negative() {
                                Decimal::MIN
                            } else {
                                Decimal::MAX
                            };
                            Ok(SqlValue::Blob(encode_dec(&bound).to_vec()))
                        }
                    }
                }
            }
            Arg::Blob(blob) => {
                decode_dec(&blob, op).map_err(|e| fail(op, e.to_string()))?;
                Ok(SqlValue::Blob(blob))
            }
            _ => Err(fail(op, "cannot cast to dec")),
        },
        _ => Err(fail(op, "target must be 'int', 'float' or 'dec'")),
    }
}

fn apply_upper_lower(name: &str, ctx: &Context<'_>) -> Result<SqlValue, Error> {
    match arg(ctx, 0)? {
        Arg::Null => Ok(SqlValue::Null),
        Arg::Text(s) => {
            let out = if name == "$$upper" {
                s.to_uppercase()
            } else {
                s.to_lowercase()
            };
            Ok(SqlValue::Text(out))
        }
        _ => Err(fail(name, "needs a string")),
    }
}

fn apply_instant(name: &str, ctx: &Context<'_>) -> Result<SqlValue, Error> {
    let blob = match arg(ctx, 0)? {
        Arg::Null => return Ok(SqlValue::Null),
        Arg::Blob(b) => b,
        _ => return Err(fail(name, "needs an instant blob")),
    };
    let t = decode_instant(&blob, name).map_err(|e| fail(name, e.to_string()))?;
    let value = match name {
        "$$instant_year" => SqlValue::Integer(t.year() as i64),
        "$$instant_month" => SqlValue::Integer(t.month() as i64),
        "$$instant_day" => SqlValue::Integer(t.day() as i64),
        "$$instant_hour" => SqlValue::Integer(t.hour() as i64),
        "$$instant_minute" => SqlValue::Integer(t.minute() as i64),
        "$$instant_second" => SqlValue::Integer(t.second() as i64),
        "$$instant_weekday" => SqlValue::Integer(t.weekday().num_days_from_sunday() as i64),
        "$$instant_epoch" => {
            let nanos = t.timestamp() as i128 * 1_000_000_000 + t.timestamp_subsec_nanos() as i128;
            SqlValue::Blob(encode_dec(&nanos_to_dec(nanos)).to_vec())
        }
        "$$instant_to_rfc3339" => {
            SqlValue::Text(t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        }
        _ => return Err(fail(name, "unknown instant extractor")),
    };
    Ok(value)
}

fn apply_duration(name: &str, ctx: &Context<'_>) -> Result<SqlValue, Error> {
    let blob = match arg(ctx, 0)? {
        Arg::Null => return Ok(SqlValue::Null),
        Arg::Blob(b) => b,
        _ => return Err(fail(name, "needs a duration blob")),
    };
    let d = decode_duration(&blob, name).map_err(|e| fail(name, e.to_string()))?;
    let seconds = nanos_to_dec(duration_nanos(&d));
    let divisor = match name {
        "$$duration_as_seconds" => None,
        "$$duration_as_minutes" => Some(Decimal::new(60, 0)),
        "$$duration_as_hours" => Some(Decimal::new(3600, 0)),
        "$$duration_as_days" => Some(Decimal::new(86400, 0)),
        _ => return Err(fail(name, "unknown duration extractor")),
    };
    match divisor {
        None => Ok(SqlValue::Blob(encode_dec(&seconds).to_vec())),
        Some(div) => seconds
            .checked_div(div)
            .map(|v| SqlValue::Blob(encode_dec(&v).to_vec()))
            .ok_or_else(|| fail(name, "duration overflow")),
    }
}

fn apply_float_round(name: &str, ctx: &Context<'_>) -> Result<SqlValue, Error> {
    match arg(ctx, 0)? {
        Arg::Null => Ok(SqlValue::Null),
        Arg::Float(f) => {
            let out = match name {
                "$$float_round" => f.round(),
                "$$float_floor" => f.floor(),
                "$$float_ceil" => f.ceil(),
                _ => return Err(fail(name, "unknown float rounding")),
            };
            Ok(SqlValue::Real(out))
        }
        _ => Err(fail(name, "needs a float")),
    }
}

fn apply_dec_round_dp(ctx: &Context<'_>) -> Result<SqlValue, Error> {
    let op = "$$dec_round_dp";
    let blob = match arg(ctx, 0)? {
        Arg::Null => return Ok(SqlValue::Null),
        Arg::Blob(b) => b,
        _ => return Err(fail(op, "needs a decimal blob")),
    };
    let places = match arg(ctx, 1)? {
        Arg::Null => return Ok(SqlValue::Null),
        Arg::Int(n) => u32::try_from(n).map_err(|_| fail(op, "places must be non-negative"))?,
        _ => return Err(fail(op, "places must be an integer")),
    };
    let d = decode_dec(&blob, op).map_err(|e| fail(op, e.to_string()))?;
    Ok(SqlValue::Blob(encode_dec(&d.round_dp(places)).to_vec()))
}

fn apply_dec_to_string(ctx: &Context<'_>) -> Result<SqlValue, Error> {
    match arg(ctx, 0)? {
        Arg::Null => Ok(SqlValue::Null),
        Arg::Blob(blob) => {
            let d = decode_dec(&blob, "$$dec_to_string")
                .map_err(|e| fail("$$dec_to_string", e.to_string()))?;
            Ok(SqlValue::Text(d.to_string()))
        }
        _ => Err(fail("$$dec_to_string", "needs a decimal blob")),
    }
}


fn apply_panic(ctx: &Context<'_>) -> Result<SqlValue, Error> {
    let message = match arg(ctx, 0)? {
        Arg::Text(s) => s,
        Arg::Null => "<null>".to_string(),
        _ => "<non-string>".to_string(),
    };
    Err(Error::UserFunctionError(
        format!("grove-abort: {message}").into(),
    ))
}

fn apply_now() -> Result<SqlValue, Error> {
    Ok(SqlValue::Blob(encode_instant(&Utc::now()).to_vec()))
}

fn apply_today(ctx: &Context<'_>) -> Result<SqlValue, Error> {
    let op = "$$today";
    let midnight = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| fail(op, "midnight is not a valid time"))?
        .and_utc();
    match arg(ctx, 0)? {
        Arg::Null => Ok(SqlValue::Blob(encode_instant(&midnight).to_vec())),
        Arg::Blob(blob) => {
            let offset = decode_duration(&blob, op).map_err(|e| fail(op, e.to_string()))?;
            let total = duration_nanos(&offset);
            if !(0..24 * 3_600_000_000_000i128).contains(&total) {
                return Err(fail(op, "offset must be within [0, 24h)"));
            }
            midnight
                .checked_add_signed(offset)
                .map(|t| SqlValue::Blob(encode_instant(&t).to_vec()))
                .ok_or_else(|| fail(op, "date overflow"))
        }
        _ => Err(fail(op, "offset must be a duration or null")),
    }
}


pub fn register_scalars(conn: &Connection, config: &RuntimeConfig) -> rusqlite::Result<()> {
    let det = FunctionFlags::SQLITE_DETERMINISTIC;
    for (name, op) in [
        ("$$add", "$$add"),
        ("$$sub", "$$sub"),
        ("$$mul", "$$mul"),
        ("$$div", "$$div"),
        ("$$rem", "$$rem"),
        ("$$mod", "$$mod"),
    ] {
        let cfg = *config;
        conn.create_scalar_function(name, 2, det, move |ctx| apply_arith2(op, ctx, cfg))?;
        let cfg = *config;
        conn.create_scalar_function(name, 4, det, move |ctx| apply_arith4(op, ctx, cfg))?;
    }
    let cfg = *config;
    conn.create_scalar_function("$$neg", 1, det, move |ctx| apply_neg(ctx, cfg))?;
    let cfg = *config;
    conn.create_scalar_function("$$neg", 2, det, move |ctx| apply_neg(ctx, cfg))?;
    conn.create_scalar_function("$$cast", 2, det, apply_cast)?;
    for name in ["$$upper", "$$lower"] {
        conn.create_scalar_function(name, 1, det, move |ctx| apply_upper_lower(name, ctx))?;
    }
    for name in [
        "$$instant_year",
        "$$instant_month",
        "$$instant_day",
        "$$instant_hour",
        "$$instant_minute",
        "$$instant_second",
        "$$instant_weekday",
        "$$instant_epoch",
        "$$instant_to_rfc3339",
    ] {
        conn.create_scalar_function(name, 1, det, move |ctx| apply_instant(name, ctx))?;
    }
    for name in [
        "$$duration_as_seconds",
        "$$duration_as_minutes",
        "$$duration_as_hours",
        "$$duration_as_days",
    ] {
        conn.create_scalar_function(name, 1, det, move |ctx| apply_duration(name, ctx))?;
    }
    for name in ["$$float_round", "$$float_floor", "$$float_ceil"] {
        conn.create_scalar_function(name, 1, det, move |ctx| apply_float_round(name, ctx))?;
    }
    conn.create_scalar_function("$$dec_round_dp", 2, det, apply_dec_round_dp)?;
    conn.create_scalar_function("$$dec_to_string", 1, det, move |ctx| {
        apply_dec_to_string(ctx)
    })?;
    conn.create_scalar_function("$$panic", 1, FunctionFlags::default(), move |ctx| {
        apply_panic(ctx)
    })?;
    conn.create_scalar_function("$$now", 0, FunctionFlags::default(), move |_| apply_now())?;
    conn.create_scalar_function("$$today", 1, FunctionFlags::default(), move |ctx| {
        apply_today(ctx)
    })?;
    Ok(())
}
