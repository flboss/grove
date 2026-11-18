//! Canonical Grove storage codecs for `Dec`, `Instant`, and `Duration`.
//!
//! The encoded BLOBs are memcmp-ordered: comparing their bytes lexicographically
//! matches comparing the decoded values.

use std::fmt;

use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;

pub const DEC_BLOB_LEN: usize = 13;
pub const TIME_BLOB_LEN: usize = 12;

const SEC_BIAS: u64 = 0x8000_0000_0000_0000;
const NANOS_PER_SEC: i32 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobKind {
    Dec,
    Instant,
    Duration,
}

impl fmt::Display for BlobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobKind::Dec => write!(f, "Dec"),
            BlobKind::Instant => write!(f, "Instant"),
            BlobKind::Duration => write!(f, "Duration"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    InvalidLength {
        kind: BlobKind,
        expected: usize,
        actual: usize,
    },
    DecExponent {
        biased: u8,
    },
    DecSignificand,
    DecUnrepresentable,
    DecNoSplit,
    InvalidNanoseconds {
        kind: BlobKind,
        nanos: u32,
    },
    InstantOutOfRange {
        seconds: i64,
    },
    DurationOutOfRange {
        seconds: i64,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::InvalidLength {
                kind,
                expected,
                actual,
            } => write!(f, "malformed {kind} blob: length {actual}, want {expected}"),
            CodecError::DecExponent { biased } => {
                write!(f, "malformed Dec blob: biased exponent {biased}, want 0-56")
            }
            CodecError::DecSignificand => {
                write!(f, "malformed Dec blob: significand field exceeds 29 digits")
            }
            CodecError::DecUnrepresentable => {
                write!(f, "malformed Dec blob: unrepresentable mantissa or scale")
            }
            CodecError::DecNoSplit => {
                write!(f, "malformed Dec blob: no viable mantissa/scale split")
            }
            CodecError::InvalidNanoseconds { kind, nanos } => write!(
                f,
                "malformed {kind} blob: nanoseconds {nanos}, want 0-999999999"
            ),
            CodecError::InstantOutOfRange { seconds } => {
                write!(f, "malformed Instant blob: seconds {seconds} out of range")
            }
            CodecError::DurationOutOfRange { seconds } => {
                write!(f, "malformed Duration blob: seconds {seconds} out of range")
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// Encode a `Dec` as its canonical 13-byte memcmp-ordered BLOB.
pub fn encode_dec(value: Decimal) -> [u8; DEC_BLOB_LEN] {
    let normalized = value.normalize();
    if normalized.is_zero() {
        return [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    }
    let raw = normalized.mantissa();
    let negative = raw < 0;
    let mantissa = raw.unsigned_abs();
    let scale = normalized.scale();
    let digits = mantissa.ilog10() + 1;
    let biased = (digits as i32 - 1 - scale as i32 + 28) as u8;
    let aligned = mantissa * 10u128.pow(29 - digits);
    let mut out = [0u8; DEC_BLOB_LEN];
    out[0] = 0x80 | (biased << 1) | (aligned >> 96) as u8;
    out[1..13].copy_from_slice(&aligned.to_be_bytes()[4..16]);
    if negative {
        for b in out.iter_mut() {
            *b = !*b;
        }
    }
    out
}

/// Decode a canonical 13-byte `Dec` BLOB.
pub fn decode_dec(blob: &[u8]) -> Result<Decimal, CodecError> {
    if blob.len() != DEC_BLOB_LEN {
        return Err(CodecError::InvalidLength {
            kind: BlobKind::Dec,
            expected: DEC_BLOB_LEN,
            actual: blob.len(),
        });
    }
    let mut raw = [0u8; DEC_BLOB_LEN];
    raw.copy_from_slice(blob);
    let negative = raw[0] & 0x80 == 0;
    if negative {
        for b in raw.iter_mut() {
            *b = !*b;
        }
    }
    let biased = (raw[0] >> 1) & 0x3F;
    if biased > 56 {
        return Err(CodecError::DecExponent { biased });
    }
    let exp = biased as i32 - 28;
    let mut field: u128 = (raw[0] & 1) as u128;
    for b in &raw[1..13] {
        field = (field << 8) | *b as u128;
    }
    if field == 0 {
        return Ok(Decimal::ZERO);
    }
    if field >= 10u128.pow(29) {
        return Err(CodecError::DecSignificand);
    }
    for k in (0..=29).rev() {
        let div = 10u128.pow(k);
        if !field.is_multiple_of(div) {
            continue;
        }
        let mantissa = field / div;
        if mantissa >= (1u128 << 96) {
            continue;
        }
        let scale = mantissa.ilog10() as i32 + 1 - 1 - exp;
        if !(0..=28).contains(&scale) {
            continue;
        }
        let mut dec = Decimal::try_from_i128_with_scale(mantissa as i128, scale as u32)
            .map_err(|_| CodecError::DecUnrepresentable)?;
        if negative {
            dec = -dec;
        }
        return Ok(dec);
    }
    Err(CodecError::DecNoSplit)
}

fn encode_secs_nanos(secs: i64, nanos: u32) -> [u8; TIME_BLOB_LEN] {
    let mut out = [0u8; TIME_BLOB_LEN];
    out[0..8].copy_from_slice(&((secs as u64) ^ SEC_BIAS).to_be_bytes());
    out[8..12].copy_from_slice(&nanos.to_be_bytes());
    out
}

fn decode_secs_nanos(blob: &[u8], kind: BlobKind) -> Result<(i64, u32), CodecError> {
    if blob.len() != TIME_BLOB_LEN {
        return Err(CodecError::InvalidLength {
            kind,
            expected: TIME_BLOB_LEN,
            actual: blob.len(),
        });
    }
    let mut sec_bytes = [0u8; 8];
    sec_bytes.copy_from_slice(&blob[0..8]);
    let mut nano_bytes = [0u8; 4];
    nano_bytes.copy_from_slice(&blob[8..12]);
    let secs = (u64::from_be_bytes(sec_bytes) ^ SEC_BIAS) as i64;
    let nanos = u32::from_be_bytes(nano_bytes);
    if nanos >= NANOS_PER_SEC as u32 {
        return Err(CodecError::InvalidNanoseconds { kind, nanos });
    }
    Ok((secs, nanos))
}

/// Encode an `Instant` as its canonical 12-byte memcmp-ordered BLOB.
pub fn encode_instant(t: DateTime<Utc>) -> [u8; TIME_BLOB_LEN] {
    encode_secs_nanos(t.timestamp(), t.timestamp_subsec_nanos())
}

/// Decode a canonical 12-byte `Instant` BLOB.
pub fn decode_instant(blob: &[u8]) -> Result<DateTime<Utc>, CodecError> {
    let (secs, nanos) = decode_secs_nanos(blob, BlobKind::Instant)?;
    DateTime::from_timestamp(secs, nanos).ok_or(CodecError::InstantOutOfRange { seconds: secs })
}

/// Encode a `Duration` as its canonical 12-byte memcmp-ordered BLOB.
pub fn encode_duration(d: TimeDelta) -> [u8; TIME_BLOB_LEN] {
    let secs = d.num_seconds();
    let nanos = d.subsec_nanos();
    let (secs, nanos) = if nanos < 0 {
        (secs - 1, (nanos + NANOS_PER_SEC) as u32)
    } else {
        (secs, nanos as u32)
    };
    encode_secs_nanos(secs, nanos)
}

/// Decode a canonical 12-byte `Duration` BLOB.
pub fn decode_duration(blob: &[u8]) -> Result<TimeDelta, CodecError> {
    let (secs, nanos) = decode_secs_nanos(blob, BlobKind::Duration)?;
    TimeDelta::new(secs, nanos).ok_or(CodecError::DurationOutOfRange { seconds: secs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn round_trip(d: Decimal) {
        assert_eq!(decode_dec(&encode_dec(d)), Ok(d.normalize()));
    }

    #[test]
    fn dec_zero_canonical() {
        let zero = [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(encode_dec(dec!(0)), zero);
        assert_eq!(encode_dec(dec!(-0)), zero);
        assert_eq!(decode_dec(&zero), Ok(Decimal::ZERO));
    }

    #[test]
    fn dec_one_bytes() {
        let bytes = encode_dec(dec!(1));
        assert_eq!(bytes[0], 0xB8);
        let mut field = [0u8; 16];
        field[4..16].copy_from_slice(&bytes[1..13]);
        assert_eq!(u128::from_be_bytes(field), 10u128.pow(28));
        assert_eq!(decode_dec(&bytes), Ok(dec!(1)));
    }

    #[test]
    fn dec_negative_inverse_positive() {
        let pos = encode_dec(dec!(1));
        let neg = encode_dec(dec!(-1));
        let inverted: Vec<u8> = pos.iter().map(|b| !b).collect();
        assert_eq!(neg, inverted.as_slice());
        assert_eq!(decode_dec(&neg), Ok(dec!(-1)));
    }

    #[test]
    fn dec_trailing_zeros_normalized() {
        assert_eq!(encode_dec(dec!(5.00)), encode_dec(dec!(5)));
        assert_eq!(decode_dec(&encode_dec(dec!(5.00))), Ok(dec!(5)));
    }

    #[test]
    fn dec_round_trips() {
        for d in [
            dec!(0),
            dec!(1),
            dec!(-1),
            dec!(5),
            dec!(0.5),
            dec!(-0.5),
            dec!(123.456),
            dec!(-123.456),
            dec!(250),
            dec!(0.1),
            dec!(1000000),
            dec!(79228162514264337593543950335),
            dec!(-79228162514264337593543950335),
            dec!(1.234567890123456789012345678),
            dec!(0.0000000000000000000000000001),
            dec!(9),
        ] {
            round_trip(d);
        }
    }

    #[test]
    fn dec_bytes_order_matches_value_order() {
        let values = [
            dec!(-79228162514264337593543950335),
            dec!(-100.5),
            dec!(-1),
            dec!(-0.01),
            dec!(0),
            dec!(0.0000000000000000000000000001),
            dec!(0.01),
            dec!(0.5),
            dec!(1),
            dec!(1.234567890123456789012345678),
            dec!(2),
            dec!(10),
            dec!(100.25),
            dec!(79228162514264337593543950335),
        ];
        let mut prev: Option<[u8; 13]> = None;
        for d in values {
            let bytes = encode_dec(d);
            if let Some(p) = prev {
                assert!(p < bytes, "{d} sorts after previous");
            }
            prev = Some(bytes);
        }
    }

    #[test]
    fn dec_malformed_is_error() {
        assert!(decode_dec(&[]).is_err());
        assert!(decode_dec(&[0u8; 12]).is_err());
        assert!(decode_dec(&[0u8; 14]).is_err());
        let mut bad_exp = [0u8; 13];
        bad_exp[0] = 0xF2;
        assert!(decode_dec(&bad_exp).is_err());
        let mut big_field = [0u8; 13];
        let field = 10u128.pow(29);
        big_field[0] = 0xB8 | ((field >> 96) & 1) as u8;
        big_field[1..13].copy_from_slice(&field.to_be_bytes()[4..16]);
        assert!(decode_dec(&big_field).is_err());
        assert!(decode_dec(&[0u8; 13]).is_err());
    }

    #[test]
    fn instant_epoch_bytes() {
        let epoch = DateTime::from_timestamp(0, 0).unwrap();
        let bytes = encode_instant(epoch);
        assert_eq!(bytes, [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_instant(&bytes), Ok(epoch));
    }

    #[test]
    fn instant_round_trips() {
        for (secs, nanos) in [
            (0, 0),
            (1_700_000_000, 123_456_789),
            (-1, 0),
            (-1, 500_000_000),
            (253_402_300_799, 999_999_999),
        ] {
            let t = DateTime::from_timestamp(secs, nanos).unwrap();
            assert_eq!(decode_instant(&encode_instant(t)), Ok(t));
        }
    }

    #[test]
    fn instant_malformed_is_error() {
        assert!(decode_instant(&[0u8; 11]).is_err());
        let mut bad_nanos = encode_instant(DateTime::from_timestamp(0, 0).unwrap());
        bad_nanos[8..12].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        assert!(decode_instant(&bad_nanos).is_err());
    }

    #[test]
    fn duration_negative_floored_seconds() {
        let bytes = encode_duration(TimeDelta::milliseconds(-1500));
        assert_eq!(&bytes[0..8], &((-2i64 as u64) ^ SEC_BIAS).to_be_bytes());
        assert_eq!(&bytes[8..12], &500_000_000u32.to_be_bytes());
    }

    #[test]
    fn duration_round_trips() {
        for d in [
            TimeDelta::zero(),
            TimeDelta::seconds(90),
            TimeDelta::milliseconds(1500),
            TimeDelta::milliseconds(-1500),
            TimeDelta::nanoseconds(1),
            TimeDelta::nanoseconds(-1),
            TimeDelta::MAX,
            TimeDelta::MIN,
        ] {
            let back = decode_duration(&encode_duration(d)).unwrap();
            assert_eq!(back.num_seconds(), d.num_seconds());
            assert_eq!(back.subsec_nanos(), d.subsec_nanos());
        }
    }
}
