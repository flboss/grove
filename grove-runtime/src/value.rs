
use chrono::{DateTime, TimeDelta, Utc};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Dec(Decimal),
    String(String),
    Bool(bool),
    Instant(DateTime<Utc>),
    Duration(TimeDelta),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Record(Vec<(String, Value)>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "none"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Dec(d) => write!(f, "{d}"),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Instant(t) => write!(
                f,
                "{}",
                t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            ),
            Value::Duration(d) => {
                let total = d.num_seconds() as i128 * 1_000_000_000 + d.subsec_nanos() as i128;
                let (sign, total) = if total < 0 {
                    ("-", -total)
                } else {
                    ("", total)
                };
                let (secs, frac) = (total / 1_000_000_000, total % 1_000_000_000);
                if frac == 0 {
                    write!(f, "{sign}{secs}s")
                } else {
                    write!(f, "{sign}{secs}.{:09}s", frac)
                }
            }
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Tuple(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, ")")
            }
            Value::Record(fields) => {
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
