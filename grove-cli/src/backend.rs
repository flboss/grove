use std::path::PathBuf;

use clap::{Args, ValueEnum};
use grove_exec::storage::Storage;
use grove_exec::{ExecError, SqliteBackend};
use grove_schema::validated::{ScalarType, ValidatedSchema};
use rusqlite::{Connection, OpenFlags};

#[derive(Args, Debug, Clone)]
pub struct BackendArgs {
    /// Storage backend
    #[arg(long, value_enum, default_value = "sqlite")]
    pub backend: BackendKind,

    /// Database file (sqlite backend only)
    #[arg(long, alias = "db", value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// Open the database read-only (sqlite backend only)
    #[arg(long)]
    pub readonly: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Sqlite,
    Memory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionSpec {
    Sqlite { file: PathBuf, readonly: bool },
    Memory,
}

impl BackendArgs {
    pub fn resolve(&self) -> Result<ConnectionSpec, String> {
        match self.backend {
            BackendKind::Sqlite => {
                let file = self
                    .file
                    .clone()
                    .ok_or_else(|| "--file is required for the SQLite backend".to_string())?;
                Ok(ConnectionSpec::Sqlite {
                    file,
                    readonly: self.readonly,
                })
            }
            BackendKind::Memory => {
                if self.file.is_some() {
                    return Err("--file is not valid for the in-memory backend".to_string());
                }
                if self.readonly {
                    return Err("--readonly is not valid for the in-memory backend".to_string());
                }
                Ok(ConnectionSpec::Memory)
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
            ConnectionSpec::Memory => "memory".to_string(),
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
            ConnectionSpec::Memory => {
                let conn = Connection::open_in_memory().map_err(|err| ExecError::Storage {
                    message: format!("error opening in-memory database: {err}"),
                })?;
                create_empty_tables(&conn, schema)?;
                Ok(Box::new(SqliteBackend::new(conn, schema)?))
            }
        }
    }

    pub fn repl_commands(&self) -> &'static [ReplCommandSpec] {
        match self {
            ConnectionSpec::Sqlite { .. } => &[OPEN_FILE_CMD, CLOSE_CMD],
            ConnectionSpec::Memory => &[OPEN_FILE_CMD],
        }
    }

    pub fn repl_command(&self, cmd: &str) -> Option<&'static ReplCommandSpec> {
        self.repl_commands()
            .iter()
            .find(|spec_cmd| spec_cmd.name == cmd)
    }

    pub fn apply_repl_action(
        &self,
        action: BackendReplAction,
    ) -> Result<BackendCommandOutcome, String> {
        match (self, action) {
            (_, BackendReplAction::OpenFile { file, schema }) => {
                if !file.is_file() {
                    return Err(format!("database file not found: {}", file.display()));
                }
                let readonly = match self {
                    ConnectionSpec::Sqlite { readonly, .. } => *readonly,
                    ConnectionSpec::Memory => false,
                };
                Ok(BackendCommandOutcome {
                    spec: ConnectionSpec::Sqlite { file, readonly },
                    schema,
                })
            }
            (ConnectionSpec::Sqlite { .. }, BackendReplAction::Close) => {
                Ok(BackendCommandOutcome {
                    spec: ConnectionSpec::Memory,
                    schema: None,
                })
            }
            _ => Err("command is not supported by the selected backend".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCommandOutcome {
    pub spec: ConnectionSpec,
    pub schema: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendReplAction {
    OpenFile {
        file: PathBuf,
        schema: Option<PathBuf>,
    },
    Close,
}

pub struct ReplCommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub parse: fn(&str) -> Result<BackendReplAction, String>,
}

const OPEN_FILE_CMD: ReplCommandSpec = ReplCommandSpec {
    name: ".open",
    usage: ".open PATH [SCHEMA]",
    description: "Open an SQLite database file",
    parse: parse_open_file,
};

const CLOSE_CMD: ReplCommandSpec = ReplCommandSpec {
    name: ".close",
    usage: ".close",
    description: "Switch to a transient in-memory session",
    parse: parse_close,
};

fn parse_open_file(argument: &str) -> Result<BackendReplAction, String> {
    let mut parts = argument.splitn(2, char::is_whitespace);
    let file = parts.next().unwrap_or("").trim();
    let schema = parts.next().unwrap_or("").trim();
    if file.is_empty() {
        return Err("usage: .open PATH [SCHEMA]".to_string());
    }
    Ok(BackendReplAction::OpenFile {
        file: PathBuf::from(file),
        schema: (!schema.is_empty()).then(|| PathBuf::from(schema)),
    })
}

fn parse_close(argument: &str) -> Result<BackendReplAction, String> {
    if argument.is_empty() {
        Ok(BackendReplAction::Close)
    } else {
        Err("usage: .close".to_string())
    }
}

fn create_empty_tables(conn: &Connection, schema: &ValidatedSchema) -> Result<(), ExecError> {
    let mut sql = String::new();
    for table in &schema.tables {
        let columns = table
            .columns
            .iter()
            .map(|column| format!("{} {}", quote_ident(&column.name), sqlite_type(column.ty)))
            .collect::<Vec<_>>();
        let columns = if columns.is_empty() {
            "\"g$placeholder\" BLOB".to_string()
        } else {
            columns.join(", ")
        };
        sql.push_str(&format!(
            "CREATE TABLE IF NOT EXISTS {} ({columns}); ",
            quote_ident(&table.name)
        ));
    }
    conn.execute_batch(&sql).map_err(|err| ExecError::Storage {
        message: format!("error creating in-memory tables: {err}"),
    })
}

fn sqlite_type(ty: Option<ScalarType>) -> &'static str {
    match ty {
        Some(ScalarType::Int | ScalarType::Bool) | None => "INTEGER",
        Some(ScalarType::Float) => "REAL",
        Some(ScalarType::String) => "TEXT",
        Some(ScalarType::Dec | ScalarType::Bytes | ScalarType::Instant | ScalarType::Duration) => {
            "BLOB"
        }
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> ValidatedSchema {
        let source = "root users: User; struct User { name: String }";
        let (schema, diagnostics) = grove_schema::parse_schema(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (schema, diagnostics) = grove_schema::validate(schema.unwrap());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        schema.unwrap()
    }

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
    fn resolve_memory_file_readonly_rejected() {
        assert_eq!(
            args(BackendKind::Memory, None, false).resolve().unwrap(),
            ConnectionSpec::Memory
        );
        assert!(
            args(BackendKind::Memory, Some("data.db"), false)
                .resolve()
                .is_err()
        );
        assert!(args(BackendKind::Memory, None, true).resolve().is_err());
    }

    #[test]
    fn memory_backend_schema_tables_created() {
        let schema = schema();
        let backend = ConnectionSpec::Memory.open(&schema).unwrap();
        assert!(
            backend
                .fetch(
                    schema.roots[0].struct_id,
                    &[],
                    &grove_exec::pushdown::FetchConstraints::EMPTY,
                )
                .is_ok()
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
            vec![".open", ".close"]
        );
        assert_eq!(
            ConnectionSpec::Memory
                .repl_commands()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            vec![".open"]
        );
    }

    #[test]
    fn open_close_actions() {
        let sqlite = ConnectionSpec::Sqlite {
            file: PathBuf::from("data.db"),
            readonly: false,
        };
        let open = sqlite.repl_command(".open").expect("open command");
        assert_eq!(
            (open.parse)("next.db next.gschema").unwrap(),
            BackendReplAction::OpenFile {
                file: PathBuf::from("next.db"),
                schema: Some(PathBuf::from("next.gschema")),
            }
        );

        let close = sqlite.repl_command(".close").expect("close command");
        assert_eq!(
            sqlite
                .apply_repl_action((close.parse)("").unwrap())
                .unwrap(),
            BackendCommandOutcome {
                spec: ConnectionSpec::Memory,
                schema: None,
            }
        );

        let file = std::env::current_dir().unwrap().join("Cargo.toml");
        let outcome = ConnectionSpec::Memory
            .apply_repl_action(BackendReplAction::OpenFile {
                file,
                schema: Some(PathBuf::from("next.gschema")),
            })
            .unwrap();
        assert!(matches!(outcome.spec, ConnectionSpec::Sqlite { .. }));
        assert_eq!(outcome.schema, Some(PathBuf::from("next.gschema")));
    }
}
