use rusqlite::types::Value;

use super::SqliteBackend;
use crate::error::ExecError;

impl SqliteBackend<'_> {
    pub(super) fn verify_schema(&self) -> Result<(), ExecError> {
        if rusqlite::version_number() < 3_037_000 {
            return Err(ExecError::Storage {
                message: "SQLite >= 3.37 required for schema verification".to_string(),
            });
        }
        let ctx = "verify database schema";
        let mut tables = self
            .conn
            .prepare("SELECT name, type, wr FROM pragma_table_list WHERE schema = 'main'")
            .map_err(|err| Self::storage_error(ctx, err))?;
        let live: Vec<(String, String, i64)> = tables
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|err| Self::storage_error(ctx, err))?
            .collect::<Result<_, _>>()
            .map_err(|err| Self::storage_error(ctx, err))?;
        for table in &self.schema.tables {
            let found = live
                .iter()
                .find(|(name, _, _)| name.eq_ignore_ascii_case(&table.name));
            let Some((_, kind, without_rowid)) = found else {
                return Err(ExecError::Storage {
                    message: format!("unknown table `{}`", table.name),
                });
            };
            if kind != "table" {
                return Err(ExecError::Storage {
                    message: format!("`{}` is a {kind}, rowid reads need a table", table.name),
                });
            }
            if *without_rowid != 0 {
                return Err(ExecError::Storage {
                    message: format!("table `{}` is WITHOUT ROWID", table.name),
                });
            }
            let mut columns = self
                .conn
                .prepare("SELECT name FROM pragma_table_xinfo(?)")
                .map_err(|err| Self::storage_error(ctx, err))?;
            let live_cols: Vec<String> = columns
                .query_map(
                    rusqlite::params_from_iter(std::iter::once(Value::Text(table.name.clone()))),
                    |row| row.get(0),
                )
                .map_err(|err| Self::storage_error(ctx, err))?
                .collect::<Result<_, _>>()
                .map_err(|err| Self::storage_error(ctx, err))?;
            for column in &table.columns {
                if !live_cols
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&column.name))
                {
                    return Err(ExecError::Storage {
                        message: format!(
                            "table `{}` is missing column `{}`",
                            table.name, column.name
                        ),
                    });
                }
            }
            if live_cols
                .iter()
                .any(|name| name.eq_ignore_ascii_case("_rowid_"))
            {
                return Err(ExecError::Storage {
                    message: format!(
                        "table `{}` has a column named `_rowid_`, shadowing the rowid alias",
                        table.name
                    ),
                });
            }
        }
        Ok(())
    }
}
