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
