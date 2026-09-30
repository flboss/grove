
use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Error, Result};
use rust_decimal::Decimal;

use super::decode::{decode_dec, decode_duration, decode_instant, encode_dec, encode_instant};
use super::funcs::{duration_from_nanos, duration_nanos};
use super::{DecArithmetic, RuntimeConfig};

fn step_blob(ctx: &Context<'_>) -> Result<Vec<u8>, Error> {
    ctx.get::<Vec<u8>>(0)
        .map_err(|_| Error::InvalidFunctionParameterType(0, rusqlite::types::Type::Blob))
}

fn abort(op: &str, msg: impl Into<String>) -> Error {
    Error::UserFunctionError(format!("{op}: {}", msg.into()).into())
}

fn div_round_nanos(total: i128, count: i64) -> Result<i128, Error> {
    let count = count as i128;
    let q = total / count;
    let r = total % count;
    if r == 0 {
        return Ok(q);
    }
    let twice = r.unsigned_abs() * 2;
    if twice >= count.unsigned_abs() {
        Ok(q + r.signum())
    } else {
        Ok(q)
    }
}


pub struct SumDec {
    config: DecArithmetic,
}

impl Aggregate<Option<Decimal>, SqlValue> for SumDec {
    fn init(&self, _ctx: &mut Context<'_>) -> Result<Option<Decimal>> {
        Ok(None)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Option<Decimal>) -> Result<()> {
        let blob = step_blob(ctx)?;
        let value =
            decode_dec(&blob, "$$sum_dec").map_err(|e| abort("$$sum_dec", e.to_string()))?;
        *acc = Some(match acc {
            None => value,
            Some(sum) => super::funcs::add_dec("$$sum_dec", *sum, value, self.config)
                .map_err(|e| abort("$$sum_dec", e.to_string()))?,
        });
        Ok(())
    }

    fn finalize(&self, _ctx: &mut Context<'_>, acc: Option<Option<Decimal>>) -> Result<SqlValue> {
        match acc.flatten() {
            None => Ok(SqlValue::Null),
            Some(sum) => Ok(SqlValue::Blob(encode_dec(&sum).to_vec())),
        }
    }
}

pub struct AvgDec {
    config: DecArithmetic,
}

impl Aggregate<Option<(Decimal, i64)>, SqlValue> for AvgDec {
    fn init(&self, _ctx: &mut Context<'_>) -> Result<Option<(Decimal, i64)>> {
        Ok(None)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Option<(Decimal, i64)>) -> Result<()> {
        let blob = step_blob(ctx)?;
        let value =
            decode_dec(&blob, "$$avg_dec").map_err(|e| abort("$$avg_dec", e.to_string()))?;
        *acc = Some(match acc {
            None => (value, 1),
            Some((sum, n)) => (
                super::funcs::add_dec("$$avg_dec", *sum, value, self.config)
                    .map_err(|e| abort("$$avg_dec", e.to_string()))?,
                *n + 1,
            ),
        });
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut Context<'_>,
        acc: Option<Option<(Decimal, i64)>>,
    ) -> Result<SqlValue> {
        match acc.flatten() {
            None => Ok(SqlValue::Null),
            Some((sum, n)) => {
                let mean = super::funcs::div_dec("$$avg_dec", sum, Decimal::from(n), self.config)
                    .map_err(|e| abort("$$avg_dec", e.to_string()))?;
                Ok(SqlValue::Blob(encode_dec(&mean).to_vec()))
            }
        }
    }
}

pub struct SumDur;

impl Aggregate<Option<chrono::TimeDelta>, SqlValue> for SumDur {
    fn init(&self, _ctx: &mut Context<'_>) -> Result<Option<chrono::TimeDelta>> {
        Ok(None)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Option<chrono::TimeDelta>) -> Result<()> {
        let blob = step_blob(ctx)?;
        let value =
            decode_duration(&blob, "$$sum_dur").map_err(|e| abort("$$sum_dur", e.to_string()))?;
        *acc = Some(match acc {
            None => value,
            Some(sum) => sum
                .checked_add(&value)
                .ok_or_else(|| abort("$$sum_dur", "duration overflow"))?,
        });
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut Context<'_>,
        acc: Option<Option<chrono::TimeDelta>>,
    ) -> Result<SqlValue> {
        use super::decode::encode_duration;
        match acc.flatten() {
            None => Ok(SqlValue::Null),
            Some(sum) => Ok(SqlValue::Blob(encode_duration(&sum).to_vec())),
        }
    }
}

pub struct AvgDur;

impl Aggregate<Option<(i128, i64)>, SqlValue> for AvgDur {
    fn init(&self, _ctx: &mut Context<'_>) -> Result<Option<(i128, i64)>> {
        Ok(None)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Option<(i128, i64)>) -> Result<()> {
        let blob = step_blob(ctx)?;
        let value =
            decode_duration(&blob, "$$avg_dur").map_err(|e| abort("$$avg_dur", e.to_string()))?;
        *acc = Some(match acc {
            None => (duration_nanos(&value), 1),
            Some((total, n)) => (*total + duration_nanos(&value), *n + 1),
        });
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut Context<'_>,
        acc: Option<Option<(i128, i64)>>,
    ) -> Result<SqlValue> {
        use super::decode::encode_duration;
        match acc.flatten() {
            None => Ok(SqlValue::Null),
            Some((total, n)) => {
                let mean = div_round_nanos(total, n)?;
                let span = duration_from_nanos("$$avg_dur", mean)?;
                Ok(SqlValue::Blob(encode_duration(&span).to_vec()))
            }
        }
    }
}

pub struct AvgInstant;

impl Aggregate<Option<(i128, i64)>, SqlValue> for AvgInstant {
    fn init(&self, _ctx: &mut Context<'_>) -> Result<Option<(i128, i64)>> {
        Ok(None)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Option<(i128, i64)>) -> Result<()> {
        let blob = step_blob(ctx)?;
        let value = decode_instant(&blob, "$$avg_instant")
            .map_err(|e| abort("$$avg_instant", e.to_string()))?;
        let nanos =
            value.timestamp() as i128 * 1_000_000_000 + value.timestamp_subsec_nanos() as i128;
        *acc = Some(match acc {
            None => (nanos, 1),
            Some((total, n)) => (*total + nanos, *n + 1),
        });
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut Context<'_>,
        acc: Option<Option<(i128, i64)>>,
    ) -> Result<SqlValue> {
        match acc.flatten() {
            None => Ok(SqlValue::Null),
            Some((total, n)) => {
                let mean = div_round_nanos(total, n)?;
                let secs = mean.div_euclid(1_000_000_000);
                let nanos = mean.rem_euclid(1_000_000_000);
                let secs = i64::try_from(secs)
                    .map_err(|_| abort("$$avg_instant", "instant out of range"))?;
                let t = chrono::DateTime::from_timestamp(secs, nanos as u32)
                    .ok_or_else(|| abort("$$avg_instant", "instant out of range"))?;
                Ok(SqlValue::Blob(encode_instant(&t).to_vec()))
            }
        }
    }
}

pub fn register_aggregates(conn: &Connection, config: &RuntimeConfig) -> rusqlite::Result<()> {
    let det = FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_aggregate_function(
        "$$sum_dec",
        1,
        det,
        SumDec {
            config: config.dec_arithmetic,
        },
    )?;
    conn.create_aggregate_function(
        "$$avg_dec",
        1,
        det,
        AvgDec {
            config: config.dec_arithmetic,
        },
    )?;
    conn.create_aggregate_function("$$sum_dur", 1, det, SumDur)?;
    conn.create_aggregate_function("$$avg_dur", 1, det, AvgDur)?;
    conn.create_aggregate_function("$$avg_instant", 1, det, AvgInstant)?;
    Ok(())
}
