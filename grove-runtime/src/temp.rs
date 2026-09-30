
use grove_query::codegen::render::quote;
use rusqlite::Connection;

use super::error::RuntimeError;

pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "temp_store", "MEMORY")
}

pub fn preamble(conn: &Connection, names: &[String]) -> Result<(), RuntimeError> {
    for name in names {
        conn.execute(&format!("DROP TABLE IF EXISTS {}", quote(name)), [])
            .map_err(|err| RuntimeError::Contract(format!("temp preamble for {name}: {err}")))?;
    }
    Ok(())
}

pub struct TempGuard<'c> {
    conn: &'c Connection,
    names: Vec<String>,
}

impl<'c> TempGuard<'c> {
    pub fn new(conn: &'c Connection, names: Vec<String>) -> Self {
        Self { conn, names }
    }
}

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = self
                .conn
                .execute(&format!("DROP TABLE IF EXISTS {}", quote(name)), []);
        }
    }
}

pub fn readback_sql(name: &str) -> String {
    format!("SELECT * FROM {}", quote(name))
}
