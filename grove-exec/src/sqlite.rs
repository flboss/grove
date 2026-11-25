use std::cell::RefCell;
use std::collections::HashMap;

use grove_schema::validated::ValidatedSchema;
use rusqlite::Connection;
use rusqlite::config::DbConfig;

use crate::error::ExecError;

mod cache;
mod decode;
mod read;
mod types;
mod verify;
mod write;

use cache::SqlCacheEntry;

#[cfg(test)]
use decode::decode_node;
#[cfg(test)]
use types::{decode_scalar, scalar_value};

pub struct SqliteBackend<'a> {
    conn: Connection,
    schema: &'a ValidatedSchema,
    sql_cache: RefCell<HashMap<u64, Vec<SqlCacheEntry>>>,
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
        let backend = SqliteBackend {
            conn,
            schema,
            sql_cache: RefCell::new(HashMap::new()),
        };
        backend.verify_schema()?;
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

#[cfg(test)]
mod tests;
