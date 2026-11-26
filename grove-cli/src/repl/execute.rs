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
