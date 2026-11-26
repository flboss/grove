use std::rc::Rc;

use grove_schema::validated::{FieldId, StructId};

use crate::value::{Key, Val};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{int, rec};
    use crate::testdata;

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
    fn value_equality_unaffected_by_origin() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let orders = testdata::field_id(&schema, user, "orders");
        let fields = vec![(Rc::from("x"), int(1))];
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
