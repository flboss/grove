use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use grove_schema::validated::ValidatedSchema;
use rusqlite::Connection;
use rusqlite::config::DbConfig;

use crate::error::ExecError;

mod cache;
mod decode;
mod read;
mod types;
mod verify;
mod vtab_keys;
mod write;

use cache::SqlCacheEntry;
use vtab_keys::KeySupply;

pub struct SqliteBackend<'a> {
    conn: Connection,
    schema: &'a ValidatedSchema,
    sql_cache: RefCell<HashMap<u64, Vec<SqlCacheEntry>>>,
    vtab_keys: Arc<KeySupply>,
}

impl<'a> SqliteBackend<'a> {
    pub fn new(conn: Connection, schema: &'a ValidatedSchema) -> Result<Self, ExecError> {
        for option in [
            // Disable quoted identifiers degrading to string literals in some cases
            DbConfig::SQLITE_DBCONFIG_DQS_DML,
            DbConfig::SQLITE_DBCONFIG_DQS_DDL,
        ] {
            conn.set_db_config(option, false)
                .map_err(|err| Self::storage_error("disable double-quoted literals", err))?;
        }
        conn.set_prepared_statement_cache_capacity(128);
        let backend = SqliteBackend {
            conn,
            schema,
            sql_cache: RefCell::new(HashMap::new()),
            vtab_keys: Arc::new(KeySupply::new()),
        };
        backend.verify_schema()?;
        vtab_keys::register(&backend.conn, Arc::clone(&backend.vtab_keys))
            .map_err(|err| Self::storage_error("register keys table", err))?;
        Ok(backend)
    }

    pub fn open_in_memory(schema: &'a ValidatedSchema) -> Result<Self, ExecError> {
        let conn = Connection::open_in_memory()
            .map_err(|err| Self::storage_error("open in-memory database", err))?;
        Self::new(conn, schema)
    }

    fn storage_error(context: &str, err: rusqlite::Error) -> ExecError {
        ExecError::Storage {
            message: format!("{context}: {err}"),
        }
    }
}

impl Drop for SqliteBackend<'_> {
    /// refresh SQLite planner statistics before closing a writable file db
    fn drop(&mut self) {
        if self.conn.is_readonly("main").unwrap_or(true) {
            return;
        }
        if !matches!(self.conn.path(), Some(path) if !path.is_empty()) {
            return;
        }
        let _ = self.conn.execute_batch("PRAGMA optimize");
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteBackend;
    use crate::storage::{CmpOp, FetchConstraints, PushExpr, Row, SortKey, Storage};
    use crate::testdata;
    use crate::value::{Scalar, Val};
    use grove_schema::validated::{FieldId, RelationId, StructId, ValidatedSchema};
    use rusqlite::Connection;

    pub(super) fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let mut conn = Connection::open_in_memory().unwrap();
        testdata::seed(&mut conn).unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    pub(super) fn user_struct(schema: &ValidatedSchema) -> StructId {
        testdata::struct_id(schema, "User")
    }

    pub(super) fn user_field(schema: &ValidatedSchema, sid: StructId, name: &str) -> FieldId {
        testdata::field_id(schema, sid, name)
    }

    pub(super) fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }

    pub(super) fn int(n: i64) -> Val {
        Val::Scalar(Scalar::Int(n))
    }

    pub(super) fn dec(d: rust_decimal::Decimal) -> Val {
        Val::Scalar(Scalar::Dec(d))
    }

    pub(super) fn float(n: f64) -> Val {
        Val::Scalar(Scalar::Float(n))
    }

    pub(super) fn bool(n: bool) -> Val {
        Val::Scalar(Scalar::Bool(n))
    }

    pub(super) fn str(s: &str) -> Val {
        Val::Scalar(Scalar::String(s.to_string()))
    }

    pub(super) fn bytes(b: impl Into<Vec<u8>>) -> Val {
        Val::Scalar(Scalar::Bytes(b.into()))
    }

    pub(super) fn rel_named(
        schema: &ValidatedSchema,
        a_struct: &str,
        a_field: &str,
        b_struct: &str,
        b_field: &str,
    ) -> RelationId {
        let asid = testdata::struct_id(schema, a_struct);
        let bsid = testdata::struct_id(schema, b_struct);
        testdata::relation_id(
            schema,
            testdata::field_id(schema, asid, a_field),
            testdata::field_id(schema, bsid, b_field),
        )
    }

    pub(super) fn firsts(groups: &[Vec<Row>]) -> Vec<Vec<Val>> {
        groups
            .iter()
            .map(|g| g.iter().map(|row| row.values[0].clone()).collect())
            .collect()
    }

    pub(super) fn push_field(schema: &ValidatedSchema, name: &str) -> PushExpr {
        PushExpr::Field(user_field(schema, user_struct(schema), name))
    }

    pub(super) fn target_field(
        schema: &ValidatedSchema,
        struct_name: &str,
        name: &str,
    ) -> PushExpr {
        PushExpr::Field(testdata::field_id(
            schema,
            testdata::struct_id(schema, struct_name),
            name,
        ))
    }

    pub(super) fn push_int(value: i64) -> PushExpr {
        PushExpr::Literal(Scalar::Int(value))
    }

    pub(super) fn push_str(value: &str) -> PushExpr {
        PushExpr::Literal(Scalar::String(value.to_string()))
    }

    pub(super) fn push_cmp(op: CmpOp, lhs: PushExpr, rhs: PushExpr) -> PushExpr {
        PushExpr::Cmp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    pub(super) fn user_names(
        backend: &SqliteBackend<'_>,
        schema: &ValidatedSchema,
        filters: Vec<PushExpr>,
        sort: Vec<SortKey>,
        offset: u64,
        limit: Option<u64>,
    ) -> Vec<String> {
        user_names_with_tie(backend, schema, filters, sort, false, offset, limit)
    }

    pub(super) fn user_names_with_tie(
        backend: &SqliteBackend<'_>,
        schema: &ValidatedSchema,
        filters: Vec<PushExpr>,
        sort: Vec<SortKey>,
        tie_desc: bool,
        offset: u64,
        limit: Option<u64>,
    ) -> Vec<String> {
        let sid = user_struct(schema);
        let rows = backend
            .fetch(
                sid,
                &[user_field(schema, sid, "name")],
                &FetchConstraints {
                    filters,
                    sort,
                    tie_desc,
                    offset,
                    limit,
                },
            )
            .unwrap();
        rows.into_iter()
            .map(|row| match row.values.as_slice() {
                [Val::Scalar(Scalar::String(name))] => name.clone(),
                other => panic!("expected one name value, got {other:?}"),
            })
            .collect()
    }

    pub(super) fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn quoted_identifiers_no_string_literal_degradation() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).unwrap();
        let err = backend
            .conn
            .prepare("SELECT \"no_such_column_xyz\"")
            .unwrap_err();
        assert!(err.to_string().contains("no such column"), "{err}");
    }
}
