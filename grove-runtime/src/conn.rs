
use std::collections::HashSet;

use rusqlite::Connection;

use super::tvf::{self, BulkSlot, LitState};
use super::{RuntimeConfig, aggs, funcs};

pub struct Runtime {
    conn: Connection,
    config: RuntimeConfig,
    pub(crate) bulk: BulkSlot,
    pub(crate) lit: LitState,
    pub(crate) lit_modules: HashSet<String>,
}

impl Runtime {
    fn build(conn: Connection, config: RuntimeConfig) -> rusqlite::Result<Self> {
        let bulk: BulkSlot = Default::default();
        let lit: LitState = Default::default();
        funcs::register_scalars(&conn, &config)?;
        aggs::register_aggregates(&conn, &config)?;
        tvf::register_bulk(&conn, bulk.clone())?;
        super::temp::configure(&conn)?;
        Ok(Self {
            conn,
            config,
            bulk,
            lit,
            lit_modules: HashSet::new(),
        })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::open_in_memory_with_config(RuntimeConfig::default())
    }

    pub fn open_in_memory_with_config(config: RuntimeConfig) -> rusqlite::Result<Self> {
        Self::build(Connection::open_in_memory()?, config)
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> rusqlite::Result<Self> {
        Self::open_with_config(path, RuntimeConfig::default())
    }

    pub fn open_with_config(
        path: impl AsRef<std::path::Path>,
        config: RuntimeConfig,
    ) -> rusqlite::Result<Self> {
        Self::build(Connection::open(path)?, config)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
}
