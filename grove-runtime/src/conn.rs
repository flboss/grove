
use rusqlite::Connection;

pub struct Runtime {
    conn: Connection,
}

impl Runtime {
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Ok(Self {
            conn: Connection::open_in_memory()?,
        })
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> rusqlite::Result<Self> {
        Ok(Self {
            conn: Connection::open(path)?,
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}
