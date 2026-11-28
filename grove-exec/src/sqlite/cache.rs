use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use grove_schema::validated::{FieldId, RelationId, StructId};
use rusqlite::types::Value;

use super::SqliteBackend;
use crate::error::ExecError;
use crate::storage::FetchConstraints;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum SqlCacheKey {
    Fetch {
        struct_id: StructId,
        fields: Vec<FieldId>,
        constraints: FetchConstraints,
    },
    Related {
        rel: RelationId,
        fetch_from_first: bool,
        fields: Vec<FieldId>,
        constraints: FetchConstraints,
        key_bucket: u8,
    },
}

impl SqlCacheKey {
    pub(super) fn matches_fetch(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> bool {
        matches!(
            self,
            SqlCacheKey::Fetch {
                struct_id: key_struct,
                fields: key_fields,
                constraints: key_constraints,
            } if *key_struct == struct_id
                && key_fields.as_slice() == fields
                && key_constraints == constraints
        )
    }

    pub(super) fn matches_related(
        &self,
        rel: RelationId,
        fetch_from_first: bool,
        fields: &[FieldId],
        constraints: &FetchConstraints,
        key_bucket: u8,
    ) -> bool {
        matches!(
            self,
            SqlCacheKey::Related {
                rel: key_rel,
                fetch_from_first: key_first,
                fields: key_fields,
                constraints: key_constraints,
                key_bucket: cached_bucket,
            } if *key_rel == rel
                && *key_first == fetch_from_first
                && key_fields.as_slice() == fields
                && key_constraints == constraints
                && *cached_bucket == key_bucket
        )
    }
}

#[derive(Debug)]
pub(super) struct SqlCacheEntry {
    key: SqlCacheKey,
    cached: Rc<CachedSql>,
}

#[derive(Debug)]
pub(super) struct CachedSql {
    pub(super) sql: String,
    pub(super) params: Vec<Value>,
}

pub(super) fn fingerprint_fetch(
    struct_id: StructId,
    fields: &[FieldId],
    constraints: &FetchConstraints,
) -> u64 {
    let mut state = DefaultHasher::new();
    0u8.hash(&mut state);
    struct_id.hash(&mut state);
    fields.hash(&mut state);
    constraints.hash(&mut state);
    state.finish()
}

/// key count buckets in powers of four
pub(super) fn key_count_bucket(count: usize) -> u8 {
    (count.max(1).ilog2() / 2) as u8
}

pub(super) fn fingerprint_fetch_related(
    rel: RelationId,
    fetch_from_first: bool,
    fields: &[FieldId],
    constraints: &FetchConstraints,
    key_bucket: u8,
) -> u64 {
    let mut state = DefaultHasher::new();
    1u8.hash(&mut state);
    rel.hash(&mut state);
    fetch_from_first.hash(&mut state);
    fields.hash(&mut state);
    constraints.hash(&mut state);
    key_bucket.hash(&mut state);
    state.finish()
}

impl SqliteBackend<'_> {
    pub(super) fn cached_sql(
        &self,
        fingerprint: u64,
        matches: impl Fn(&SqlCacheKey) -> bool,
        build_key: impl FnOnce() -> SqlCacheKey,
        build: impl FnOnce() -> Result<(String, Vec<Value>), ExecError>,
    ) -> Result<Rc<CachedSql>, ExecError> {
        if let Some(cached) = self
            .sql_cache
            .borrow()
            .get(&fingerprint)
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| matches(&entry.key))
                    .map(|entry| Rc::clone(&entry.cached))
            })
        {
            return Ok(cached);
        }
        let (sql, params) = build()?;
        let cached = Rc::new(CachedSql { sql, params });
        self.sql_cache
            .borrow_mut()
            .entry(fingerprint)
            .or_default()
            .push(SqlCacheEntry {
                key: build_key(),
                cached: Rc::clone(&cached),
            });
        Ok(cached)
    }
}

#[cfg(test)]
mod tests {
    use crate::sqlite::tests::*;
    use crate::storage::{CmpOp, FetchConstraints, PushExpr, SortKey, Storage};
    use crate::testdata;
    use crate::value::{Key, Scalar, Val};

    #[test]
    fn sql_cache_fetch_statements() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let fields = vec![user_field(&schema, sid, "name")];
        let constraints = FetchConstraints {
            filters: vec![push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(2))],
            sort: vec![],
            tie_desc: false,
            offset: 0,
            limit: None,
        };

        backend.fetch(sid, &fields, &constraints).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend.fetch(sid, &fields, &constraints).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let capped = FetchConstraints {
            offset: 1,
            ..constraints.clone()
        };
        backend.fetch(sid, &fields, &capped).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 2);

        backend
            .fetch(sid, &[user_field(&schema, sid, "id")], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 3);
    }

    #[test]
    fn fetch_enum_filter_and_cache() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = testdata::struct_id(&schema, "Setting");
        let name = testdata::field_id(&schema, sid, "name");
        let priority = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Eq,
                target_field(&schema, "Setting", "priority"),
                push_str("high"),
            )],
            sort: vec![],
            tie_desc: false,
            offset: 0,
            limit: None,
        };
        let rows = backend.fetch(sid, &[name], &priority).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values[0].clone())
                .collect::<Vec<_>>(),
            vec![str("alpha")]
        );
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend.fetch(sid, &[name], &priority).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let level = FetchConstraints {
            filters: vec![PushExpr::In {
                lhs: Box::new(target_field(&schema, "Setting", "level")),
                items: vec![push_int(2), push_int(-1)],
            }],
            sort: vec![],
            tie_desc: false,
            offset: 0,
            limit: None,
        };
        let rows = backend.fetch(sid, &[name], &level).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values[0].clone())
                .collect::<Vec<_>>(),
            vec![str("alpha"), str("beta")]
        );
        assert_eq!(backend.sql_cache.borrow().len(), 2);
    }

    #[test]
    fn sql_cache_fetch_related_statements() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Gt,
                target_field(&schema, "Order", "amount"),
                PushExpr::Literal(Scalar::Int(150)),
            )],
            sort: vec![SortKey {
                field: amount,
                desc: true,
            }],
            tie_desc: false,
            offset: 0,
            limit: Some(1),
        };

        backend
            .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend
            .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let uncapped = FetchConstraints {
            limit: None,
            ..constraints.clone()
        };
        backend
            .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &uncapped)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 2);

        backend
            .fetch_related(rel, false, &[Key(1)], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 2);

        backend
            .fetch_related(rel, false, &[Key(1); 16], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 3);

        backend
            .fetch_related(rel, false, &[Key(1); 17], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 3);
    }

    #[test]
    fn fetch_bytes_filter_sort_and_cache() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = testdata::struct_id(&schema, "Blob");
        let id = testdata::field_id(&schema, sid, "id");
        let data = testdata::field_id(&schema, sid, "data");
        let maybe = testdata::field_id(&schema, sid, "maybe");
        let chunks = testdata::field_id(&schema, sid, "chunks");

        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Gt,
                target_field(&schema, "Blob", "data"),
                PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x00])),
            )],
            sort: vec![SortKey {
                field: data,
                desc: false,
            }],
            tie_desc: false,
            offset: 0,
            limit: None,
        };
        let rows = backend.fetch(sid, &[id], &constraints).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values[0].clone())
                .collect::<Vec<_>>(),
            vec![int(1), int(3)]
        );
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend.fetch(sid, &[id], &constraints).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let rows = backend
            .fetch(sid, &[maybe, chunks], &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(
            rows[0].values,
            vec![
                some(Val::Scalar(Scalar::Bytes(vec![0xff]))),
                Val::list(vec![
                    Val::Scalar(Scalar::Bytes(vec![0x01])),
                    Val::Scalar(Scalar::Bytes(vec![0x01, 0x00])),
                ]),
            ]
        );
        assert_eq!(rows[1].values, vec![Val::None, Val::list(vec![])]);
        assert_eq!(
            rows[2].values,
            vec![
                some(Val::Scalar(Scalar::Bytes(vec![0x00]))),
                Val::list(vec![]),
            ]
        );
    }
}
