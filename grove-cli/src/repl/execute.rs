use std::fs;
use std::time::Instant;

use chrono::Utc;
use grove_exec::ExecError;
use grove_exec::value::Val;
use grove_query::ast::QueryFile;

use super::ReplSession;
use crate::diagnostics;
use crate::error::{CliError, CliResult};
use crate::render::{self, RenderOptions};

impl ReplSession<'_> {
    pub(super) fn execute_source(&mut self, source: &str, name: &str) -> CliResult<Option<Val>> {
        let (file, diagnostics) = grove_query::parse_query(source);
        diagnostics::print(source, name, &diagnostics);
        let Some(file) = file else {
            return Ok(None);
        };
        self.execute_file(file, source, name)
    }

    pub(super) fn execute_file(
        &mut self,
        file: QueryFile,
        source: &str,
        name: &str,
    ) -> CliResult<Option<Val>> {
        let (typed, diagnostics) =
            grove_query::typecheck_with_bindings(file, self.schema, &self.types);
        diagnostics::print(source, name, &diagnostics);
        let Some(typed) = typed else {
            return Ok(None);
        };

        let start = Instant::now();
        let result = self.exec.execute(&typed, Utc::now(), self.exec_options);
        let elapsed = start.elapsed();

        match result {
            Ok(mut result) => {
                for (name, ty) in typed.let_bindings() {
                    self.types.define(name, ty.clone());
                }
                if let Some(value) = &mut result {
                    let options = RenderOptions::from_args(
                        &self.settings.output,
                        self.settings.output.output_color_enabled(),
                    );
                    match render::render(value, typed.output_type(), self.schema, &options) {
                        Ok(text) => {
                            if let Err(error) =
                                crate::write_output(self.settings.output.output.as_deref(), &text)
                            {
                                self.report_error(error);
                            }
                        }
                        Err(error) => self.report_error(error),
                    }
                }
                if self.settings.stats {
                    eprintln!("executed in {:.3} ms", crate::millis(elapsed));
                }
                Ok(result)
            }
            Err(ExecError::Abort { message, span }) => {
                let diagnostic = diagnostics::abort(message, span);
                diagnostics::print(source, name, std::slice::from_ref(&diagnostic));
                Ok(None)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(None)
            }
        }
    }

    pub(super) fn read_file(&mut self, path: &str) -> CliResult<()> {
        if path.is_empty() {
            eprintln!("usage: .read PATH");
            return Ok(());
        }
        match fs::read_to_string(path) {
            Ok(source) => {
                let _ = self.execute_source(&source, path)?;
            }
            Err(error) => eprintln!("error reading {path}: {error}"),
        }
        Ok(())
    }

    pub(super) fn report_error(&self, error: CliError) {
        if let Some(message) = error.message {
            eprintln!("error: {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use grove_exec::SqliteBackend;
    use grove_exec::value::{Scalar, Val};
    use grove_schema::ValidatedSchema;

    use crate::repl::tests::*;

    #[test]
    fn bindings_persistence_across_turns() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        assert_eq!(
            session.execute_source("let x = 41;", "<test:1>").unwrap(),
            None
        );
        assert!(session.types.get("x").is_some());
        assert_eq!(
            session.execute_source("x + 1", "<test:2>").unwrap(),
            Some(Val::Scalar(Scalar::Int(42)))
        );
    }

    #[test]
    fn persisted_let_all_fields_kept() {
        let source = "root users: User; struct User { name: String, age: Int }";
        let (schema, diagnostics) = grove_schema::parse_schema(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (schema, diagnostics) = grove_schema::validate(schema.unwrap());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let schema = schema.unwrap();

        let conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(
            "CREATE TABLE users (name TEXT NOT NULL, age INTEGER NOT NULL);\
             INSERT INTO users (name, age) VALUES ('Alice', 30);",
        )
        .expect("seed users");
        let backend = SqliteBackend::new(conn, &schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        assert_eq!(
            session
                .execute_source("let a = users.first();", "<test:1>")
                .unwrap(),
            None
        );
        let value = session
            .execute_source("a", "<test:2>")
            .unwrap()
            .expect("record");
        let Val::Some(inner) = value else {
            panic!("expected some(record), got {value:?}");
        };
        let Val::Record(fields) = inner.as_ref() else {
            panic!("expected record, got {inner:?}");
        };
        assert!(fields.iter().any(|(name, value)| {
            name.as_ref() == "name" && *value == Val::Scalar(Scalar::String("Alice".into()))
        }));
        assert!(fields.iter().any(|(name, value)| {
            name.as_ref() == "age" && *value == Val::Scalar(Scalar::Int(30))
        }));
    }

    #[test]
    fn failed_turn_no_bindings_committed() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        session.execute_source("let x = 1;", "<test:1>").unwrap();
        assert_eq!(
            session
                .execute_source("let y = 2; 1 / 0", "<test:2>")
                .unwrap(),
            None
        );
        assert!(session.types.get("x").is_some());
        assert!(session.types.get("y").is_none());
    }
}
