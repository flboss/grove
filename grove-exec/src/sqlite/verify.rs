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

#[cfg(test)]
mod tests {
    use super::*;
    use grove_schema::validated::ValidatedSchema;
    use rusqlite::Connection;

    #[test]
    fn open_in_memory_constructor() {
        assert!(SqliteBackend::open_in_memory(&ValidatedSchema::default()).is_ok());
    }

    fn verify_backend(tables: &[(&str, &[&str])], ddl: &str) -> Result<(), ExecError> {
        use grove_schema::validated::{Column, Table};
        let schema = ValidatedSchema {
            tables: tables
                .iter()
                .map(|(name, cols)| Table {
                    name: name.to_string(),
                    columns: cols
                        .iter()
                        .map(|c| Column {
                            name: c.to_string(),
                            ty: None,
                        })
                        .collect(),
                })
                .collect(),
            ..Default::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        SqliteBackend::new(conn, &schema).map(|_| ())
    }

    #[test]
    fn verify_missing_table() {
        let err = verify_backend(&[("t", &["a"])], "CREATE TABLE other(x);").unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("unknown table `t`"), "{err}");
    }

    #[test]
    fn verify_view_instead_of_table() {
        let err = verify_backend(
            &[("t", &["a"])],
            "CREATE TABLE base(x); CREATE VIEW t AS SELECT x AS a FROM base;",
        )
        .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("is a view"), "{err}");
    }

    #[test]
    fn verify_without_rowid_table() {
        let err = verify_backend(
            &[("t", &["a"])],
            "CREATE TABLE t(a INTEGER PRIMARY KEY) WITHOUT ROWID;",
        )
        .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("WITHOUT ROWID"), "{err}");
    }

    #[test]
    fn verify_missing_column() {
        let err = verify_backend(&[("t", &["a", "b"])], "CREATE TABLE t(a INTEGER);").unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("missing column `b`"), "{err}");
    }

    #[test]
    fn verify_ignores_unknown_and_affinity() {
        assert!(
            verify_backend(
                &[("t", &["a"])],
                "CREATE TABLE t(a TEXT, extra BLOB); CREATE TABLE other(x);"
            )
            .is_ok()
        );
    }

    #[test]
    fn verify_case_insensitive_names() {
        assert!(verify_backend(&[("Users", &["A"])], "CREATE TABLE users(a INTEGER);").is_ok());
    }

    #[test]
    fn verify_rowid_shadow_column() {
        let err = verify_backend(&[("t", &["a"])], "CREATE TABLE t(a INTEGER, _rowid_ TEXT);")
            .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(
            err.to_string().contains("shadowing the rowid alias"),
            "{err}"
        );
    }
}
