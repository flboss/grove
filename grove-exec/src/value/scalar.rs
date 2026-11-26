use std::hash::{Hash, Hasher};

use chrono::{DateTime, TimeDelta, Utc};
use grove_types::Span;
use rust_decimal::Decimal;

use crate::error::ExecError;

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Int(i64),
    Float(f64),
    Dec(Decimal),
    String(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Instant(DateTime<Utc>),
    Duration(TimeDelta),
}

impl Scalar {
    pub(crate) fn kind_str(&self) -> &'static str {
        match self {
            Scalar::Int(_) => "Int",
            Scalar::Float(_) => "Float",
            Scalar::Dec(_) => "Dec",
            Scalar::String(_) => "String",
            Scalar::Bytes(_) => "Bytes",
            Scalar::Bool(_) => "Bool",
            Scalar::Instant(_) => "Instant",
            Scalar::Duration(_) => "Duration",
        }
    }
}

impl Hash for Scalar {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Scalar::Int(n) => {
                0u8.hash(state);
                n.hash(state);
            }
            Scalar::Float(x) => {
                1u8.hash(state);
                let bits = if x.is_nan() {
                    f64::NAN.to_bits()
                } else if *x == 0.0 {
                    0.0f64.to_bits()
                } else {
                    x.to_bits()
                };
                bits.hash(state);
            }
            Scalar::Dec(d) => {
                2u8.hash(state);
                d.hash(state);
            }
            Scalar::String(s) => {
                3u8.hash(state);
                s.hash(state);
            }
            Scalar::Bool(b) => {
                4u8.hash(state);
                b.hash(state);
            }
            Scalar::Instant(t) => {
                5u8.hash(state);
                t.hash(state);
            }
            Scalar::Duration(d) => {
                6u8.hash(state);
                d.hash(state);
            }
            Scalar::Bytes(bytes) => {
                7u8.hash(state);
                bytes.hash(state);
            }
        }
    }
}

/// The storage row a value was materialized from
impl Scalar {
    pub fn cmp(&self, other: &Scalar, span: Span) -> Result<std::cmp::Ordering, ExecError> {
        match (self, other) {
            (Scalar::Int(x), Scalar::Int(y)) => Ok(x.cmp(y)),
            (Scalar::Float(x), Scalar::Float(y)) => {
                x.partial_cmp(y).ok_or_else(|| ExecError::Abort {
                    message: "cannot order NaN values".to_string(),
                    span,
                })
            }
            (Scalar::Dec(x), Scalar::Dec(y)) => Ok(x.cmp(y)),
            (Scalar::String(x), Scalar::String(y)) => Ok(x.cmp(y)),
            (Scalar::Bytes(x), Scalar::Bytes(y)) => Ok(x.cmp(y)),
            (Scalar::Bool(x), Scalar::Bool(y)) => Ok(x.cmp(y)),
            (Scalar::Instant(x), Scalar::Instant(y)) => Ok(x.cmp(y)),
            (Scalar::Duration(x), Scalar::Duration(y)) => Ok(x.cmp(y)),
            _ => Err(ExecError::Internal {
                message: "mismatched comparison operands".to_string(),
            }),
        }
    }

    pub fn eq(&self, other: &Scalar) -> Option<bool> {
        match (self, other) {
            (Scalar::Int(x), Scalar::Int(y)) => Some(x == y),
            (Scalar::Float(x), Scalar::Float(y)) => Some(x == y),
            (Scalar::Dec(x), Scalar::Dec(y)) => Some(x == y),
            (Scalar::String(x), Scalar::String(y)) => Some(x == y),
            (Scalar::Bytes(x), Scalar::Bytes(y)) => Some(x == y),
            (Scalar::Bool(x), Scalar::Bool(y)) => Some(x == y),
            (Scalar::Instant(x), Scalar::Instant(y)) => Some(x == y),
            (Scalar::Duration(x), Scalar::Duration(y)) => Some(x == y),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::hash::DefaultHasher;

    use rust_decimal::dec;

    use super::*;

    #[test]
    fn scalar_display() {
        assert_eq!(Scalar::Int(-7).to_string(), "-7");
        assert_eq!(Scalar::Float(1.0).to_string(), "1.0f");
        assert_eq!(Scalar::Float(1.5).to_string(), "1.5f");
        assert_eq!(Scalar::Bool(true).to_string(), "true");
        assert_eq!(Scalar::String("a\"b".to_string()).to_string(), "\"a\\\"b\"");
        assert_eq!(Scalar::Dec(dec!(30.00)).to_string(), "30.00");
        assert_eq!(Scalar::Bytes(Vec::new()).to_string(), "hex\"\"");
        assert_eq!(
            Scalar::Bytes(vec![0x00, 0x01, 0xde, 0xff]).to_string(),
            "hex\"0001deff\""
        );
    }

    #[test]
    fn scalar_bytes_compare_hash() {
        let span = Span::new(0, 0);
        let a = Scalar::Bytes(vec![0x01]);
        let b = Scalar::Bytes(vec![0x02]);

        assert_eq!(a.cmp(&b, span), Ok(Ordering::Less));
        assert_eq!(a.eq(&a), Some(true));
        assert_eq!(a.eq(&b), Some(false));
        assert_eq!(a.eq(&Scalar::String("01".to_string())), None);

        let hash = |value: &Scalar| {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&a), hash(&a.clone()));
        assert_ne!(hash(&a), hash(&Scalar::String("01".to_string())));
    }
}
