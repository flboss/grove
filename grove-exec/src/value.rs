use std::borrow::Cow;
use std::collections::HashSet;
use std::rc::Rc;

use grove_schema::validated::{Enum, EnumId, EnumValue, FieldId};
use grove_types::Span;

use crate::error::ExecError;

mod collections;
mod format;
pub(crate) mod literal;
mod optional;
mod scalar;
mod time;

pub use collections::{CollectionOrigin, ListVal, RecordVal, RowOrigin};
pub use format::ValFormatter;
pub use scalar::Scalar;

pub(crate) use optional::{rewrap_optional, unwrap_optional_depth};
pub(crate) use time::{NANOS_PER_DAY, NANOS_PER_HOUR, NANOS_PER_MIN, NANOS_PER_SEC, total_nanos};

/// An opaque unique row identifier (SQLite rowid)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub i64);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{int, some};

    #[test]
    fn enum_equality() {
        let high = Val::Enum(EnumId::new(0), 1);
        assert_eq!(high.eq(&Val::Enum(EnumId::new(0), 1)), Some(true));
        assert_eq!(high.eq(&Val::Enum(EnumId::new(0), 0)), Some(false));
        assert_eq!(high.eq(&Val::Enum(EnumId::new(1), 1)), None);
    }

    #[test]
    fn interner_record_field_names() {
        let mut interner = Interner::default();
        let a = interner.intern("a");
        let b = interner.intern("b");
        assert!(Rc::ptr_eq(&a, &interner.intern("a")));
        assert!(!Rc::ptr_eq(&a, &b));
        assert_eq!(interner.names.len(), 2);
        let record = Val::record(vec![(a, int(2)), (b, some(int(3)))]);
        assert_eq!(record.to_string(), "{a: 2, b: some(3)}");
    }
}
