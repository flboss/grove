use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use grove_types::Span;
use rust_decimal::Decimal;

use crate::error::ExecError;

/// A scalar Grove value
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Int(i64),
    Float(f64),
    Dec(Decimal),
    String(String),
    Bool(bool),
    Instant(DateTime<Utc>),
    Duration(TimeDelta),
}

/// A Grove value
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    None,
    Some(Box<Val>),
    Scalar(Scalar),
    List(Vec<Val>),
    Tuple(Vec<Val>),
    Record(Vec<(Arc<str>, Val)>),
}

impl Val {
    pub fn cmp(&self, other: &Val, span: Span) -> Result<std::cmp::Ordering, ExecError> {
        match (self, other) {
            (Val::Scalar(x), Val::Scalar(y)) => x.cmp(y, span),
            (Val::Tuple(xs), Val::Tuple(ys)) => {
                if xs.len() != ys.len() {
                    return Err(ExecError::Integrity {
                        message: "mismatched tuple comparison (internal error)".to_string(),
                    });
                }
                for (x, y) in xs.iter().zip(ys) {
                    match x.cmp(y, span)? {
                        std::cmp::Ordering::Equal => {}
                        other => return Ok(other),
                    }
                }
                Ok(std::cmp::Ordering::Equal)
            }
            _ => Err(ExecError::Integrity {
                message: "mismatched comparison operands (internal error)".to_string(),
            }),
        }
    }
}

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
            (Scalar::Bool(x), Scalar::Bool(y)) => Ok(x.cmp(y)),
            (Scalar::Instant(x), Scalar::Instant(y)) => Ok(x.cmp(y)),
            (Scalar::Duration(x), Scalar::Duration(y)) => Ok(x.cmp(y)),
            _ => Err(ExecError::Integrity {
                message: "mismatched comparison operands (internal error)".to_string(),
            }),
        }
    }
}

/// Field-name interner owned by the executor
#[derive(Debug, Default)]
pub struct Interner {
    names: HashSet<Arc<str>>,
}

impl Interner {
    pub fn intern(&mut self, name: &str) -> Arc<str> {
        if let Some(existing) = self.names.get(name) {
            return existing.clone();
        }
        let arc: Arc<str> = Arc::from(name);
        self.names.insert(arc.clone());
        arc
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// TODO: show as larger units (m/h/d)
fn fmt_duration(d: &TimeDelta) -> String {
    let total_nanos = d
        .num_nanoseconds()
        .map(|ns| ns as i128)
        .unwrap_or_else(|| d.num_seconds() as i128 * 1_000_000_000);
    let sign = if total_nanos < 0 { "-" } else { "" };
    let abs = total_nanos.unsigned_abs();
    let secs = abs / 1_000_000_000;
    let nanos = abs % 1_000_000_000;
    if nanos == 0 {
        format!("{sign}{secs}s")
    } else {
        let frac = format!("{nanos:09}");
        format!("{sign}{secs}.{}s", frac.trim_end_matches('0'))
    }
}

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::Int(n) => write!(f, "{n}"),
            Scalar::Float(x) => write!(f, "{x:?}"),
            Scalar::Dec(d) => write!(f, "{d}"),
            Scalar::String(s) => write!(f, "{s:?}"),
            Scalar::Bool(b) => write!(f, "{b}"),
            Scalar::Instant(t) => write!(
                f,
                "{}",
                t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            ),
            Scalar::Duration(d) => write!(f, "{}", fmt_duration(d)),
        }
    }
}

impl fmt::Display for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::None => write!(f, "none"),
            Val::Some(inner) => write!(f, "some({inner})"),
            Val::Scalar(s) => write!(f, "{s}"),
            Val::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Val::Tuple(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, ")")
            }
            Val::Record(fields) => {
                write!(f, "{{")?;
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {value}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }

    #[test]
    fn scalar_display() {
        assert_eq!(Scalar::Int(-7).to_string(), "-7");
        assert_eq!(Scalar::Float(1.0).to_string(), "1.0");
        assert_eq!(Scalar::Float(1.5).to_string(), "1.5");
        assert_eq!(Scalar::Bool(true).to_string(), "true");
        assert_eq!(Scalar::String("a\"b".to_string()).to_string(), "\"a\\\"b\"");
        assert_eq!(Scalar::Dec(dec!(30.00)).to_string(), "30.00");
    }

    #[test]
    fn option_display() {
        assert_eq!(Val::None.to_string(), "none");
        assert_eq!(some(Val::Scalar(Scalar::Int(3))).to_string(), "some(3)");
        assert_eq!(
            some(some(Val::Scalar(Scalar::Int(3)))).to_string(),
            "some(some(3))"
        );
    }

    #[test]
    fn duration_display() {
        let s = |secs: i64| Scalar::Duration(TimeDelta::seconds(secs)).to_string();
        assert_eq!(s(3600), "3600s");
        assert_eq!(s(0), "0s");
        assert_eq!(s(-90), "-90s");
        assert_eq!(
            Scalar::Duration(TimeDelta::milliseconds(1500)).to_string(),
            "1.5s"
        );
        assert_eq!(
            Scalar::Duration(TimeDelta::nanoseconds(1_234_567_890)).to_string(),
            "1.23456789s"
        );
    }

    #[test]
    fn instant_display() {
        let t = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(Scalar::Instant(t).to_string(), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn instant_display_subseconds() {
        // AutoSi picks millis/micros/nanos from the value; UTC
        // always renders as `Z`.
        let at = |nanos: u32| {
            Scalar::Instant(DateTime::from_timestamp(1_700_000_000, nanos).unwrap()).to_string()
        };
        assert_eq!(at(0), "2023-11-14T22:13:20Z");
        assert_eq!(at(123_000_000), "2023-11-14T22:13:20.123Z");
        assert_eq!(at(123_456_000), "2023-11-14T22:13:20.123456Z");
        assert_eq!(at(123_456_789), "2023-11-14T22:13:20.123456789Z");
        assert_eq!(at(1), "2023-11-14T22:13:20.000000001Z");
    }

    #[test]
    fn composite_display() {
        let mut interner = Interner::default();
        let x = interner.intern("x");
        let y = interner.intern("y");
        assert!(Arc::ptr_eq(&x, &interner.intern("x")));
        assert!(!Arc::ptr_eq(&x, &y));
        assert_eq!(interner.len(), 2);
        let list = Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]);
        assert_eq!(list.to_string(), "[1, none]");
        let tuple = Val::Tuple(vec![
            Val::Scalar(Scalar::String("a".to_string())),
            Val::Scalar(Scalar::Int(1)),
        ]);
        assert_eq!(tuple.to_string(), "(\"a\", 1)");
        let record = Val::Record(vec![(x, Val::Scalar(Scalar::Int(1))), (y, list)]);
        assert_eq!(record.to_string(), "{x: 1, y: [1, none]}");
        assert_eq!(Val::List(vec![]).to_string(), "[]");
    }
}
