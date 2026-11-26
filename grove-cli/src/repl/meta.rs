use std::path::{Path, PathBuf};

use grove_schema::ValidatedSchema;

use super::{ReplOutcome, ReplSession};
use crate::args::Format;
use crate::error::CliResult;

impl ReplSession<'_> {
    pub(super) fn meta_command(&mut self, line: &str) -> CliResult<Option<ReplOutcome>> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let argument = parts.next().unwrap_or("").trim();

        if let Some(spec_command) = self.spec.repl_command(command) {
            let action = match (spec_command.parse)(argument) {
                Ok(action) => action,
                Err(message) => {
                    eprintln!("{message}");
                    return Ok(None);
                }
            };
            let change = match self.spec.apply_repl_action(action) {
                Ok(change) => change,
                Err(message) => {
                    eprintln!("{message}");
                    return Ok(None);
                }
            };
            let new_schema = if let Some(path) = &change.schema {
                match self.load_schema(path) {
                    Some(schema) => Some((path.clone(), Box::new(schema))),
                    None => return Ok(None),
                }
            } else {
                None
            };
            return Ok(Some(ReplOutcome::ChangeBackend {
                spec: change.spec,
                schema: new_schema,
            }));
        }

        match command {
            ".quit" | ".exit" => return Ok(Some(ReplOutcome::Quit)),
            ".help" => self.print_help(),
            ".vars" | ".lets" => self.print_vars(),
            ".unlet" => self.unlet(argument),
            ".clear" => {
                self.types.clear();
                self.exec.clear();
                println!("Bindings cleared.");
            }
            ".read" => self.read_file(argument)?,
            ".schema" => {
                let argument = argument.trim();
                if argument.is_empty() {
                    self.print_schema();
                    return Ok(None);
                }
                let path = (argument != "reload").then(|| PathBuf::from(argument));
                return self.reload_schema(path);
            }
            ".format" => {
                if let Some(format) = Format::parse(argument) {
                    self.settings.output.format = format;
                    println!("format mode: {}", format.name());
                } else {
                    eprintln!("usage: .format auto|inline|raw|table|vertical");
                }
            }
            ".headers" => match argument {
                "on" => {
                    self.settings.output.no_headers = false;
                    println!("headers: on");
                }
                "off" => {
                    self.settings.output.no_headers = true;
                    println!("headers: off");
                }
                _ => eprintln!("usage: .headers on|off"),
            },
            ".stats" => match argument {
                "on" => {
                    self.settings.stats = true;
                    println!("stats: on");
                }
                "off" => {
                    self.settings.stats = false;
                    println!("stats: off");
                }
                _ => eprintln!("usage: .stats on|off"),
            },
            ".hiderefs" => match argument {
                "on" => {
                    self.settings.output.hide_refs = true;
                    println!("hide refs: on");
                }
                "off" => {
                    self.settings.output.hide_refs = false;
                    println!("hide refs: off");
                }
                _ => eprintln!("usage: .hiderefs on|off"),
            },
            ".width" => {
                if argument == "auto" {
                    self.settings.output.width = None;
                    println!("width: auto");
                } else if let Ok(width) = argument.parse::<usize>() {
                    self.settings.output.width = Some(width);
                    println!("width: {width}");
                } else {
                    eprintln!("usage: .width N|auto");
                }
            }
            ".maxrows" => {
                if matches!(argument, "unlimited" | "none" | "inf") {
                    self.settings.output.max_rows = 0;
                    println!("maxrows: unlimited");
                } else if let Ok(max_rows) = argument.parse::<usize>() {
                    self.settings.output.max_rows = max_rows;
                    println!("maxrows: {max_rows}");
                } else {
                    eprintln!("usage: .maxrows N|unlimited");
                }
            }
            ".output" => {
                if argument.is_empty() || argument == "stdout" {
                    self.settings.output.output = None;
                    println!("output: stdout");
                } else {
                    self.settings.output.output = Some(PathBuf::from(argument));
                    println!("output: {argument}");
                }
            }
            _ => {
                eprintln!(
                    "unknown REPL command: \"{command}\". Type \".help\" for a list of commands."
                )
            }
        }

        Ok(None)
    }

    fn reload_schema(&mut self, path: Option<PathBuf>) -> CliResult<Option<ReplOutcome>> {
        let path = path.unwrap_or_else(|| self.schema_path.clone());
        let (name, source) = match crate::read_schema(&path) {
            Ok(source) => source,
            Err(error) => {
                self.report_error(error);
                return Ok(None);
            }
        };
        let Some(schema) = crate::compile_schema(&name, &source) else {
            return Ok(None);
        };
        Ok(Some(ReplOutcome::ReloadSchema {
            schema: Box::new(schema),
            path,
        }))
    }

    fn load_schema(&self, path: &Path) -> Option<ValidatedSchema> {
        let (name, source) = match crate::read_schema(path) {
            Ok(source) => source,
            Err(error) => {
                self.report_error(error);
                return None;
            }
        };
        crate::compile_schema(&name, &source)
    }

    // TODO: option to print a more complete representation of the schema
    fn print_schema(&self) {
        println!("schema: {}", self.schema_path.display());
        for root in &self.schema.roots {
            if let Some(struct_) = self.schema.structs.get(root.struct_id.index()) {
                println!("  root {}: List<{}>", root.name, struct_.name);
            }
        }
        if !self.schema.enums.is_empty() {
            let enums = self
                .schema
                .enums
                .iter()
                .map(|enum_| enum_.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            println!("  enums: {enums}");
        }
    }

    fn print_vars(&self) {
        if self.types.is_empty() {
            println!("No bindings.");
            return;
        }

        for (name, ty) in self.types.iter() {
            let value = self
                .exec
                .binding(name)
                .map(|value| value.formatter(self.schema).to_string())
                .unwrap_or_else(|| "<missing>".to_string());
            println!("{name}: {ty} = {value}");
        }
    }

    fn unlet(&mut self, name: &str) {
        if name.is_empty() {
            eprintln!("usage: .unlet NAME");
            return;
        }
        if self.types.remove(name).is_some() {
            self.exec.remove(name);
            println!("Removed `{name}`.");
        } else {
            eprintln!("no binding named `{name}`");
        }
    }

    fn print_help(&self) {
        println!(
            "\
.help                          Show this command list
.quit, .exit                   Quit the REPL
.vars, .lets                   List let bindings
.unlet NAME                    Remove a binding
.clear                         Clear all bindings
.read PATH                     Run a query file in this session
.schema [reload|PATH]          Show schema summary or load from file
.format MODE                   auto|inline|raw|table|vertical
.headers on|off                Toggle table headers
.stats on|off                  Toggle per-turn query stats
.hiderefs on|off               Hide unresolved non-owning references
.width N|auto                  Set render width
.maxrows N|unlimited           Set the table render row limit
.output PATH|stdout            Redirect results"
        );

        let backend_commands = self.spec.repl_commands();
        if !backend_commands.is_empty() {
            println!();
            println!("Backend commands:");
            for command in backend_commands {
                println!("{:<30} {}", command.usage, command.description);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use grove_exec::SqliteBackend;
    use grove_schema::ValidatedSchema;

    use crate::args::{ConnectionSpec, Format};
    use crate::repl::ReplOutcome;
    use crate::repl::tests::*;

    #[test]
    fn meta_commands_render_settings() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        session.meta_command(".format vertical").unwrap();
        session.meta_command(".headers off").unwrap();
        session.meta_command(".width 42").unwrap();
        session.meta_command(".maxrows 5").unwrap();
        session.meta_command(".hiderefs on").unwrap();

        assert_eq!(session.settings.output.format, Format::Vertical);
        assert!(session.settings.output.no_headers);
        assert_eq!(session.settings.output.width, Some(42));
        assert_eq!(session.settings.output.max_rows, 5);
        assert!(session.settings.output.hide_refs);
    }

    #[test]
    fn schema_command_load_new_path() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        let path =
            std::env::temp_dir().join(format!("grove-repl-schema-{}.gschema", std::process::id()));
        fs::write(&path, "root items: Item; struct Item { id: Int }").expect("write schema");
        let outcome = session
            .meta_command(&format!(".schema {}", path.display()))
            .unwrap()
            .expect("reload outcome");
        let _ = fs::remove_file(&path);

        match outcome {
            ReplOutcome::ReloadSchema {
                path: actual_path, ..
            } => assert_eq!(actual_path, path),
            other => panic!("expected schema reload, got {other:?}"),
        }
    }

    #[test]
    fn open_command_db_schema() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = ConnectionSpec::Memory;
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        let schema_path =
            std::env::temp_dir().join(format!("grove-repl-open-{}.gschema", std::process::id()));
        fs::write(&schema_path, "root items: Item; struct Item { id: Int }").expect("write schema");
        let db = std::env::current_dir().unwrap().join("Cargo.toml");
        let outcome = session
            .meta_command(&format!(".open {} {}", db.display(), schema_path.display()))
            .unwrap()
            .expect("open outcome");
        let _ = fs::remove_file(&schema_path);

        match outcome {
            ReplOutcome::ChangeBackend {
                spec: ConnectionSpec::Sqlite { .. },
                schema: Some((path, _)),
            } => assert_eq!(path, schema_path),
            other => panic!("expected backend change, got {other:?}"),
        }
    }

    #[test]
    fn close_command_switch_to_memory() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        let outcome = session
            .meta_command(".close")
            .unwrap()
            .expect("close outcome");
        match outcome {
            ReplOutcome::ChangeBackend {
                spec: ConnectionSpec::Memory,
                schema: None,
            } => {}
            other => panic!("expected memory backend, got {other:?}"),
        }
    }

    #[test]
    fn clear_bindings_types_and_values() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        session.execute_source("let x = 1;", "<test:1>").unwrap();
        session.meta_command(".clear").unwrap();
        assert!(session.types.is_empty());
        assert_eq!(session.exec.bindings().count(), 0);
    }

    #[test]
    fn unlet_remove_binding() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        session.execute_source("let x = 1;", "<test:1>").unwrap();
        session.execute_source("let y = 2;", "<test:2>").unwrap();
        session.meta_command(".unlet x").unwrap();
        assert!(session.types.get("x").is_none());
        assert!(session.exec.binding("x").is_none());
        assert!(session.types.get("y").is_some());
    }

    #[test]
    fn vars_empty_contexts() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        session.execute_source("let x = 1;", "<test:1>").unwrap();
        session.meta_command(".vars").unwrap();
        session.meta_command(".clear").unwrap();
        session.meta_command(".vars").unwrap();
    }

    #[test]
    fn read_execute_file_in_session() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let mut settings = settings();
        let spec = spec();
        let mut turn = 0;
        let mut session = session(&schema, &spec, &backend, &mut settings, &mut turn);

        let path = std::env::temp_dir().join(format!("grove-repl-read-{}.grv", std::process::id()));
        fs::write(&path, "let y = 2;").expect("write query file");
        session
            .meta_command(&format!(".read {}", path.display()))
            .unwrap();
        let _ = fs::remove_file(&path);
        assert!(session.types.get("y").is_some());
    }
}
