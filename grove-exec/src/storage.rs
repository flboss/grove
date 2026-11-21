use grove_schema::validated::{FieldId, RelationId, StructId};

use crate::error::ExecError;
use crate::pushdown::FetchConstraints;
use crate::value::Val;

/// A decoded storage row
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub rowid: Key,
    pub values: Vec<Val>,
}

/// An opaque unique row identifier (SQLite rowid)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub i64);

pub trait Storage {
    /// Read `fields` of every row of the struct's table
    fn fetch(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> Result<Vec<Row>, ExecError>;

    /// `fields` of rows on the other side of `rel`, grouped per requested rowid
    fn fetch_related(
        &self,
        rel: RelationId,
        fetch_from_first: bool,
        parents: &[Key],
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> Result<Vec<Vec<Row>>, ExecError>;

    /// Begin a write transaction (if supported by the backend)
    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>, ExecError> {
        Err(ExecError::Unsupported {
            message: "storage backend does not support mutations".to_string(),
        })
    }
}

/// A transaction-scoped mutation interface
pub trait WriteTxn {
    fn insert_row(
        &mut self,
        struct_id: StructId,
        values: &[(FieldId, Val)],
    ) -> Result<Key, ExecError>;

    fn insert_rows(
        &mut self,
        struct_id: StructId,
        rows: &[Vec<(FieldId, Val)>],
    ) -> Result<Vec<Key>, ExecError> {
        rows.iter()
            .map(|values| self.insert_row(struct_id, values))
            .collect()
    }

    fn insert_row_with_parent(
        &mut self,
        _struct_id: StructId,
        _values: &[(FieldId, Val)],
        _rel: RelationId,
        _parent: Key,
        _parent_is_first: bool,
    ) -> Result<Key, ExecError> {
        Err(ExecError::Unsupported {
            message: "backend does not support inserting through relations".to_string(),
        })
    }

    fn insert_rows_with_parent(
        &mut self,
        struct_id: StructId,
        rows: &[Vec<(FieldId, Val)>],
        rel: RelationId,
        parent: Key,
        parent_is_first: bool,
    ) -> Result<Vec<Key>, ExecError> {
        rows.iter()
            .map(|values| {
                self.insert_row_with_parent(struct_id, values, rel, parent, parent_is_first)
            })
            .collect()
    }

    fn update_row(
        &mut self,
        struct_id: StructId,
        key: Key,
        values: &[(FieldId, Val)],
    ) -> Result<(), ExecError>;

    fn delete_rows(&mut self, struct_id: StructId, keys: &[Key]) -> Result<(), ExecError>;

    fn clear_foreign_keys(
        &mut self,
        _rel: RelationId,
        _referenced: &[Key],
    ) -> Result<(), ExecError> {
        Err(ExecError::Unsupported {
            message: "backend does not support foreign-key cleanup".to_string(),
        })
    }

    fn commit(&mut self) -> Result<(), ExecError>;

    fn rollback(&mut self) -> Result<(), ExecError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Scalar;

    struct Stub {
        rows: Vec<Row>,
    }

    impl Storage for Stub {
        fn fetch(
            &self,
            _struct_id: StructId,
            _fields: &[FieldId],
            _constraints: &FetchConstraints,
        ) -> Result<Vec<Row>, ExecError> {
            Ok(self.rows.clone())
        }

        fn fetch_related(
            &self,
            _rel: RelationId,
            _fetch_from_first: bool,
            parents: &[Key],
            _fields: &[FieldId],
            _constraints: &FetchConstraints,
        ) -> Result<Vec<Vec<Row>>, ExecError> {
            Ok(parents
                .iter()
                .map(|key| {
                    self.rows
                        .iter()
                        .filter(|row| row.rowid == *key)
                        .cloned()
                        .collect()
                })
                .collect())
        }
    }

    fn some_val(n: i64) -> Val {
        Val::Some(Box::new(Val::Scalar(Scalar::Int(n))))
    }

    #[test]
    fn stub_backend_groups_by_parent() {
        let rows = vec![
            Row {
                rowid: Key(1),
                values: vec![some_val(1), Val::None],
            },
            Row {
                rowid: Key(2),
                values: vec![some_val(2), some_val(20)],
            },
            Row {
                rowid: Key(3),
                values: vec![some_val(3), Val::None],
            },
        ];
        let backend = Stub { rows };
        let rel = RelationId::new(0);
        let groups = backend
            .fetch_related(rel, true, &[Key(2), Key(9)], &[], &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[0][0].rowid, Key(2));
        assert_eq!(groups[0][0].values[1], some_val(20));
        assert!(groups[1].is_empty());
    }
}
