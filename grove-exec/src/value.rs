use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::{self, Write};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use chrono::{DateTime, TimeDelta, Utc};
use grove_schema::validated::{Enum, EnumId, EnumValue, Field, FieldId, StructId, ValidatedSchema};
use grove_types::Span;
use rust_decimal::Decimal;

use crate::error::ExecError;

/// An opaque unique row identifier (SQLite rowid)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub i64);
pub(crate) mod literal;

/// A scalar Grove value
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowOrigin {
    pub struct_id: StructId,
    pub key: Key,
}

/// The storage collection a list was materialized from
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollectionOrigin {
    Root {
        struct_id: StructId,
        root_idx: usize,
    },
    Ref {
        field: FieldId,
        parent: RowOrigin,
    },
}

#[derive(Debug, Clone)]
pub struct RecordVal {
    pub fields: Vec<(Rc<str>, Val)>,
    pub origin: Option<RowOrigin>,
}

impl RecordVal {
    pub fn new(fields: Vec<(Rc<str>, Val)>) -> Self {
        RecordVal {
            fields,
            origin: None,
        }
    }

    pub fn with_origin(fields: Vec<(Rc<str>, Val)>, origin: RowOrigin) -> Self {
        RecordVal {
            fields,
            origin: Some(origin),
        }
    }
}

impl PartialEq for RecordVal {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
    }
}
impl PartialEq<Vec<(Rc<str>, Val)>> for RecordVal {
    fn eq(&self, other: &Vec<(Rc<str>, Val)>) -> bool {
        &self.fields == other
    }
}
impl PartialEq<[(Rc<str>, Val)]> for RecordVal {
    fn eq(&self, other: &[(Rc<str>, Val)]) -> bool {
        self.fields == other
    }
}

impl std::ops::Deref for RecordVal {
    type Target = [(Rc<str>, Val)];

    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}
impl std::ops::DerefMut for RecordVal {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fields
    }
}

impl IntoIterator for RecordVal {
    type Item = (Rc<str>, Val);
    type IntoIter = std::vec::IntoIter<(Rc<str>, Val)>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.into_iter()
    }
}
impl<'a> IntoIterator for &'a RecordVal {
    type Item = &'a (Rc<str>, Val);
    type IntoIter = std::slice::Iter<'a, (Rc<str>, Val)>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.iter()
    }
}
impl<'a> IntoIterator for &'a mut RecordVal {
    type Item = &'a mut (Rc<str>, Val);
    type IntoIter = std::slice::IterMut<'a, (Rc<str>, Val)>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.iter_mut()
    }
}

#[derive(Debug, Clone)]
pub struct ListVal {
    pub items: Vec<Val>,
    pub origin: Option<CollectionOrigin>,
}

impl ListVal {
    pub fn new(items: Vec<Val>) -> Self {
        ListVal {
            items,
            origin: None,
        }
    }

    pub fn with_origin(items: Vec<Val>, origin: CollectionOrigin) -> Self {
        ListVal {
            items,
            origin: Some(origin),
        }
    }

    pub fn push(&mut self, value: Val) {
        self.items.push(value);
    }

    pub fn pop(&mut self) -> Option<Val> {
        self.items.pop()
    }

    pub fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    pub fn drain<R>(&mut self, range: R) -> std::vec::Drain<'_, Val>
    where
        R: std::ops::RangeBounds<usize>,
    {
        self.items.drain(range)
    }

    pub fn swap_remove(&mut self, index: usize) -> Val {
        self.items.swap_remove(index)
    }

    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = Val>,
    {
        self.items.extend(iter);
    }
}

impl PartialEq for ListVal {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}
impl PartialEq<Vec<Val>> for ListVal {
    fn eq(&self, other: &Vec<Val>) -> bool {
        &self.items == other
    }
}
impl PartialEq<[Val]> for ListVal {
    fn eq(&self, other: &[Val]) -> bool {
        self.items == other
    }
}
impl<const N: usize> PartialEq<[Val; N]> for ListVal {
    fn eq(&self, other: &[Val; N]) -> bool {
        self.items == other
    }
}

impl std::ops::Deref for ListVal {
    type Target = [Val];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}
impl std::ops::DerefMut for ListVal {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.items
    }
}

impl IntoIterator for ListVal {
    type Item = Val;
    type IntoIter = std::vec::IntoIter<Val>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}
impl<'a> IntoIterator for &'a ListVal {
    type Item = &'a Val;
    type IntoIter = std::slice::Iter<'a, Val>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}
impl<'a> IntoIterator for &'a mut ListVal {
    type Item = &'a mut Val;
    type IntoIter = std::slice::IterMut<'a, Val>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter_mut()
    }
}

/// A Grove value
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    None,
    Some(Box<Val>),
    Scalar(Scalar),
    Enum(EnumId, usize),
    List(ListVal),
    Tuple(Vec<Val>),
    Record(RecordVal),
    /// Reference to an existing in-storage row
    RowRef(RowOrigin),
    /// Unresolved navigation link of the `parent` row
    Ref {
        field: FieldId,
        parent: Key,
    },
}

impl Val {
    pub fn record(fields: Vec<(Rc<str>, Val)>) -> Self {
        Val::Record(RecordVal::new(fields))
    }

    pub fn record_with_origin(fields: Vec<(Rc<str>, Val)>, origin: Option<RowOrigin>) -> Self {
        Val::Record(RecordVal { fields, origin })
    }

    pub fn record_at(fields: Vec<(Rc<str>, Val)>, origin: RowOrigin) -> Self {
        Val::Record(RecordVal::with_origin(fields, origin))
    }

    pub fn list(items: Vec<Val>) -> Self {
        Val::List(ListVal::new(items))
    }

    pub fn list_with_origin(items: Vec<Val>, origin: Option<CollectionOrigin>) -> Self {
        Val::List(ListVal { items, origin })
    }

    pub fn list_at(items: Vec<Val>, origin: CollectionOrigin) -> Self {
        Val::List(ListVal::with_origin(items, origin))
    }

    pub fn row_origin(&self) -> Option<&RowOrigin> {
        match self {
            Val::Record(record) => record.origin.as_ref(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn row_ref_origin(&self) -> Option<&RowOrigin> {
        match self {
            Val::RowRef(origin) => Some(origin),
            _ => None,
        }
    }

    pub fn collection_origin(&self) -> Option<&CollectionOrigin> {
        match self {
            Val::List(list) => list.origin.as_ref(),
            _ => None,
        }
    }

    pub fn cmp(&self, other: &Val, span: Span) -> Result<std::cmp::Ordering, ExecError> {
        match (self, other) {
            (Val::Scalar(x), Val::Scalar(y)) => x.cmp(y, span),
            (Val::Tuple(xs), Val::Tuple(ys)) => {
                if xs.len() != ys.len() {
                    return Err(ExecError::Internal {
                        message: "mismatched tuple comparison".to_string(),
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
            _ => Err(ExecError::Internal {
                message: "mismatched comparison operands".to_string(),
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
            (Val::Ref { .. }, Val::Ref { .. }) => Some(true),
            (Val::RowRef(left), Val::RowRef(right)) => Some(left == right),
            _ => None,
        }
    }

    pub fn contains_ref(&self) -> bool {
        match self {
            Val::Some(val) => val.contains_ref(),
            Val::List(vals) => vals.iter().any(Val::contains_ref),
            Val::Tuple(vals) => vals.iter().any(Val::contains_ref),
            Val::Record(items) => items.iter().any(|(_, val)| val.contains_ref()),
            Val::Ref { .. } | Val::RowRef(_) => true,
            _ => false,
        }
    }
}

pub(crate) fn rewrap_optional(depth: usize, mut value: Val) -> Val {
    for _ in 0..depth {
        value = Val::Some(Box::new(value));
    }
    value
}

pub(crate) fn unwrap_optional_depth(
    mut value: Val,
    depth: usize,
) -> Result<(Val, usize), ExecError> {
    let mut peeled = 0;
    while peeled < depth {
        match value {
            Val::Some(inner) => {
                value = *inner;
                peeled += 1;
            }
            Val::None => return Ok((Val::None, peeled)),
            other => {
                return Err(ExecError::Internal {
                    message: format!("optional access expected {depth} layer(s), found {other}"),
                });
            }
        }
    }
    Ok((value, peeled))
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

/// Field-name interner owned by the executor
#[derive(Debug, Default)]
pub(crate) struct Interner {
    names: HashSet<Rc<str>>,
}

impl Interner {
    pub(crate) fn intern<'a>(&mut self, name: impl Into<Cow<'a, str>>) -> Rc<str> {
        let name = name.into();
        if let Some(existing) = self.names.get(name.as_ref()) {
            return Rc::clone(existing);
        }
        let arc: Rc<str> = Rc::from(name.into_owned());
        self.names.insert(Rc::clone(&arc));
        arc
    }
}

/// Total nanoseconds of a Duration
pub(crate) fn total_nanos(td: &TimeDelta) -> i128 {
    td.num_seconds() as i128 * NANOS_PER_SEC as i128 + td.subsec_nanos() as i128
}

pub(crate) const NANOS_PER_SEC: u128 = 1_000_000_000;
pub(crate) const NANOS_PER_MIN: u128 = 60 * NANOS_PER_SEC;
pub(crate) const NANOS_PER_HOUR: u128 = 60 * NANOS_PER_MIN;
pub(crate) const NANOS_PER_DAY: u128 = 24 * NANOS_PER_HOUR;

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
            Scalar::Float(x) => write!(f, "{x:?}f"),
            Scalar::Dec(d) => write!(f, "{d}{}", if d.scale() == 0 { ".0" } else { "" }),
            Scalar::String(s) => write!(f, "{s:?}"),
            Scalar::Bytes(bytes) => {
                write!(f, "hex\"")?;
                for byte in bytes {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, "\"")
            }
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
            Val::RowRef(origin) => {
                write!(f, "&Struct#{}@{}", origin.struct_id.index(), origin.key.0)
            }
        }
    }
}

impl Val {
    pub fn formatter<'v, 's>(&'v self, schema: &'s ValidatedSchema) -> ValFormatter<'v, 's> {
        ValFormatter::new(self, schema)
    }

    fn fmt_with(&self, schema: &ValidatedSchema, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::None => write!(f, "none"),
            Val::Some(inner) => write!(f, "some({})", inner.formatter(schema)),
            Val::Scalar(s) => write!(f, "{s}"),
            Val::Enum(id, variant) => {
                if let Some(enum_def) = schema.enums.get(id.index())
                    && let Some(variant_def) = enum_def.variants.get(*variant)
                {
                    return write!(f, "{}::{}", enum_def.name, variant_def.name);
                }
                write!(f, "enum({}:{})", id.index(), variant)
            }
            Val::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item.formatter(schema))?;
                }
                write!(f, "]")
            }
            Val::Tuple(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item.formatter(schema))?;
                }
                write!(f, ")")
            }
            Val::Record(fields) => {
                write!(f, "{{")?;
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {}", value.formatter(schema))?;
                }
                write!(f, "}}")
            }
            Val::Ref { field, parent } => {
                let Some(Field::Ref {
                    target,
                    optional,
                    is_list,
                    owning,
                    ..
                }) = schema.field(*field)
                else {
                    return write!(f, "ref({}:{})", field.struct_id().index(), field.local());
                };
                let Some(target) = schema.structs.get(target.index()) else {
                    return write!(f, "ref({}:{})", field.struct_id().index(), field.local());
                };
                if !owning {
                    write!(f, "&")?;
                }
                match (is_list, optional) {
                    (true, _) => write!(f, "List<{}>", target.name)?,
                    (false, true) => write!(f, "?{}", target.name)?,
                    (false, false) => write!(f, "{}", target.name)?,
                }
                write!(f, "#{}", parent.0)
            }
            Val::RowRef(origin) => {
                let Some(struct_) = schema.structs.get(origin.struct_id.index()) else {
                    return write!(f, "&Struct#{}@{}", origin.struct_id.index(), origin.key.0);
                };
                write!(f, "&{}#{}", struct_.name, origin.key.0)
            }
        }
    }
}

pub struct ValFormatter<'v, 's> {
    value: &'v Val,
    schema: &'s ValidatedSchema,
}

impl<'v, 's> ValFormatter<'v, 's> {
    pub(crate) fn new(value: &'v Val, schema: &'s ValidatedSchema) -> Self {
        ValFormatter { value, schema }
    }
}

impl<'v, 's> fmt::Display for ValFormatter<'v, 's> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt_with(self.schema, f)
    }
}

#[cfg(test)]
mod tests {
    use std::{cmp::Ordering, hash::DefaultHasher};

    use crate::eval::harness::{int, rec};
    use crate::testdata;

    use super::*;
    use grove_schema::validated::StructId;
    use rust_decimal::dec;

    fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }

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
    fn optional_depth_helpers() {
        let value = some(some(int(1)));
        assert_eq!(unwrap_optional_depth(value, 2).unwrap(), (int(1), 2));
        assert_eq!(
            unwrap_optional_depth(some(Val::None), 2).unwrap(),
            (Val::None, 1)
        );
        assert_eq!(rewrap_optional(2, int(1)), some(some(int(1))));
        assert_eq!(rewrap_optional(1, Val::None), some(Val::None));
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
    fn contextual_enum_display() {
        let schema = testdata::schema();
        let priority = testdata::enum_id(&schema, "Priority");
        let value = Val::Enum(priority, 1);
        assert_eq!(value.to_string(), "enum(0:1)");
        assert_eq!(value.formatter(&schema).to_string(), "Priority::Medium");

        assert_eq!(
            Val::Enum(priority, 99).formatter(&schema).to_string(),
            "enum(0:99)"
        );
        assert_eq!(
            Val::Enum(EnumId::new(99), 0).formatter(&schema).to_string(),
            "enum(99:0)"
        );
    }

    #[test]
    fn contextual_ref_display() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let profile = testdata::struct_id(&schema, "Profile");
        let key = Key(123);
        let link = |struct_id: StructId, name: &str| Val::Ref {
            field: testdata::field_id(&schema, struct_id, name),
            parent: key,
        };

        assert_eq!(
            link(user, "orders").formatter(&schema).to_string(),
            "List<Order>#123"
        );
        assert_eq!(
            link(user, "profile").formatter(&schema).to_string(),
            "?Profile#123"
        );
        assert_eq!(
            link(user, "roles").formatter(&schema).to_string(),
            "&List<Role>#123"
        );
        assert_eq!(
            link(user, "manager").formatter(&schema).to_string(),
            "&?User#123"
        );
        assert_eq!(
            link(profile, "user").formatter(&schema).to_string(),
            "&User#123"
        );
    }

    #[test]
    fn contextual_ref_display_raw_fallback() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let field = testdata::field_id(&schema, user, "id");
        let value = Val::Ref {
            field,
            parent: Key(1),
        };
        assert_eq!(
            value.formatter(&schema).to_string(),
            format!("ref({}:{})", field.struct_id().index(), field.local())
        );

        let value = Val::Ref {
            field: FieldId::new(StructId::new(99), 0),
            parent: Key(1),
        };
        assert_eq!(value.formatter(&schema).to_string(), "ref(99:0)");
    }

    #[test]
    fn contextual_composite_display() {
        let schema = testdata::schema();
        let priority = testdata::enum_id(&schema, "Priority");
        let user = testdata::struct_id(&schema, "User");
        let value = Val::record(vec![
            ("p".into(), Val::Some(Box::new(Val::Enum(priority, 2)))),
            (
                "m".into(),
                Val::list(vec![Val::Ref {
                    field: crate::testdata::field_id(&schema, user, "manager"),
                    parent: Key(7),
                }]),
            ),
            ("t".into(), Val::Tuple(vec![Val::None, int(3)])),
        ]);
        assert_eq!(
            value.formatter(&schema).to_string(),
            "{p: some(Priority::High), m: [&?User#7], t: (none, 3)}"
        );
    }

    #[test]
    fn composite_display() {
        let mut interner = Interner::default();
        let x = interner.intern("x");
        let y = interner.intern("y");
        assert!(Rc::ptr_eq(&x, &interner.intern("x")));
        assert!(!Rc::ptr_eq(&x, &y));
        assert_eq!(interner.names.len(), 2);
        let list = Val::list(vec![Val::Scalar(Scalar::Int(1)), Val::None]);
        assert_eq!(list.to_string(), "[1, none]");
        let tuple = Val::Tuple(vec![
            Val::Scalar(Scalar::String("a".to_string())),
            Val::Scalar(Scalar::Int(1)),
        ]);
        assert_eq!(tuple.to_string(), "(\"a\", 1)");
        let record = Val::record(vec![(x, Val::Scalar(Scalar::Int(1))), (y, list)]);
        assert_eq!(record.to_string(), "{x: 1, y: [1, none]}");
        assert_eq!(Val::list(vec![]).to_string(), "[]");
    }

    #[test]
    fn value_equality_unaffected_by_origin() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let orders = testdata::field_id(&schema, user, "orders");
        let fields = vec![(Rc::from("x"), Val::Scalar(Scalar::Int(1)))];
        let origin = RowOrigin {
            struct_id: user,
            key: Key(7),
        };
        assert_eq!(Val::record_at(fields.clone(), origin), Val::record(fields));
        assert_eq!(
            Val::list_at(
                vec![int(1)],
                CollectionOrigin::Ref {
                    field: orders,
                    parent: origin,
                },
            ),
            Val::list(vec![int(1)])
        );
    }

    #[test]
    fn row_ref_value() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let origin = RowOrigin {
            struct_id: user,
            key: Key(7),
        };
        let other = RowOrigin {
            struct_id: user,
            key: Key(8),
        };
        let value = Val::RowRef(origin);

        assert_eq!(value.to_string(), format!("&Struct#{}@7", user.index()));
        assert_eq!(value.formatter(&schema).to_string(), "&User#7");
        assert_eq!(value.row_ref_origin(), Some(&origin));
        assert_eq!(value.row_origin(), None);
        assert_eq!(value.eq(&Val::RowRef(origin)), Some(true));
        assert_eq!(value.eq(&Val::RowRef(other)), Some(false));
        assert!(value.contains_ref());
    }
}
