
use rusqlite::Connection;

use super::{RuntimeConfig, aggs, funcs};

pub struct Runtime {
    conn: Connection,
    config: RuntimeConfig,
}

impl Runtime {
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::open_in_memory_with_config(RuntimeConfig::default())
    }

    pub fn open_in_memory_with_config(config: RuntimeConfig) -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        funcs::register_scalars(&conn, &config)?;
        aggs::register_aggregates(&conn, &config)?;
        Ok(Self { conn, config })
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> rusqlite::Result<Self> {
        Self::open_with_config(path, RuntimeConfig::default())
    }

    pub fn open_with_config(
        path: impl AsRef<std::path::Path>,
        config: RuntimeConfig,
    ) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        funcs::register_scalars(&conn, &config)?;
        aggs::register_aggregates(&conn, &config)?;
        Ok(Self { conn, config })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
}
