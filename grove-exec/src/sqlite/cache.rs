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
        parents: usize,
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
        parents: usize,
    ) -> bool {
        matches!(
            self,
            SqlCacheKey::Related {
                rel: key_rel,
                fetch_from_first: key_first,
                fields: key_fields,
                constraints: key_constraints,
                parents: key_parents,
            } if *key_rel == rel
                && *key_first == fetch_from_first
                && key_fields.as_slice() == fields
                && key_constraints == constraints
                && *key_parents == parents
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

pub(super) fn fingerprint_fetch_related(
    rel: RelationId,
    fetch_from_first: bool,
    fields: &[FieldId],
    constraints: &FetchConstraints,
    parents: usize,
) -> u64 {
    let mut state = DefaultHasher::new();
    1u8.hash(&mut state);
    rel.hash(&mut state);
    fetch_from_first.hash(&mut state);
    fields.hash(&mut state);
    constraints.hash(&mut state);
    parents.hash(&mut state);
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
