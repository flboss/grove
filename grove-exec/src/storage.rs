use grove_schema::validated::{FieldId, RelationId, StructId};

use crate::error::ExecError;
use crate::value::Val;

/// A decoded storage row
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pk: Option<Key>,
    pub values: Vec<Val>,
}

/// A single-column PK/FK
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub i64);

pub trait Storage {
    /// Read `fields` of every row of the struct's table
    fn scan(&self, struct_id: StructId, fields: &[FieldId]) -> Result<Vec<Row>, ExecError>;

    /// Rows on the other side of `rel`, grouped per requested PK
    fn fetch_related(
        &self,
        rel: RelationId,
        from_first: bool,
        parents: &[Key],
    ) -> Result<Vec<Vec<Row>>, ExecError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Scalar;

    struct Stub {
        rows: Vec<Row>,
    }

    impl Storage for Stub {
        fn scan(&self, _struct_id: StructId, _fields: &[FieldId]) -> Result<Vec<Row>, ExecError> {
            Ok(self.rows.clone())
        }

        fn fetch_related(
            &self,
            _rel: RelationId,
            _from_first: bool,
            parents: &[Key],
        ) -> Result<Vec<Vec<Row>>, ExecError> {
            Ok(parents
                .iter()
                .map(|key| {
                    self.rows
                        .iter()
                        .filter(|row| row.pk == Some(*key))
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
                pk: Some(Key(1)),
                values: vec![some_val(1), Val::None],
            },
            Row {
                pk: Some(Key(2)),
                values: vec![some_val(2), some_val(20)],
            },
            Row {
                pk: None,
                values: vec![some_val(3), Val::None],
            },
        ];
        let backend = Stub { rows };
        let rel = RelationId::new(0);
        let groups = backend.fetch_related(rel, true, &[Key(2), Key(9)]).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[0][0].pk, Some(Key(2)));
        assert_eq!(groups[0][0].values[1], some_val(20));
        assert!(groups[1].is_empty());
    }
}
