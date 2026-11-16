use std::path::PathBuf;

use clap::{Args, ValueEnum};
use grove_exec::storage::Storage;
use grove_exec::{ExecError, SqliteBackend};
use grove_schema::validated::ValidatedSchema;
use rusqlite::{Connection, OpenFlags};

#[derive(Args, Debug, Clone)]
pub struct BackendArgs {
    /// Storage backend
    #[arg(long, value_enum, default_value = "sqlite")]
    pub backend: BackendKind,

    /// Database file (sqlite backend only)
    #[arg(long, alias = "db", value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Open the database read-only
    #[arg(long)]
    pub readonly: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionSpec {
    Sqlite { file: PathBuf, readonly: bool },
}

impl BackendArgs {
    pub fn resolve(&self) -> Result<ConnectionSpec, String> {
        match self.backend {
            BackendKind::Sqlite => {
                let file = self
                    .file
                    .clone()
                    .ok_or_else(|| "--file is required for --backend sqlite".to_string())?;
                Ok(ConnectionSpec::Sqlite {
                    file,
                    readonly: self.readonly,
                })
            }
        }
    }
}

impl ConnectionSpec {
    pub fn description(&self) -> String {
        match self {
            ConnectionSpec::Sqlite { file, readonly } => {
                let mode = if *readonly { " (read-only)" } else { "" };
                format!("sqlite: {}{mode}", file.display())
            }
        }
    }

    pub fn open<'a>(
        &self,
        schema: &'a ValidatedSchema,
    ) -> Result<Box<dyn Storage + 'a>, ExecError> {
        match self {
            ConnectionSpec::Sqlite { file, readonly } => {
                let result = if *readonly {
                    Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)
                } else {
                    Connection::open(file)
                };
                let conn = result.map_err(|err| ExecError::Storage {
                    message: format!("error opening database {}: {err}", file.display()),
                })?;
                Ok(Box::new(SqliteBackend::new(conn, schema)?))
            }
        }
    }

    pub fn repl_commands(&self) -> &'static [ReplCommandSpec] {
        match self {
            ConnectionSpec::Sqlite { .. } => &[OPEN_FILE_CMD],
        }
    }

    pub fn repl_command(&self, cmd: &str) -> Option<&'static ReplCommandSpec> {
        self.repl_commands()
            .iter()
            .find(|spec_cmd| spec_cmd.name == cmd)
    }

    pub fn apply_repl_action(&self, action: BackendReplAction) -> Result<ConnectionSpec, String> {
        match (self, action) {
            (ConnectionSpec::Sqlite { readonly, .. }, BackendReplAction::OpenFile(file)) => {
                if !file.is_file() {
                    return Err(format!("database file not found: {}", file.display()));
                }
                Ok(ConnectionSpec::Sqlite {
                    file,
                    readonly: *readonly,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendReplAction {
    OpenFile(PathBuf),
}

pub struct ReplCommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub parse: fn(&str) -> Result<BackendReplAction, String>,
}

const OPEN_FILE_CMD: ReplCommandSpec = ReplCommandSpec {
    name: ".open",
    usage: ".open PATH",
    description: "Open an SQLite database file",
    parse: parse_open_file,
};

fn parse_open_file(argument: &str) -> Result<BackendReplAction, String> {
    if argument.is_empty() {
        Err("usage: .open PATH".to_string())
    } else {
        Ok(BackendReplAction::OpenFile(PathBuf::from(argument)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(backend: BackendKind, file: Option<&str>, readonly: bool) -> BackendArgs {
        BackendArgs {
            backend,
            file: file.map(PathBuf::from),
            readonly,
        }
    }

    #[test]
    fn resolve_sqlite_file_required() {
        assert!(args(BackendKind::Sqlite, None, false).resolve().is_err());
        assert_eq!(
            args(BackendKind::Sqlite, Some("data.db"), true)
                .resolve()
                .unwrap(),
            ConnectionSpec::Sqlite {
                file: PathBuf::from("data.db"),
                readonly: true,
            }
        );
    }

    #[test]
    fn backend_specific_repl_commands() {
        assert_eq!(
            ConnectionSpec::Sqlite {
                file: PathBuf::from("data.db"),
                readonly: false,
            }
            .repl_commands()
            .iter()
            .map(|command| command.name)
            .collect::<Vec<_>>(),
            vec![".open"]
        );
    }
}
