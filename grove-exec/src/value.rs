use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::{self, Write};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use chrono::{DateTime, TimeDelta, Utc};
use grove_schema::validated::{Enum, EnumId, EnumValue, FieldId};
use grove_types::Span;
use rust_decimal::Decimal;

use crate::error::ExecError;
use crate::storage::Key;

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

impl Scalar {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Scalar::Int(_) => "Int",
            Scalar::Float(_) => "Float",
            Scalar::Dec(_) => "Dec",
            Scalar::String(_) => "String",
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
        }
    }
}

/// A Grove value
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    None,
    Some(Box<Val>),
    Scalar(Scalar),
    Enum(EnumId, usize),
    List(Vec<Val>),
    Tuple(Vec<Val>),
    Record(Vec<(Rc<str>, Val)>),
    /// Unresolved navigation link of the `parent` row
    Ref {
        field: FieldId,
        parent: Key,
    },
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

    pub fn eq(&self, other: &Val) -> Option<bool> {
        match (self, other) {
            (Val::None, Val::None) => Some(true),
            (Val::Some(x), Val::Some(y)) => Some(x.as_ref().eq(y)?),
            (Val::None, Val::Some(_)) | (Val::Some(_), Val::None) => Some(false),
            (Val::Scalar(x), Val::Scalar(y)) => x.eq(y),
            (Val::Enum(a, ai), Val::Enum(b, bi)) => {
                if a == b {
                    Some(ai == bi)
                } else {
                    None
                }
            }
            (Val::List(xs), Val::List(ys)) => {
                if xs.len() != ys.len() {
                    return Some(false);
                }
                xs.iter().zip(ys).try_fold(true, |all_equal, (x, y)| {
                    x.eq(y).map(|equal| all_equal & equal)
                })
            }
            (Val::Tuple(xs), Val::Tuple(ys)) => {
                if xs.len() != ys.len() {
                    return None;
                }
                xs.iter().zip(ys).try_fold(true, |all_equal, (x, y)| {
                    x.eq(y).map(|equal| all_equal & equal)
                })
            }
            (Val::Record(xs), Val::Record(ys)) => {
                if xs.len() != ys.len() {
                    return None;
                }
                xs.iter().try_fold(true, |all_equal, (name, x)| {
                    let (_, y) = ys.iter().find(|(n, _)| n == name)?;
                    x.eq(y).map(|equal| all_equal & equal)
                })
            }
            (Val::Ref { .. }, _) | (_, Val::Ref { .. }) => None,
            _ => None,
        }
    }

    pub fn contains_ref(&self) -> bool {
        match self {
            Val::Some(val) => val.contains_ref(),
            Val::List(vals) | Val::Tuple(vals) => vals.iter().any(Val::contains_ref),
            Val::Record(items) => items.iter().any(|(_, val)| val.contains_ref()),
            Val::Ref { .. } => true,
            _ => false,
        }
    }
}

pub(crate) fn rewrap_optional(optional: bool, value: Val) -> Val {
    if optional {
        Val::Some(Box::new(value))
    } else {
        value
    }
}

pub(crate) fn enum_value_scalar(value: EnumValue) -> Scalar {
    match value {
        EnumValue::Int(n) => Scalar::Int(n),
        EnumValue::String(s) => Scalar::String(s),
    }
}

pub(crate) fn enum_variant_scalar(enum_def: &Enum, index: usize) -> Option<Scalar> {
    enum_def
        .variants
        .get(index)
        .map(|variant| enum_value_scalar(variant.value.clone()))
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

    pub fn eq(&self, other: &Scalar) -> Option<bool> {
        match (self, other) {
            (Scalar::Int(x), Scalar::Int(y)) => Some(x == y),
            (Scalar::Float(x), Scalar::Float(y)) => Some(x == y),
            (Scalar::Dec(x), Scalar::Dec(y)) => Some(x == y),
            (Scalar::String(x), Scalar::String(y)) => Some(x == y),
            (Scalar::Bool(x), Scalar::Bool(y)) => Some(x == y),
            (Scalar::Instant(x), Scalar::Instant(y)) => Some(x == y),
            (Scalar::Duration(x), Scalar::Duration(y)) => Some(x == y),
            _ => None,
        }
    }
}

/// Field-name interner owned by the executor
#[derive(Debug, Default)]
pub struct Interner {
    names: HashSet<Rc<str>>,
}

impl Interner {
    pub fn intern<'a>(&mut self, name: impl Into<Cow<'a, str>>) -> Rc<str> {
        let name = name.into();
        if let Some(existing) = self.names.get(name.as_ref()) {
            return Rc::clone(existing);
        }
        let arc: Rc<str> = Rc::from(name.into_owned());
        self.names.insert(Rc::clone(&arc));
        arc
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Total nanoseconds of a Duration
pub fn total_nanos(td: &TimeDelta) -> i128 {
    td.num_seconds() as i128 * NANOS_PER_SEC as i128 + td.subsec_nanos() as i128
}

pub const NANOS_PER_SEC: u128 = 1_000_000_000;
pub const NANOS_PER_MIN: u128 = 60 * NANOS_PER_SEC;
pub const NANOS_PER_HOUR: u128 = 60 * NANOS_PER_MIN;
pub const NANOS_PER_DAY: u128 = 24 * NANOS_PER_HOUR;

fn fmt_duration(td: &TimeDelta) -> String {
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

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::Int(n) => write!(f, "{n}"),
            Scalar::Float(x) => write!(f, "{x:?}"),
            Scalar::Dec(d) => write!(f, "{d}{}", if d.scale() == 0 { ".0" } else { "" }),
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
            Val::Enum(id, variant) => write!(f, "enum({}:{})", id.index(), variant),
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
            Val::Ref { field, .. } => {
                write!(f, "ref({}:{})", field.struct_id().index(), field.local())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::harness::{int, rec};

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

    #[test]
    fn record_eq_order_independent() {
        assert_eq!(
            rec(vec![("b", int(2)), ("a", int(1))]).eq(&rec(vec![("a", int(1)), ("b", int(2))])),
            Some(true)
        );
        assert_eq!(
            rec(vec![("b", int(2)), ("a", int(1))]).eq(&rec(vec![("a", int(9)), ("b", int(2))])),
            Some(false)
        );
        assert_eq!(
            rec(vec![("b", int(2)), ("a", int(1))]).eq(&rec(vec![("a", int(1)), ("c", int(2))])),
            None
        );
        assert_eq!(
            rec(vec![("a", int(1))]).eq(&rec(vec![("a", int(1)), ("b", int(2))])),
            None
        );
        assert_eq!(
            rec(vec![
                ("outer", rec(vec![("y", int(2)), ("x", int(1))])),
                ("z", int(3))
            ])
            .eq(&rec(vec![
                ("z", int(3)),
                ("outer", rec(vec![("x", int(1)), ("y", int(2))]))
            ])),
            Some(true)
        );
    }

    #[test]
    fn enum_display_and_equality() {
        let high = Val::Enum(EnumId::new(0), 1);
        assert_eq!(high.to_string(), "enum(0:1)");
        assert_eq!(high.eq(&Val::Enum(EnumId::new(0), 1)), Some(true));
        assert_eq!(high.eq(&Val::Enum(EnumId::new(0), 0)), Some(false));
        assert_eq!(high.eq(&Val::Enum(EnumId::new(1), 1)), None);
        assert!(!high.contains_ref());
    }

    #[test]
    fn composite_display() {
        let mut interner = Interner::default();
        let x = interner.intern("x");
        let y = interner.intern("y");
        assert!(Rc::ptr_eq(&x, &interner.intern("x")));
        assert!(!Rc::ptr_eq(&x, &y));
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
