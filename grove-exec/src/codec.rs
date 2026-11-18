use chrono::{DateTime, TimeDelta, Utc};
use grove_codec::CodecError;
use rust_decimal::Decimal;

use crate::error::ExecError;

pub(crate) use grove_codec::{encode_dec, encode_duration, encode_instant};

fn malformed(err: CodecError) -> ExecError {
    ExecError::Integrity {
        message: err.to_string(),
    }
}

pub(crate) fn decode_dec(blob: &[u8]) -> Result<Decimal, ExecError> {
    grove_codec::decode_dec(blob).map_err(malformed)
}

pub(crate) fn decode_instant(blob: &[u8]) -> Result<DateTime<Utc>, ExecError> {
    grove_codec::decode_instant(blob).map_err(malformed)
}

pub(crate) fn decode_duration(blob: &[u8]) -> Result<TimeDelta, ExecError> {
    grove_codec::decode_duration(blob).map_err(malformed)
}
