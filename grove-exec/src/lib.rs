pub mod codec;
pub mod error;
pub mod eval;
pub(crate) mod method_meta;
pub mod pushdown;
pub mod sqlite;
pub mod storage;
pub mod value;

pub use error::ExecError;
pub use eval::{ExecOptions, ExecSession, exec_file, exec_file_with_options};
pub use sqlite::SqliteBackend;

#[cfg(test)]
mod testdata;
